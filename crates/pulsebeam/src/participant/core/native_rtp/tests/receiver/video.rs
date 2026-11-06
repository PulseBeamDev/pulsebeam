use super::*;
use crate::media::{Rid, SimulcastLayer};
use crate::rtp::cache::TrackStreamCache;
use crate::rtp::test_utils::H264StreamBuilder;

fn receiver_spec(track: TrackId, mid: &str, ssrc: u32, fixed: bool) -> wire::ResourceSpec {
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
        declaration: Some(resource_spec::Declaration::Subscription(wire::SubscriptionSpec {
            track_id: track.as_str(),
            kind: wire::TrackKind::Video as i32,
            binding: Some(wire::TrackBinding { transport: Some(wire::track_binding::Transport::Rtp(wire::RtpParameters {
                mid: mid.into(),
                codec: Some(wire::RtpCodecParameters { payload_type: Some(96), format: Some(format), feedback: vec![wire::RtcpFeedback::TransportCc.as_str().into()], ..Default::default() }),
                encodings: vec![wire::RtpEncodingParameters { ssrc: Some(ssrc), ..Default::default() }],
                rtcp: Some(wire::RtcpParameters { cname: "component-video-receiver".into(), reduced_size: false }),
                header_extensions: vec![wire::RtpHeaderExtensionParameters { id: 1, uri: wire::RtpHeaderExtensionType::RtpHeaderExtensionMid.as_str().into() }, wire::RtpHeaderExtensionParameters { id: 2, uri: wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01.as_str().into() }, wire::RtpHeaderExtensionParameters { id: 3, uri: wire::RtpHeaderExtensionType::RtpHeaderExtensionPlayoutDelay.as_str().into() }],
            })) }),
        })),
        settings: Some(wire::ResourceSettings { enabled: Some(true), receive: Some(wire::resource_settings::Receive::Video(wire::VideoReceiveOptions { min_height: Some(1), min_fps: Some(1), playout_delay: fixed.then_some(wire::PlayoutDelay { min_ms: 100, max_ms: 100 }), ..Default::default() })) }),
    }
}

fn feed(
    participant: &mut NativeParticipant,
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

fn outputs(participant: &mut NativeParticipant, id: TrackId) -> std::collections::BTreeSet<Mid> {
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
    participant: &mut NativeParticipant,
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
    let first = receiver_spec(id, "video-a", 801, true);
    let Some(resource_spec::Declaration::Subscription(subscription)) = &first.declaration else {
        unreachable!()
    };
    let Some(wire::track_binding::Transport::Rtp(parameters)) = subscription
        .binding
        .as_ref()
        .and_then(|binding| binding.transport.as_ref())
    else {
        unreachable!()
    };
    let Track::Video(source) = &mut video else {
        unreachable!()
    };
    source.format = parameters
        .codec
        .as_ref()
        .and_then(|codec| codec.format.clone());
    let handle = handles.insert(id);
    participant.add_track_candidate(handle, video);
    for (reference, spec) in [
        (5, first.clone()),
        (7, receiver_spec(id, "video-b", 803, false)),
    ] {
        pulsebeam_proto::state::validation::validate_spec(&spec).unwrap();
        desire(participant, sink, reference, reference, Some(spec));
        assert_eq!(
            last_live_for(participant, reference).1.execution,
            wire::ExecutionStatus::WaitingForSource as i32
        );
        if reference == 5 {
            let blocked = Mid::from("blocked");
            assert!(participant.downstream.video.install_owned_slot(
                blocked,
                None,
                803.into(),
                96u8.into()
            ));
            desire(
                participant,
                sink,
                6,
                6,
                Some(receiver_spec(id, "blocked", 803, false)),
            );
            let rejected = native_data::tests::confirmations_for(participant, 6)
                .pop()
                .unwrap();
            assert_eq!(
                rejected.rejection.unwrap().code,
                wire::FailureCode::Incompatible as i32
            );
            assert!(participant.transport.stream_tx_ssrc(blocked).is_none());
            assert!(participant.downstream.video.has_slot(blocked));
            participant.downstream.video.remove_owned_slot(blocked);
        }
    }
    participant.native_source_binding(id, handle, true);
    for _ in 0..16 {
        if !participant.advance_native_source(sink) {
            break;
        }
    }
    confirmations(participant);
    let a = Mid::from("video-a");
    let b = Mid::from("video-b");
    assert!(
        participant
            .native_receivers
            .can_write_rtp(a, id, 801.into(), 96u8.into())
    );
    assert!(
        !participant
            .native_receivers
            .can_write_rtp(a, audio.id(), 801.into(), 96u8.into())
    );
    assert!(
        !participant
            .native_receivers
            .can_write_rtp(a, id, 803.into(), 96u8.into())
    );
    assert!(
        !participant
            .native_receivers
            .can_write_rtp(a, id, 801.into(), 97u8.into())
    );
    assert!(!participant.native_receivers.can_write_rtp(
        Mid::from("unknown"),
        id,
        801.into(),
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
        Some(receiver_spec(id, "video-a", 801, false)),
    );
    confirmations(participant);
    assert!(participant.native_receivers.playout_delay(a).is_none());
    assert_eq!(outputs(participant, id), [b].into());
    let mut bounded = receiver_spec(id, "video-a", 801, false);
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
            .can_write_rtp(b, id, 803.into(), 96u8.into())
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
