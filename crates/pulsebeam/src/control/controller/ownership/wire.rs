//! Complete peer snapshots. Origin and freshness come from the authenticated
//! session, never from the decoded document.

use std::{
    collections::{BTreeSet, HashMap},
    num::NonZeroU64,
};

use pulsebeam_core::fabric::{MAJOR, Profile};
use pulsebeam_proto::{prelude::Message, state::Track};
use serde::{Deserialize, Serialize};

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
    entity::{
        ConnectionId, EndpointKey, ParticipantId, ProjectId, ProjectRoomKey, PublicationKey, RoomId,
    },
    media::Rid,
    route::fabric::{DataPublisher, local::Emission},
    shard_update::PublisherAnchor,
    track::{LayerQuality, SourceEncoding},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    revision: NonZeroU64,
    authority: Checkpoint,
    endpoints: Box<[EndpointImage]>,
    routes: Box<[relay::Offer]>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fabric_routes: Vec<relay::fabric::WireLiveRouteAck>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fabric_retired: Vec<[u8; 4]>,
}

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

pub(in crate::control) type DataAnchors = HashMap<PublicationKey, (DataPublisher, u64)>;
pub(in crate::control) type OpusAnchors = HashMap<PublicationKey, (PublisherAnchor, u64)>;

pub(in crate::control) struct Decoded {
    pub revision: NonZeroU64,
    pub authority: Checkpoint,
    pub endpoints: Snapshot,
    pub routes: relay::Offers,
    pub fabric_routes: Box<[relay::fabric::LiveRouteAck]>,
    pub wan: Option<WanOwnership>,
    pub fabric_retired: Vec<[u8; 4]>,
    pub data_anchors: DataAnchors,
    pub opus_anchors: OpusAnchors,
}

pub(in crate::control) struct WanOwnership {
    pub routes: Box<[relay::fabric::WanLiveRouteAck]>,
    pub sources: Box<[WanSourceWindow]>,
    pub retired: Vec<[u8; 4]>,
}

impl Decoded {
    pub(in crate::control) fn matches_carriage(&self, fabric: bool) -> bool {
        if self.wan.is_some() {
            return false;
        }
        if fabric {
            self.routes.is_empty()
        } else {
            self.fabric_routes.is_empty()
                && self.fabric_retired.is_empty()
                && self.opus_anchors.is_empty()
                && self.endpoints.iter().all(|(_, endpoint)| {
                    endpoint
                        .publications
                        .iter()
                        .all(|publication| publication.fabric_data_source.is_none())
                })
        }
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sequence_anchor: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    opus_source: Option<OpusSourceImage>,
    layers: Box<[LayerImage]>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpusSourceImage {
    incarnation: NonZeroU64,
    extensions: u16,
    sequence_anchor: u64,
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
    fn from_fact(fact: &PublicationFact, fabric: bool, sequence_anchor: Option<u64>) -> Self {
        Self {
            descriptor: fact.descriptor.encode_to_vec().into_boxed_slice(),
            fabric_data_source: if fabric {
                fact.fabric_data_source
            } else {
                None
            },
            sequence_anchor,
            opus_source: None,
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

pub(in crate::control) enum RouteImage {
    Legacy(Box<[relay::Offer]>),
    Wan {
        offers: Box<[relay::fabric::WireWanLiveRouteAck]>,
        sources: Box<[WanSourceWindow]>,
        retired: Vec<[u8; 4]>,
    },
    Fabric {
        offers: Vec<relay::fabric::WireLiveRouteAck>,
        retired: Vec<[u8; 4]>,
        data_anchors: DataAnchors,
        opus_anchors: Option<(u8, OpusAnchors)>,
    },
}

pub(in crate::control) fn encode(
    revision: NonZeroU64,
    authority: Checkpoint,
    snapshot: &Snapshot,
    routes: RouteImage,
) -> Result<Vec<u8>, Error> {
    validate(snapshot)?;
    let fabric = !matches!(routes, RouteImage::Legacy(_));
    let (routes, fabric_routes, fabric_retired, data_anchors, opus_anchors, wan_routes) =
        match routes {
            RouteImage::Legacy(offers) => (
                offers,
                Vec::new(),
                Vec::new(),
                DataAnchors::new(),
                None,
                None,
            ),
            RouteImage::Wan {
                offers,
                sources,
                retired,
            } => (
                Box::default(),
                Vec::new(),
                Vec::new(),
                DataAnchors::new(),
                None,
                Some((offers, sources, retired)),
            ),
            RouteImage::Fabric {
                offers,
                retired,
                data_anchors,
                opus_anchors,
            } => (
                Box::default(),
                offers,
                retired,
                data_anchors,
                opus_anchors,
                None,
            ),
        };
    if fabric_routes.len() > relay::LIMIT
        || wan_routes
            .as_ref()
            .is_some_and(|(offers, _, _)| offers.len() > relay::LIMIT)
        || data_anchors.len() > relay::LIMIT
        || opus_anchors
            .as_ref()
            .is_some_and(|(_, anchors)| anchors.len() > relay::LIMIT)
    {
        return Err(Error::Capacity);
    }
    validate_retirements(&fabric_retired)?;
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
                .map(|publication| {
                    let sequence_anchor = publication
                        .data_publisher(fact.claim.connection)
                        .and_then(|publisher| {
                            let &(sampled, anchor) = data_anchors.get(&publisher.publication)?;
                            (sampled == publisher).then_some(anchor)
                        });
                    let mut image =
                        PublicationImage::from_fact(publication, fabric, sequence_anchor);
                    image.opus_source = publication.opus_publication().and_then(|key| {
                        let (revision, anchors) = opus_anchors.as_ref()?;
                        let &(publisher, sequence_anchor) = anchors.get(&key)?;
                        if publisher.publication != key
                            || publisher.connection != fact.claim.connection
                            || publisher.emission.profile() != Profile::Opus
                            || !publisher.emission.fits_registry(MAJOR, *revision)
                        {
                            return None;
                        }
                        Some(OpusSourceImage {
                            incarnation: publisher.incarnation,
                            extensions: publisher.emission.wire_profile().1,
                            sequence_anchor,
                        })
                    });
                    image
                })
                .collect(),
        })
        .collect();
    let mut bytes = ImageWriter(Vec::new());
    let result = if let Some((wan_routes, sources, wan_retired)) = wan_routes {
        validate_wan_sources(&sources, snapshot)?;
        validate_retirements(&wan_retired)?;
        let wan_sources = sources.into_vec().into_iter().map(Into::into).collect();
        serde_json::to_writer(
            &mut bytes,
            &WanImage {
                revision,
                authority,
                endpoints,
                wan_routes,
                wan_sources,
                wan_retired,
            },
        )
    } else {
        serde_json::to_writer(
            &mut bytes,
            &Image {
                revision,
                authority,
                endpoints,
                routes,
                fabric_routes,
                fabric_retired,
            },
        )
    };
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

pub(in crate::control) fn decode(data: &[u8], opus_revision: Option<u8>) -> Result<Decoded, Error> {
    if data.len() > MAX_IMAGE_BYTES {
        return Err(Error::Capacity);
    }
    let image = serde_json::from_slice(data).map_err(|_| Error::Invalid)?;
    decode_image(image, opus_revision)
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
    let mut decoded = decode_image(
        Image {
            revision,
            authority,
            endpoints,
            routes: Box::default(),
            fabric_routes: Vec::new(),
            fabric_retired: Vec::new(),
        },
        None,
    )?;
    if !decoded.data_anchors.is_empty() || !decoded.opus_anchors.is_empty() {
        return Err(Error::Invalid);
    }
    validate_wan_sources(&sources, &decoded.endpoints)?;
    decoded.wan = Some(WanOwnership {
        routes,
        sources,
        retired: wan_retired,
    });
    Ok(decoded)
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

fn decode_image(image: Image, opus_revision: Option<u8>) -> Result<Decoded, Error> {
    if image.endpoints.len() > MAX_CLAIMS_PER_ORIGIN {
        return Err(Error::Capacity);
    }
    let mut data_anchors = DataAnchors::new();
    let mut opus_anchors = OpusAnchors::new();
    let snapshot = image
        .endpoints
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
                        .map(|mut image| {
                            let anchor = image.sequence_anchor;
                            let opus = image.opus_source.take();
                            let fact = PublicationFact::try_from(image)?;
                            if let Some(opus) = opus {
                                if opus_anchors.len() >= relay::LIMIT {
                                    return Err(Error::Capacity);
                                }
                                let publication = fact.opus_publication().ok_or(Error::Invalid)?;
                                let emission = Emission::from_wire_profile(
                                    Profile::Opus as u8,
                                    opus.extensions,
                                    MAJOR,
                                    opus_revision.ok_or(Error::Invalid)?,
                                )
                                .ok_or(Error::Invalid)?;
                                let publisher = PublisherAnchor {
                                    publication,
                                    connection: endpoint.connection,
                                    incarnation: opus.incarnation,
                                    emission,
                                };
                                if opus_anchors
                                    .insert(publication, (publisher, opus.sequence_anchor))
                                    .is_some()
                                {
                                    return Err(Error::Invalid);
                                }
                            }
                            if let Some(anchor) = anchor {
                                let publisher = fact
                                    .data_publisher(endpoint.connection)
                                    .ok_or(Error::Invalid)?;
                                if data_anchors.len() >= relay::LIMIT {
                                    return Err(Error::Capacity);
                                }
                                if data_anchors
                                    .insert(publisher.publication, (publisher, anchor))
                                    .is_some()
                                {
                                    return Err(Error::Invalid);
                                }
                            }
                            Ok(fact)
                        })
                        .collect::<Result<_, _>>()?,
                },
            ))
        })
        .collect::<Result<Snapshot, Error>>()?;
    validate(&snapshot)?;
    validate_retirements(&image.fabric_retired)?;
    Ok(Decoded {
        revision: image.revision,
        authority: image.authority,
        endpoints: snapshot,
        routes: relay::decode(image.routes).ok_or(Error::Invalid)?,
        fabric_routes: relay::fabric::decode(image.fabric_routes.into_boxed_slice())
            .ok_or(Error::Invalid)?,
        wan: None,
        fabric_retired: image.fabric_retired,
        data_anchors,
        opus_anchors,
    })
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
    use super::{fixture_checkpoint as authority, *};
    use pulsebeam_proto::state::{AudioTrack, CodecFormat, VideoTrack, track};

    fn decode(bytes: &[u8]) -> Result<Decoded, Error> {
        super::decode(bytes, None)
    }

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
    fn selected_wan_image_rejects_legacy_routes_and_duplicate_window_labels() {
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
            window: Window::new(Profile::H264, 1 << 33, 0, NonZeroU32::MIN, 0).unwrap(),
        };
        let encode_image =
            |routes| encode(NonZeroU64::MIN, authority(), &endpoints, routes).unwrap();
        let wan = encode_image(RouteImage::Wan {
            offers: Box::new([ack.into()]),
            sources: Box::default(),
            retired: vec![2u32.to_be_bytes()],
        });
        let decoded = decode_wan(&wan).unwrap();
        let ownership = decoded.wan.unwrap();
        assert_eq!(ownership.routes.as_ref(), &[ack]);
        assert_eq!(ownership.retired, [2u32.to_be_bytes()]);
        let mut invalid = serde_json::from_slice::<serde_json::Value>(&wan).unwrap();
        invalid["wan_retired"] = serde_json::json!([[0, 0, 0, 2], [0, 0, 0, 2]]);
        assert!(matches!(
            decode_wan(&serde_json::to_vec(&invalid).unwrap()),
            Err(Error::Invalid)
        ));
        assert_eq!(decoded.endpoints, endpoints);
        assert!(matches!(decode(&wan), Err(Error::Invalid)));
        let legacy = encode_image(RouteImage::Legacy(Box::default()));
        assert!(matches!(decode_wan(&legacy), Err(Error::Invalid)));
        let duplicate = encode_image(RouteImage::Wan {
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
    fn opus_anchor_requires_negotiated_emission_and_exact_publication() {
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
        let checkpoint = serde_json::to_vec(&authority()).unwrap();
        let encode_image = |opus_anchors| {
            encode(
                NonZeroU64::MIN,
                serde_json::from_slice(&checkpoint).unwrap(),
                &audio,
                RouteImage::Fabric {
                    offers: Vec::new(),
                    retired: Vec::new(),
                    data_anchors: DataAnchors::new(),
                    opus_anchors,
                },
            )
            .unwrap()
        };
        let anchors = |publisher| OpusAnchors::from([(key, (publisher, 65_536))]);
        let bytes = encode_image(Some((1, anchors(publisher))));
        let decoded = super::decode(&bytes, Some(1)).unwrap();
        assert_eq!(decoded.endpoints, audio);
        assert_eq!(decoded.opus_anchors, anchors(publisher));
        assert!(decoded.matches_carriage(true));
        assert!(!decoded.matches_carriage(false));
        assert!(matches!(decode(&bytes), Err(Error::Invalid)));
        assert!(matches!(
            super::decode(&bytes, Some(0)),
            Err(Error::Invalid)
        ));

        let no_capability = encode_image(None);
        assert_eq!(encode_image(Some((0, anchors(publisher)))), no_capability);
        let stale = PublisherAnchor {
            connection: ConnectionId::new(),
            ..publisher
        };
        assert_eq!(encode_image(Some((1, anchors(stale)))), no_capability);
        assert!(decode(&no_capability).unwrap().opus_anchors.is_empty());

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
            RouteImage::Wan {
                offers: Box::default(),
                sources: rollover.clone(),
                retired: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(decode_wan(&wan).unwrap().wan.unwrap().sources, rollover);

        use pulsebeam_clock::{
            domain::{Projection, Provenance, Quality},
            time::{Binding, DomainId, Generation, GlobalInstant, RtpTimestamp},
        };
        let timed = source.with_projection(Some(Projection {
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
            RouteImage::Wan {
                offers: Box::default(),
                sources: timed_windows.clone(),
                retired: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(
            decode_wan(&image).unwrap().wan.unwrap().sources,
            timed_windows
        );
        let advanced =
            WanSourceWindow::from_sample(source.room, publisher, source.sequence_anchor + 1);
        let updated = plan_windows(vec![advanced], &timed_windows, 3);
        assert_eq!(updated[0].sequence_anchor, source.sequence_anchor);
        assert!(updated[0].timing.is_none());

        assert!(matches!(
            encode(
                NonZeroU64::MIN,
                authority(),
                &audio,
                RouteImage::Wan {
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
        assert!(!source.matches_ack(relay::fabric::WanLiveRouteAck {
            sequence_anchor: 0x1_fffe,
            ..ack
        }));
        let mut unscoped: serde_json::Value = serde_json::from_slice(&wan).unwrap();
        unscoped["wan_sources"][0]["connection"] = serde_json::to_value(stale.connection).unwrap();
        assert!(matches!(
            decode_wan(&serde_json::to_vec(&unscoped).unwrap()),
            Err(Error::Invalid)
        ));

        let mut not_audio: Image = serde_json::from_slice(&bytes).unwrap();
        not_audio.endpoints[0].publications[0].descriptor = snapshot()[0].1.publications[0]
            .descriptor
            .encode_to_vec()
            .into_boxed_slice();
        assert!(matches!(
            super::decode(&serde_json::to_vec(&not_audio).unwrap(), Some(1)),
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
            RouteImage::Fabric {
                offers: Vec::new(),
                retired: vec![[1, 2, 0, 3]],
                data_anchors: DataAnchors::new(),
                opus_anchors: None,
            },
        )
        .unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.revision, NonZeroU64::MIN);
        assert_eq!(decoded.endpoints, snapshot);
        assert_eq!(decoded.fabric_retired, [[1, 2, 0, 3]]);
        assert!(matches!(
            decode(bytes.get(..bytes.len().saturating_sub(1)).unwrap()),
            Err(Error::Invalid)
        ));
    }

    #[test]
    fn fabric_data_source_is_scoped_to_a_data_publication_and_fabric_carriage() {
        use pulsebeam_proto::state::{DataDelivery, DataTrack};
        let mut snapshot = snapshot();
        let source = NonZeroU64::new(73);
        snapshot
            .first_mut()
            .unwrap()
            .1
            .publications
            .first_mut()
            .unwrap()
            .fabric_data_source = source;
        let fabric = |data_anchors| RouteImage::Fabric {
            offers: Vec::new(),
            retired: Vec::new(),
            data_anchors,
            opus_anchors: None,
        };
        assert_eq!(
            encode(
                NonZeroU64::MIN,
                authority(),
                &snapshot,
                fabric(DataAnchors::new())
            ),
            Err(Error::Invalid)
        );
        let fact = snapshot
            .first_mut()
            .unwrap()
            .1
            .publications
            .first_mut()
            .unwrap();
        fact.descriptor.kind = Some(track::Kind::Data(DataTrack {
            capture_clock: None,
            delivery: DataDelivery::Ordered as i32,
            record_dd: false,
        }));
        fact.layers = Box::new([]);
        let (_, owner) = snapshot.first().unwrap();
        let publisher = owner
            .publications
            .first()
            .unwrap()
            .data_publisher(owner.claim.connection)
            .unwrap();
        let sequence = u64::from(u32::MAX) + 7;
        let anchors = [(publisher.publication, (publisher, sequence))].into();
        let protected = encode(NonZeroU64::MIN, authority(), &snapshot, fabric(anchors)).unwrap();
        let decoded = decode(&protected).unwrap();
        assert_eq!(
            decoded.data_anchors.get(&publisher.publication),
            Some(&(publisher, sequence))
        );
        let mut invalid: Image = serde_json::from_slice(&protected).unwrap();
        invalid
            .endpoints
            .first_mut()
            .unwrap()
            .publications
            .first_mut()
            .unwrap()
            .fabric_data_source = None;
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid).unwrap()),
            Err(Error::Invalid)
        ));
        let stale = DataPublisher {
            connection: ConnectionId::new(),
            ..publisher
        };
        let stale_bytes = encode(
            NonZeroU64::MIN,
            authority(),
            &snapshot,
            fabric([(publisher.publication, (stale, sequence))].into()),
        )
        .unwrap();
        assert!(decode(&stale_bytes).unwrap().data_anchors.is_empty());
        assert!(decoded.matches_carriage(true));
        assert!(!decoded.matches_carriage(false));
        assert_eq!(decoded.endpoints, snapshot);
        let (endpoint, owner) = decoded.endpoints.first().unwrap();
        let advertised = owner
            .publications
            .first()
            .unwrap()
            .media_track(*endpoint, owner.claim.connection)
            .unwrap();
        assert_eq!(advertised.fabric_data_source(), source);

        let legacy = encode(
            NonZeroU64::MIN,
            authority(),
            &snapshot,
            RouteImage::Legacy(Box::new([])),
        )
        .unwrap();
        let decoded = decode(&legacy).unwrap();
        assert!(decoded.matches_carriage(false));
        assert!(decoded.data_anchors.is_empty());
        assert!(
            decoded
                .endpoints
                .first()
                .unwrap()
                .1
                .publications
                .first()
                .unwrap()
                .fabric_data_source
                .is_none()
        );
    }

    #[test]
    fn claims_without_an_authority_checkpoint_are_not_a_complete_image() {
        let data = encode(
            NonZeroU64::MIN,
            authority(),
            &snapshot(),
            RouteImage::Legacy(Box::new([])),
        )
        .unwrap();
        assert!(decode(&data).unwrap().fabric_retired.is_empty());
        let mut fields: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&data).unwrap();
        assert!(fields.remove("authority").is_some());
        assert!(matches!(
            decode(&serde_json::to_vec(&fields).unwrap()),
            Err(Error::Invalid)
        ));
    }

    #[test]
    fn fabric_retirement_labels_must_be_bounded_and_unambiguous() {
        let bytes = encode(
            NonZeroU64::MIN,
            authority(),
            &snapshot(),
            RouteImage::Legacy(Box::new([])),
        )
        .unwrap();
        let mut image: Image = serde_json::from_slice(&bytes).unwrap();
        for labels in [vec![[0; 4]], vec![[1; 4], [1; 4]], vec![[2; 4], [1; 4]]] {
            image.fabric_retired = labels;
            assert!(matches!(
                decode(&serde_json::to_vec(&image).unwrap()),
                Err(Error::Invalid)
            ));
        }
        image.fabric_retired = vec![[1; 4]; relay::LIMIT + 1];
        assert!(matches!(
            decode(&serde_json::to_vec(&image).unwrap()),
            Err(Error::Capacity)
        ));
    }

    #[test]
    fn invalid_scope_or_rank_cannot_enter_a_peer_snapshot() {
        let data = encode(
            NonZeroU64::MIN,
            authority(),
            &snapshot(),
            RouteImage::Legacy(Box::new([])),
        )
        .unwrap();
        let mut image: Image = serde_json::from_slice(&data).unwrap();
        image.endpoints.first_mut().unwrap().position = 0;
        assert!(matches!(
            decode(&serde_json::to_vec(&image).unwrap()),
            Err(Error::Invalid)
        ));
        let mut snapshot = snapshot();
        snapshot.first_mut().unwrap().0.participant = ParticipantId::new("wrong").unwrap();
        assert_eq!(
            encode(
                NonZeroU64::MIN,
                authority(),
                &snapshot,
                RouteImage::Legacy(Box::new([])),
            ),
            Err(Error::Invalid)
        );
    }
}
