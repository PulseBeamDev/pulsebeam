use super::*;
use crate::media::{Rid, SimulcastLayer};
use crate::rtp::cache::TrackStreamCache;
use crate::rtp::test_utils::H264StreamBuilder;

fn receiver_spec(track: TrackId, mid: &str, fixed: bool) -> wire::ResourceSpec {
    let format = capabilities()
        .codecs
        .into_iter()
        .find_map(|codec| {
            codec.format.filter(|format| {
                matches!(
                    format.known_codec(),
                    Some(wire::codec_format::Codec::H264(_))
                )
            })
        })
        .unwrap();
    wire::ResourceSpec {
        declaration: Some(resource_spec::Declaration::Subscription(
            wire::SubscriptionSpec {
                track_id: track.as_str(),
                kind: wire::TrackKind::Video as i32,
                binding: None,
                rtp_offer: Some(wire::RtpReceiveOffer {
                    mid: mid.into(),
                    codecs: vec![wire::RtpCodecParameters {
                        payload_type: Some(96),
                        format: Some(format),
                        feedback: vec![wire::RtcpFeedback::TransportCc.as_str().into()],
                        ..Default::default()
                    }],
                    rtcp_modes: vec![wire::RtcpMode::Compound.into()],
                    header_extensions: [
                        wire::RtpHeaderExtensionType::RtpHeaderExtensionMid,
                        wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01,
                        wire::RtpHeaderExtensionType::RtpHeaderExtensionPlayoutDelay,
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(i, kind)| wire::RtpHeaderExtensionParameters {
                        id: u32::try_from(i.checked_add(1).unwrap()).unwrap(),
                        uri: kind.as_str().into(),
                    })
                    .collect(),
                }),
            },
        )),
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: Some(wire::resource_settings::Receive::Video(
                wire::VideoReceiveOptions {
                    min_height: Some(1),
                    min_fps: Some(1),
                    playout_delay: fixed.then_some(wire::PlayoutDelay {
                        min_ms: 100,
                        max_ms: 100,
                    }),
                    ..Default::default()
                },
            )),
        }),
    }
}

fn feed(
    participant: &mut Participant,
    handle: TrackHandle,
    tx: &mut track::UpstreamTrack,
    cache: &mut TrackStreamCache,
    frames: &mut H264StreamBuilder,
    height: Option<u32>,
) {
    use pulsebeam_core::vla::{
        ResolutionAndFramerate, SimulcastStreamAllocation, SpatialLayerAllocation,
        TemporalLayerAllocation, VideoLayersAllocation,
    };
    let rid = Rid::from("q");
    let mut last_arrival = Instant::now();
    for (index, mut raw) in frames.keyframe(2).into_iter().enumerate() {
        raw.ext_vals.rid = Some(rid);
        if index == 0
            && let Some(height) = height
        {
            raw.ext_vals
                .set_video_layers_allocation(VideoLayersAllocation {
                    current_simulcast_stream_index: 0,
                    simulcast_streams: vec![SimulcastStreamAllocation {
                        spatial_layers: vec![SpatialLayerAllocation {
                            temporal_layers: vec![TemporalLayerAllocation {
                                cumulative_kbps: 50,
                            }],
                            resolution_and_framerate: Some(ResolutionAndFramerate {
                                width: height.saturating_mul(2),
                                height,
                                framerate: 30,
                            }),
                        }],
                    }],
                });
        }
        last_arrival = raw.arrival_ts;
        let processed = tx.process(Some(&rid), raw, None);
        for packet in processed.first.into_iter().chain(processed.remaining) {
            assert!(cache.push(packet.clone()).is_none());
            participant.on_track_packet(handle, TrackPacketRef::Rtp(&packet), Some(cache));
        }
    }
    tx.poll_stats(last_arrival);
}

fn outputs(participant: &mut Participant, id: TrackId) -> std::collections::BTreeSet<Mid> {
    let mut mids = std::collections::BTreeSet::new();
    while let Some(write) = participant.stream_writer.pop() {
        let StreamWrite::Video {
            track_id, mid, pt, ..
        } = write
        else {
            panic!("video output")
        };
        assert_eq!(track_id, id);
        assert_eq!(pt, 96u8.into());
        mids.insert(mid);
    }
    mids
}

pub(super) fn check(
    participant: &mut Participant,
    sink: &mut MockParticipantSink,
    handles: &mut slotmap::SlotMap<TrackHandle, TrackId>,
    audio: &Track,
) {
    let mut meta = audio.meta().clone();
    meta.id = meta.connection_id.derive_publication_track_id(
        TrackKind::Video,
        "camera",
        NonZeroU64::new(2).unwrap(),
    );
    meta.label = Some("camera".into());
    let id = meta.id;
    participant
        .downstream
        .update_bitrate(Instant::now(), crate::bitrate::Bitrate::from(2_000_000));
    let (mut tx, mut video) =
        track::new_video(Mid::from("camera"), meta, vec![SimulcastLayer::new("q")]);
    let first = receiver_spec(id, "video-a", true);
    let Some(resource_spec::Declaration::Subscription(subscription)) = &first.declaration else {
        unreachable!()
    };
    let Track::Video(source) = &mut video else {
        unreachable!()
    };
    source.format = subscription.rtp_offer.as_ref().unwrap().codecs[0]
        .format
        .clone();
    let handle = handles.insert(id);
    participant.add_track_candidate(handle, video);
    let mut identities = Vec::new();
    for (reference, spec) in [(5, first.clone()), (7, receiver_spec(id, "video-b", false))] {
        pulsebeam_proto::state::validation::validate_spec(&spec).unwrap();
        desire(participant, sink, reference, reference, Some(spec));
        let live = last_live_for(participant, reference).1;
        assert_eq!(live.execution, wire::ExecutionStatus::Preparing as i32);
        let plan = live.binding.unwrap();
        identities.push(plan_ssrc(&plan));
        installed(participant, sink, reference, plan);
        assert_eq!(
            last_live_for(participant, reference).1.execution,
            wire::ExecutionStatus::WaitingForSource as i32
        );
    }
    participant.native_source_binding(id, handle, true);
    for _ in 0..16 {
        if !participant.advance_native_source(sink) {
            break;
        }
    }
    confirmations(participant);
    let [a_ssrc, b_ssrc]: [crate::media::Ssrc; 2] = identities.try_into().unwrap();
    let a = Mid::from("video-a");
    let b = Mid::from("video-b");
    assert!(
        participant
            .native_receivers
            .can_write_rtp(a, id, a_ssrc, 96u8.into())
    );
    assert!(
        !participant
            .native_receivers
            .can_write_rtp(a, audio.id(), a_ssrc, 96u8.into())
    );
    assert!(
        !participant
            .native_receivers
            .can_write_rtp(a, id, b_ssrc, 96u8.into())
    );
    assert!(
        !participant
            .native_receivers
            .can_write_rtp(a, id, a_ssrc, 97u8.into())
    );
    assert!(!participant.native_receivers.can_write_rtp(
        Mid::from("unknown"),
        id,
        a_ssrc,
        96u8.into()
    ));
    assert!(participant.native_receivers.playout_delay(a).is_some());
    assert!(participant.native_receivers.playout_delay(b).is_none());
    let mut frames = H264StreamBuilder::new(991, 100, 0, Instant::now());
    let mut cache = TrackStreamCache::new();
    feed(participant, handle, &mut tx, &mut cache, &mut frames, None);
    assert_eq!(outputs(participant, id), [a, b].into());
    feed(
        participant,
        handle,
        &mut tx,
        &mut cache,
        &mut frames,
        Some(180),
    );
    desire(
        participant,
        sink,
        5,
        8,
        Some(receiver_spec(id, "video-a", false)),
    );
    confirmations(participant);
    assert!(participant.native_receivers.playout_delay(a).is_none());
    assert_eq!(outputs(participant, id), [b].into());
    let mut bounded = receiver_spec(id, "video-a", false);
    let Some(wire::resource_settings::Receive::Video(options)) = bounded
        .settings
        .as_mut()
        .and_then(|settings| settings.receive.as_mut())
    else {
        unreachable!()
    };
    options.max_height = Some(180);
    desire(participant, sink, 5, 9, Some(bounded.clone()));
    confirmations(participant);
    participant.downstream.update_allocations(Instant::now());
    feed(participant, handle, &mut tx, &mut cache, &mut frames, None);
    assert_eq!(outputs(participant, id), [a, b].into());
    // No allocation pass between the new declaration and the output boundary.
    feed(
        participant,
        handle,
        &mut tx,
        &mut cache,
        &mut frames,
        Some(720),
    );
    assert_eq!(outputs(participant, id), [b].into());
    feed(
        participant,
        handle,
        &mut tx,
        &mut cache,
        &mut frames,
        Some(180),
    );
    assert_eq!(outputs(participant, id), [b].into());
    participant.downstream.update_allocations(Instant::now());
    feed(participant, handle, &mut tx, &mut cache, &mut frames, None);
    // Existing clean-frame recovery may wait its bounded alignment grace.
    feed(participant, handle, &mut tx, &mut cache, &mut frames, None);
    assert_eq!(outputs(participant, id), [a, b].into());
    if let Some(wire::resource_settings::Receive::Video(options)) = bounded
        .settings
        .as_mut()
        .and_then(|settings| settings.receive.as_mut())
    {
        options.max_height = Some(1);
    }
    desire(participant, sink, 5, 10, Some(bounded));
    confirmations(participant);
    participant.downstream.update_allocations(Instant::now());
    feed(participant, handle, &mut tx, &mut cache, &mut frames, None);
    assert_eq!(outputs(participant, id), [b].into());
    desire(participant, sink, 5, 11, None);
    assert!(!participant.downstream.video.has_slot(a));
    assert!(participant.downstream.video.has_slot(b));
    participant.complete_native_resource_cutoff(5, true);
    confirmations(participant);
    participant.native_source_removed(id, crate::participant::effect::TrackRemoval::Withdrawn);
    participant.remove_track_candidate(handle, id);
    for _ in 0..16 {
        if !participant.advance_native_source(sink) {
            break;
        }
    }
    confirmations(participant);
    assert!(
        !participant
            .native_receivers
            .can_write_rtp(b, id, b_ssrc, 96u8.into())
    );
    desire(participant, sink, 7, 12, None);
    assert!(!participant.downstream.video.has_slot(b));
    assert!(
        participant
            .transport
            .stream_tx_ssrc(Mid::from("second"))
            .is_some()
    );
    participant.complete_native_resource_cutoff(7, true);
    confirmations(participant);
    assert!(participant.disconnect_reason.is_none());
}
