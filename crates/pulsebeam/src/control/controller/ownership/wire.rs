//! Complete peer snapshots. Origin and freshness come from the authenticated
//! session, never from the decoded document.

use pulsebeam_proto::{prelude::Message, state::Track};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, num::NonZeroU64};

use super::{
    ClaimRank,
    observations::{EndpointFact, Error, PublicationFact, Snapshot, validate_publications},
    receipt::epoch::Checkpoint,
};
use crate::control::controller::peer_ownership::relay::wan_source::{
    WanSourceWindow, WireWanSourceWindow,
};
use crate::{
    control::controller::peer_ownership::{
        MAX_CLAIMS_PER_ORIGIN, MAX_PUBLICATIONS_PER_ORIGIN, relay, transfer::MAX_IMAGE_BYTES,
    },
    entity::{ConnectionId, EndpointKey, ParticipantId, ProjectId, ProjectRoomKey, RoomId},
    media::Rid,
    track::{LayerQuality, SourceEncoding},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WanImage {
    revision: NonZeroU64,
    authority: Checkpoint,
    endpoints: Box<[EndpointImage]>,
    wan_routes: Box<[relay::fabric::WireWanLiveRouteAck]>,
    wan_sources: Box<[WireWanSourceWindow]>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    wan_retired: Vec<[u8; 4]>,
}

pub(in crate::control) struct Decoded {
    pub revision: NonZeroU64,
    pub authority: Checkpoint,
    pub endpoints: Snapshot,
    pub wan: WanOwnership,
}

pub(in crate::control) struct WanOwnership {
    pub routes: Box<[relay::fabric::WanLiveRouteAck]>,
    pub sources: Box<[WanSourceWindow]>,
    pub retired: Vec<[u8; 4]>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointImage {
    project: ProjectId,
    room: RoomId,
    participant: ParticipantId,
    root: ConnectionId,
    position: u64,
    connection: ConnectionId,
    publications: Box<[PublicationImage]>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationImage {
    descriptor: Box<[u8]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fabric_data_source: Option<NonZeroU64>,
    layers: Box<[LayerImage]>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerImage {
    encoding: EncodingImage,
    rid: Option<String>,
    quality: QualityImage,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EncodingImage {
    Rid(Option<String>),
    Ssrc(u32),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QualityImage {
    Low,
    Medium,
    High,
}

impl PublicationImage {
    fn from_fact(fact: &PublicationFact) -> Self {
        Self {
            descriptor: fact.descriptor.encode_to_vec().into_boxed_slice(),
            fabric_data_source: fact.fabric_data_source,
            layers: fact
                .layers
                .iter()
                .map(|&(encoding, rid, quality)| LayerImage {
                    encoding: match encoding {
                        SourceEncoding::Rid(value) => {
                            EncodingImage::Rid(value.map(|rid| rid.to_string()))
                        }
                        SourceEncoding::Ssrc(value) => EncodingImage::Ssrc(value.into()),
                    },
                    rid: rid.map(|rid| rid.to_string()),
                    quality: match quality {
                        LayerQuality::Low => QualityImage::Low,
                        LayerQuality::Medium => QualityImage::Medium,
                        LayerQuality::High => QualityImage::High,
                    },
                })
                .collect(),
        }
    }
}

impl TryFrom<PublicationImage> for PublicationFact {
    type Error = Error;

    fn try_from(image: PublicationImage) -> Result<Self, Error> {
        if image.layers.len() > 3 {
            return Err(Error::Invalid);
        }
        let mut encodings = Vec::with_capacity(image.layers.len());
        let mut layers = Vec::with_capacity(image.layers.len());
        for layer in image.layers {
            let rid = parse_rid(layer.rid.as_deref())?;
            let encoding = match layer.encoding {
                EncodingImage::Rid(value) if value.as_deref() == layer.rid.as_deref() => {
                    SourceEncoding::Rid(rid)
                }
                EncodingImage::Ssrc(value) if value != 0 => SourceEncoding::Ssrc(value.into()),
                _ => return Err(Error::Invalid),
            };
            if encodings.contains(&encoding) {
                return Err(Error::Invalid);
            }
            encodings.push(encoding);
            let quality = match layer.quality {
                QualityImage::Low => LayerQuality::Low,
                QualityImage::Medium => LayerQuality::Medium,
                QualityImage::High => LayerQuality::High,
            };
            layers.push((encoding, rid, quality));
        }
        Ok(Self {
            descriptor: Track::decode(image.descriptor.as_ref()).map_err(|_| Error::Invalid)?,
            fabric_data_source: image.fabric_data_source,
            layers: layers.into_boxed_slice(),
        })
    }
}

fn parse_rid(input: Option<&str>) -> Result<Option<Rid>, Error> {
    input.map(Rid::new).transpose().map_err(|_| Error::Invalid)
}

#[derive(Default)]
pub(in crate::control) struct WanRoutes {
    pub offers: Box<[relay::fabric::WireWanLiveRouteAck]>,
    pub sources: Box<[WanSourceWindow]>,
    pub retired: Vec<[u8; 4]>,
}

pub(in crate::control) fn encode_wan(
    revision: NonZeroU64,
    authority: Checkpoint,
    snapshot: &Snapshot,
    routes: WanRoutes,
) -> Result<Vec<u8>, Error> {
    validate(snapshot)?;
    if routes.offers.len() > relay::LIMIT {
        return Err(Error::Capacity);
    }
    validate_wan_sources(&routes.sources, snapshot)?;
    validate_retirements(&routes.retired)?;
    let endpoints = snapshot
        .iter()
        .map(|(key, fact)| EndpointImage {
            project: key.room.project,
            room: key.room.room,
            participant: key.participant,
            root: fact.claim.root,
            position: fact.claim.position,
            connection: fact.claim.connection,
            publications: fact
                .publications
                .iter()
                .map(PublicationImage::from_fact)
                .collect(),
        })
        .collect();
    let mut bytes = ImageWriter(Vec::new());
    let result = serde_json::to_writer(
        &mut bytes,
        &WanImage {
            revision,
            authority,
            endpoints,
            wan_routes: routes.offers,
            wan_sources: routes
                .sources
                .into_vec()
                .into_iter()
                .map(Into::into)
                .collect(),
            wan_retired: routes.retired,
        },
    );
    result.map_err(|error| {
        if error.is_io() {
            Error::Capacity
        } else {
            Error::Invalid
        }
    })?;
    Ok(bytes.0)
}

struct ImageWriter(Vec<u8>);

impl std::io::Write for ImageWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_IMAGE_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("peer image capacity"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(in crate::control) fn decode_wan(data: &[u8]) -> Result<Decoded, Error> {
    if data.len() > MAX_IMAGE_BYTES {
        return Err(Error::Capacity);
    }
    let WanImage {
        revision,
        authority,
        endpoints,
        wan_routes,
        wan_sources,
        wan_retired,
    } = serde_json::from_slice(data).map_err(|_| Error::Invalid)?;
    validate_retirements(&wan_retired)?;
    if wan_sources.len() > relay::LIMIT {
        return Err(Error::Capacity);
    }
    let routes = relay::fabric::decode_wan(wan_routes).ok_or(Error::Invalid)?;
    let sources = wan_sources
        .into_vec()
        .into_iter()
        .map(WireWanSourceWindow::decode)
        .collect::<Option<Box<[_]>>>()
        .ok_or(Error::Invalid)?;
    let endpoints = decode_endpoints(endpoints)?;
    validate_wan_sources(&sources, &endpoints)?;
    Ok(Decoded {
        revision,
        authority,
        endpoints,
        wan: WanOwnership {
            routes,
            sources,
            retired: wan_retired,
        },
    })
}

fn validate_wan_sources(sources: &[WanSourceWindow], snapshot: &Snapshot) -> Result<(), Error> {
    if sources.len() > relay::LIMIT {
        return Err(Error::Capacity);
    }
    use pulsebeam_core::fabric::wan::WINDOW_SPAN;
    use relay::wan_source::WINDOWS_PER_SOURCE;
    use std::collections::BTreeMap;
    let mut scopes: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for &source in sources {
        let windows = scopes.entry(source.scope()).or_default();
        if windows.len() >= WINDOWS_PER_SOURCE
            || pulsebeam_core::fabric::wan::Window::start_for(source.window_start)
                != source.window_start
            || !source.present_in(snapshot)
        {
            return Err(Error::Invalid);
        }
        windows.push(source);
    }
    for windows in scopes.values_mut() {
        windows.sort_unstable_by_key(|source| source.window_start);
        if windows.windows(2).any(|pair| matches!(pair, [left, right]
            if left.publisher != right.publisher || left.window_start.checked_add(WINDOW_SPAN) != Some(right.window_start)))
        {
            return Err(Error::Invalid);
        }
    }
    Ok(())
}

fn decode_endpoints(endpoints: Box<[EndpointImage]>) -> Result<Snapshot, Error> {
    if endpoints.len() > MAX_CLAIMS_PER_ORIGIN {
        return Err(Error::Capacity);
    }
    let snapshot = endpoints
        .into_vec()
        .into_iter()
        .map(|endpoint| {
            if endpoint.publications.len() > MAX_PUBLICATIONS_PER_ORIGIN {
                return Err(Error::Capacity);
            }
            if (endpoint.position == 0) != (endpoint.root == endpoint.connection) {
                return Err(Error::Invalid);
            }
            Ok((
                EndpointKey {
                    room: ProjectRoomKey {
                        project: endpoint.project,
                        room: endpoint.room,
                    },
                    participant: endpoint.participant,
                },
                EndpointFact {
                    claim: ClaimRank {
                        root: endpoint.root,
                        position: endpoint.position,
                        connection: endpoint.connection,
                    },
                    publications: endpoint
                        .publications
                        .into_vec()
                        .into_iter()
                        .map(PublicationFact::try_from)
                        .collect::<Result<_, _>>()?,
                },
            ))
        })
        .collect::<Result<Snapshot, Error>>()?;
    validate(&snapshot)?;
    Ok(snapshot)
}

fn validate_retirements(labels: &[[u8; 4]]) -> Result<(), Error> {
    if labels.len() > relay::LIMIT {
        return Err(Error::Capacity);
    }
    if labels.contains(&[0; 4])
        || !labels
            .windows(2)
            .all(|pair| matches!(pair, [a, b] if a < b))
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

fn validate(snapshot: &Snapshot) -> Result<(), Error> {
    if snapshot.len() > MAX_CLAIMS_PER_ORIGIN {
        return Err(Error::Capacity);
    }
    let mut endpoints = BTreeSet::new();
    for (key, _) in snapshot {
        if !endpoints.insert(*key) {
            return Err(Error::Invalid);
        }
    }
    validate_publications(
        snapshot.iter().map(|(key, fact)| (*key, fact)),
        MAX_PUBLICATIONS_PER_ORIGIN,
    )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "the test checkpoint uses fixed valid keys"
)]
pub(in crate::control) fn fixture_checkpoint() -> Checkpoint {
    use super::receipt::{Issuer, epoch::Trust};
    use pulsebeam_core::{auth::ApiSigningKey, identity::NodeId};
    let issuer = Issuer {
        trust_domain: [1; 16],
        node: NodeId::new(),
        incarnation: NonZeroU64::MIN,
    };
    let remote = Issuer {
        node: NodeId::new(),
        ..issuer
    };
    Trust::new(
        issuer,
        ApiSigningKey::from_seed([1; 32]),
        remote,
        ApiSigningKey::from_seed([2; 32]).public_key(),
    )
    .unwrap()
    .checkpoint()
}

#[cfg(test)]
mod tests {
    use super::{encode_wan as encode, fixture_checkpoint as authority, *};
    use crate::{
        entity::PublicationKey, route::fabric::local::Emission, shard_update::PublisherAnchor,
    };
    use pulsebeam_core::fabric::Profile;
    use pulsebeam_proto::state::{AudioTrack, CodecFormat, VideoTrack, track};

    fn snapshot() -> Snapshot {
        let participant = ParticipantId::new("sender").unwrap();
        let room = RoomId::new("stage").unwrap();
        let media = Rid::new("f").unwrap();
        Box::new([(
            EndpointKey {
                room: ProjectRoomKey {
                    project: ProjectId::new(),
                    room,
                },
                participant,
            },
            EndpointFact {
                claim: ClaimRank::root(ConnectionId::new())
                    .successor(ConnectionId::new())
                    .unwrap(),
                publications: Box::new([PublicationFact {
                    descriptor: Track {
                        room_id: room.to_string(),
                        participant_id: participant.to_string(),
                        publication_id: crate::entity::PublicationId::mint().to_string(),
                        track_id: "camera".into(),
                        kind: Some(track::Kind::Video(VideoTrack {
                            av1_whole_source: false,
                            format: Some(CodecFormat {
                                mime_type: "video/vp8".into(),
                                clock_rate: 90_000,
                                channels: None,
                                parameters: Vec::new(),
                            }),
                        })),
                    },
                    fabric_data_source: None,
                    layers: Box::new([
                        (
                            SourceEncoding::Rid(Some(media)),
                            Some(media),
                            LayerQuality::High,
                        ),
                        (SourceEncoding::Ssrc(12.into()), None, LayerQuality::Low),
                    ]),
                }]),
            },
        )])
    }

    #[test]
    fn wan_image_rejects_retired_fields_and_duplicate_window_labels() {
        use pulsebeam_core::fabric::wan::Window;
        use relay::fabric::WanLiveRouteAck;
        use std::num::NonZeroU32;
        let endpoints = snapshot();
        let ack = WanLiveRouteAck {
            room: endpoints[0].0.room,
            publication: ConnectionId::from_bytes([7; 16])
                .fixture_publication(crate::entity::TrackKind::Video, "camera"),
            incarnation: NonZeroU64::MIN,
            sequence_anchor: 37,
            offer_cut: Some(NonZeroU64::MIN),
            window: Window::new(Profile::H264, 1 << 33, 0, NonZeroU32::MIN, 0).unwrap(),
        };
        let encode_image =
            |routes| encode(NonZeroU64::MIN, authority(), &endpoints, routes).unwrap();
        let wan = encode_image(WanRoutes {
            offers: Box::new([ack.into()]),
            sources: Box::default(),
            retired: vec![2u32.to_be_bytes()],
        });
        let decoded = decode_wan(&wan).unwrap();
        let ownership = decoded.wan;
        assert_eq!(ownership.routes.as_ref(), &[ack]);
        assert_eq!(ownership.retired, [2u32.to_be_bytes()]);
        let mut invalid = serde_json::from_slice::<serde_json::Value>(&wan).unwrap();
        invalid["wan_retired"] = serde_json::json!([[0, 0, 0, 2], [0, 0, 0, 2]]);
        assert!(matches!(
            decode_wan(&serde_json::to_vec(&invalid).unwrap()),
            Err(Error::Invalid)
        ));
        assert_eq!(decoded.endpoints, endpoints);
        let duplicate = encode_image(WanRoutes {
            offers: Box::new([ack.into(), ack.into()]),
            sources: Box::default(),
            retired: Vec::new(),
        });
        assert!(matches!(decode_wan(&duplicate), Err(Error::Invalid)));
        let mut mixed = serde_json::from_slice::<serde_json::Value>(&wan).unwrap();
        mixed["routes"] = serde_json::json!([]);
        assert!(matches!(
            decode_wan(&serde_json::to_vec(&mixed).unwrap()),
            Err(Error::Invalid)
        ));
    }

    #[test]
    fn wan_video_window_requires_exact_codec_and_single_stream() {
        let mut endpoints = snapshot();
        let room = endpoints[0].0.room;
        let connection = endpoints[0].1.claim.connection();
        let fact = &mut endpoints[0].1.publications[0];
        let Some(track::Kind::Video(video)) = fact.descriptor.kind.as_mut() else {
            panic!("expected video");
        };
        video.format.as_mut().unwrap().mime_type = "video/h264".into();
        let publication = PublicationKey::Video(fact.descriptor.publication_id.parse().unwrap());
        let source = |profile, publication| {
            WanSourceWindow::from_sample(
                room,
                PublisherAnchor {
                    publication,
                    connection,
                    incarnation: NonZeroU64::MIN,
                    emission: Emission::new(profile, &[]).unwrap(),
                },
                65_536,
            )
        };
        let encode = |endpoints: &Snapshot, source| {
            encode(
                NonZeroU64::MIN,
                authority(),
                endpoints,
                WanRoutes {
                    offers: Box::default(),
                    sources: Box::new([source]),
                    retired: Vec::new(),
                },
            )
        };
        let h264 = source(Profile::H264, publication);
        assert!(matches!(encode(&endpoints, h264), Err(Error::Invalid)));
        endpoints[0].1.publications[0].layers =
            Box::new([(SourceEncoding::Ssrc(12.into()), None, LayerQuality::High)]);
        let image = encode(&endpoints, h264).unwrap();
        assert_eq!(decode_wan(&image).unwrap().wan.sources.as_ref(), &[h264]);
        assert!(matches!(
            encode(&endpoints, source(Profile::Av1, publication)),
            Err(Error::Invalid)
        ));
        let unknown =
            ConnectionId::new().fixture_publication(crate::entity::TrackKind::Video, "camera");
        assert!(matches!(
            encode(&endpoints, source(Profile::H264, unknown)),
            Err(Error::Invalid)
        ));

        let bare_vp9 = source(Profile::Vp9, publication);
        let mut vp9 = bare_vp9;
        vp9.publisher.emission = Emission::new(
            Profile::Vp9,
            &[pulsebeam_core::fabric::ExtensionKind::DependencyDescriptor],
        )
        .unwrap();
        assert!(matches!(encode(&endpoints, vp9), Err(Error::Invalid)));
        let fact = &mut endpoints[0].1.publications[0];
        let Some(track::Kind::Video(video)) = fact.descriptor.kind.as_mut() else {
            panic!("expected video");
        };
        video.format = CodecFormat::native_quic_baseline("video/vp9");
        assert!(matches!(encode(&endpoints, bare_vp9), Err(Error::Invalid)));
        let ladder = crate::rtp::monitor::MeasuredTargetLadder {
            context: crate::route::fabric::local::DecodeTargetRateContext {
                structure_origin: vp9.sequence_anchor,
                active_targets: 0b101,
            },
            through: vp9.sequence_anchor + 1,
            heights: [180, 0, 720],
            payload_bps: [100_000, 0, 1_000_000],
            fps: [30, 0, 30],
        };
        assert!(h264.with_ladder(Some(ladder)).ladder.is_none());
        assert!(
            vp9.with_ladder(Some(crate::rtp::monitor::MeasuredTargetLadder {
                heights: [180, 360, 720],
                ..ladder
            }))
            .ladder
            .is_none()
        );
        let offered = vp9.with_ladder(Some(ladder));
        assert!(offered.same_window(vp9));
        assert_eq!(offered.ladder, Some(ladder));
        let planned = crate::control::controller::peer_ownership::relay::wan_source::plan_windows(
            vec![offered],
            &[vp9],
            2,
        );
        assert_eq!(planned[0], offered);
        assert!(planned[1].ladder.is_none());
        let withdrawn = crate::control::controller::peer_ownership::relay::wan_source::plan_windows(
            vec![vp9],
            &planned,
            2,
        );
        assert_eq!(withdrawn[0], vp9);
        let vp9 = offered;
        let decoded = decode_wan(&encode(&endpoints, vp9).unwrap()).unwrap();
        let sources = decoded.wan.sources;
        assert_eq!(sources.as_ref(), &[vp9]);
        let (endpoint, observed) = &decoded.endpoints[0];
        let routed = PublicationFact::routed_track(
            *endpoint,
            sources[0].publisher,
            &observed.publications[0].descriptor,
        )
        .unwrap();
        assert!(routed.as_video().unwrap().guarantees_video_dd);
    }

    #[test]
    fn wan_opus_windows_preserve_exact_publication_and_sender_timing() {
        let mut audio = snapshot();
        let fact = &mut audio[0].1;
        let publication = &mut fact.publications[0];
        publication.descriptor.kind = Some(track::Kind::Audio(AudioTrack {
            format: Some(CodecFormat {
                mime_type: "audio/opus".into(),
                clock_rate: 48_000,
                channels: Some(2),
                parameters: Vec::new(),
            }),
        }));
        publication.layers = Box::default();
        let key = publication.opus_publication().unwrap();
        let publisher = PublisherAnchor {
            publication: key,
            connection: fact.claim.connection(),
            incarnation: NonZeroU64::new(7).unwrap(),
            emission: Emission::new(
                Profile::Opus,
                &[pulsebeam_core::fabric::ExtensionKind::AudioLevel],
            )
            .unwrap(),
        };
        use pulsebeam_core::fabric::wan::WINDOW_SPAN;
        use relay::wan_source::plan_windows;
        let source = WanSourceWindow::from_sample(audio[0].0.room, publisher, (1 << 33) + 0x1_fffe);
        let prepared = plan_windows(vec![source], &[], 3);
        assert_eq!(prepared.len(), 2);
        assert_eq!(prepared[1].window_start, source.window_start + WINDOW_SPAN);
        let crossed =
            WanSourceWindow::from_sample(source.room, publisher, prepared[1].window_start + 17);
        let rollover = plan_windows(vec![crossed], &prepared, 3);
        assert_eq!(&rollover[..2], prepared.as_ref());
        assert_eq!(
            rollover[2].window_start,
            prepared[1].window_start + WINDOW_SPAN
        );
        assert_eq!(plan_windows(vec![crossed], &prepared, 2), prepared);
        let wan = encode(
            NonZeroU64::MIN,
            authority(),
            &audio,
            WanRoutes {
                offers: Box::default(),
                sources: rollover.clone(),
                retired: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(decode_wan(&wan).unwrap().wan.sources, rollover);

        use pulsebeam_clock::{
            domain::{Projection, Provenance, Quality},
            time::{Binding, DomainId, Generation, GlobalInstant, RtpTimestamp},
        };
        let origin = crate::cluster::mesh::Member {
            node: pulsebeam_core::identity::NodeId::new(),
            generation: NonZeroU64::MIN,
        };
        let timed = WanSourceWindow {
            origin: Some(origin),
            ..source
        }
        .with_projection(Some(Projection {
            global: GlobalInstant::from_nanos(100_000_000),
            rtp: RtpTimestamp(4_800),
            binding: Binding {
                domain: DomainId(publisher.incarnation.get()),
                generation: Generation(3),
            },
            provenance: Provenance::SenderReport,
            source: None,
            absolute_uncertainty_ns: None,
            quality: Quality::Degraded,
            calibration: None,
        }));
        assert!(timed.timing.is_some());
        assert_ne!(timed, source);
        assert!(timed.same_window(source));
        assert!(!timed.same_window(WanSourceWindow::from_sample(
            source.room,
            PublisherAnchor {
                incarnation: NonZeroU64::MAX,
                ..publisher
            },
            source.sequence_anchor,
        )));
        let timed_windows = plan_windows(vec![timed], &[], 3);
        assert_eq!(timed_windows[0], timed);
        assert!(timed_windows[1].timing.is_none());
        let image = encode(
            NonZeroU64::MIN,
            authority(),
            &audio,
            WanRoutes {
                offers: Box::default(),
                sources: timed_windows.clone(),
                retired: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(decode_wan(&image).unwrap().wan.sources, timed_windows);
        let advanced = WanSourceWindow {
            origin: Some(origin),
            ..WanSourceWindow::from_sample(source.room, publisher, source.sequence_anchor + 1)
        };
        let updated = plan_windows(vec![advanced], &timed_windows, 3);
        assert_eq!(updated[0].sequence_anchor, source.sequence_anchor);
        assert!(updated[0].timing.is_none());

        assert!(matches!(
            encode(
                NonZeroU64::MIN,
                authority(),
                &audio,
                WanRoutes {
                    offers: Box::default(),
                    sources: Box::new([source, rollover[2]]),
                    retired: Vec::new(),
                }
            ),
            Err(Error::Invalid)
        ));
        let ack = relay::fabric::WanLiveRouteAck {
            room: source.room,
            publication: publisher.publication,
            incarnation: publisher.incarnation,
            sequence_anchor: source.sequence_anchor,
            offer_cut: None,
            window: pulsebeam_core::fabric::wan::Window::new(
                Profile::Opus,
                source.window_start,
                source.forms(),
                std::num::NonZeroU32::MIN,
                publisher.emission.wire_profile().1,
            )
            .unwrap(),
        };
        assert!(source.matches_ack(ack));
        let cut = WanSourceWindow {
            offer_cut: Some(NonZeroU64::MIN),
            ..source
        };
        assert!(!cut.matches_ack(ack));
        assert!(cut.matches_ack(relay::fabric::WanLiveRouteAck {
            offer_cut: cut.offer_cut,
            ..ack
        }));
        assert!(!source.matches_ack(relay::fabric::WanLiveRouteAck {
            sequence_anchor: 0x1_fffe,
            ..ack
        }));
        let mut unscoped: serde_json::Value = serde_json::from_slice(&wan).unwrap();
        unscoped["wan_sources"][0]["connection"] =
            serde_json::to_value(ConnectionId::new()).unwrap();
        assert!(matches!(
            decode_wan(&serde_json::to_vec(&unscoped).unwrap()),
            Err(Error::Invalid)
        ));
    }

    #[test]
    fn complete_snapshot_round_trips_with_exact_publication_and_encoding_identity() {
        let snapshot = snapshot();
        let bytes = encode(
            NonZeroU64::MIN,
            authority(),
            &snapshot,
            WanRoutes {
                retired: vec![[1, 2, 0, 3]],
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = decode_wan(&bytes).unwrap();
        assert_eq!(decoded.revision, NonZeroU64::MIN);
        assert_eq!(decoded.endpoints, snapshot);
        assert_eq!(decoded.wan.retired, [[1, 2, 0, 3]]);
        assert!(matches!(
            decode_wan(bytes.get(..bytes.len().saturating_sub(1)).unwrap()),
            Err(Error::Invalid)
        ));
    }

    #[test]
    fn wan_data_window_requires_the_exact_source_incarnation() {
        use pulsebeam_proto::state::{DataDelivery, DataTrack};
        let mut snapshot = snapshot();
        let source = NonZeroU64::new(73);
        snapshot[0].1.publications[0].fabric_data_source = source;
        assert_eq!(
            encode(
                NonZeroU64::MIN,
                authority(),
                &snapshot,
                WanRoutes::default()
            ),
            Err(Error::Invalid)
        );
        let fact = &mut snapshot[0].1.publications[0];
        fact.descriptor.kind = Some(track::Kind::Data(DataTrack {
            capture_clock: None,
            delivery: DataDelivery::Ordered as i32,
            record_dd: false,
        }));
        fact.layers = Box::new([]);
        let (endpoint, owner) = &snapshot[0];
        let publisher = owner.publications[0]
            .data_publisher(owner.claim.connection())
            .unwrap();
        let window =
            WanSourceWindow::from_sample(endpoint.room, publisher.into(), u64::from(u32::MAX) + 7);
        let bytes = encode(
            NonZeroU64::MIN,
            authority(),
            &snapshot,
            WanRoutes {
                sources: Box::new([window]),
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = decode_wan(&bytes).unwrap();
        assert_eq!(decoded.wan.sources.as_ref(), &[window]);
        assert_eq!(decoded.endpoints, snapshot);
        let (endpoint, owner) = &decoded.endpoints[0];
        let advertised = owner.publications[0]
            .media_track(*endpoint, owner.claim.connection())
            .unwrap();
        assert_eq!(advertised.fabric_data_source(), source);
        let mut invalid: WanImage = serde_json::from_slice(&bytes).unwrap();
        invalid.endpoints[0].publications[0].fabric_data_source = None;
        assert!(matches!(
            decode_wan(&serde_json::to_vec(&invalid).unwrap()),
            Err(Error::Invalid)
        ));
    }

    #[test]
    fn claims_without_an_authority_checkpoint_are_not_a_complete_image() {
        let data = encode(
            NonZeroU64::MIN,
            authority(),
            &snapshot(),
            WanRoutes::default(),
        )
        .unwrap();
        assert!(decode_wan(&data).unwrap().wan.retired.is_empty());
        let mut fields: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&data).unwrap();
        assert!(fields.remove("authority").is_some());
        assert!(matches!(
            decode_wan(&serde_json::to_vec(&fields).unwrap()),
            Err(Error::Invalid)
        ));
    }

    #[test]
    fn fabric_retirement_labels_must_be_bounded_and_unambiguous() {
        let bytes = encode(
            NonZeroU64::MIN,
            authority(),
            &snapshot(),
            WanRoutes::default(),
        )
        .unwrap();
        let mut image: WanImage = serde_json::from_slice(&bytes).unwrap();
        for labels in [vec![[0; 4]], vec![[1; 4], [1; 4]], vec![[2; 4], [1; 4]]] {
            image.wan_retired = labels;
            assert!(matches!(
                decode_wan(&serde_json::to_vec(&image).unwrap()),
                Err(Error::Invalid)
            ));
        }
        image.wan_retired = vec![[1; 4]; relay::LIMIT + 1];
        assert!(matches!(
            decode_wan(&serde_json::to_vec(&image).unwrap()),
            Err(Error::Capacity)
        ));
    }

    #[test]
    fn invalid_scope_or_rank_cannot_enter_a_peer_snapshot() {
        let data = encode(
            NonZeroU64::MIN,
            authority(),
            &snapshot(),
            WanRoutes::default(),
        )
        .unwrap();
        let mut image: WanImage = serde_json::from_slice(&data).unwrap();
        image.endpoints.first_mut().unwrap().position = 0;
        assert!(matches!(
            decode_wan(&serde_json::to_vec(&image).unwrap()),
            Err(Error::Invalid)
        ));
        let mut snapshot = snapshot();
        snapshot.first_mut().unwrap().0.participant = ParticipantId::new("wrong").unwrap();
        assert_eq!(
            encode(
                NonZeroU64::MIN,
                authority(),
                &snapshot,
                WanRoutes::default(),
            ),
            Err(Error::Invalid)
        );
    }
}
