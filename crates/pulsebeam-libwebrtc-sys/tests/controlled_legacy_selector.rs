use pulsebeam_webrtc_sys::*;
use std::{cell::Cell, net::Ipv4Addr, rc::Rc, sync::Mutex, time::Duration};
static CONTROLLED: Mutex<()> = Mutex::new(());
fn finish(
    world: &ControlledWorld,
    peer: &PeerConnection,
    id: OperationId,
) -> Option<SessionDescription> {
    for _ in 0..10_000 {
        world.pump(256);
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::OperationComplete(done) = event {
                if done.operation_id == id {
                    return done.result.unwrap();
                }
            }
        }
    }
    panic!("controlled SDP callback did not complete");
}
#[test]
fn selected_foreign_origin_retires_without_clock_or_caller_queue_progress() {
    let _guard = CONTROLLED.lock().unwrap_or_else(|e| e.into_inner());
    {
        let world = ControlledWorld::acquire(920, Duration::from_secs(3)).unwrap();
        let network = world.create_network().unwrap();
        let endpoint = network
            .register_endpoint(Ipv4Addr::new(10, 91, 10, 1).into())
            .unwrap();
        let other = network
            .register_endpoint(Ipv4Addr::new(10, 91, 10, 2).into())
            .unwrap();
        let input = EncodedVideoInput::new().unwrap();
        let factory = world
            .peer_factory_builder()
            .unwrap()
            .network_manager(endpoint.network_manager().unwrap())
            .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
            .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
            .audio_decoder_factory(AudioDecoderFactory::builtin_opus().unwrap())
            .video_encoder_factory(input.encoder_factory())
            .video_decoder_factory(input.encoded_receive_factory().unwrap())
            .build()
            .unwrap();
        let mut peer = factory.create_peer_connection(Default::default()).unwrap();
        let audio_source = factory.create_encoded_audio_source(1).unwrap();
        let audio = factory
            .create_encoded_audio_track("controlled-remembered", &audio_source)
            .unwrap();
        let sender = peer
            .add_audio_transceiver(&audio, RtpTransceiverDirection::SendOnly)
            .unwrap();
        let offer = finish(&world, &peer, peer.create_offer().unwrap()).unwrap();
        finish(&world, &peer, peer.set_local_description(offer).unwrap());
        let foreign = world
            .peer_factory_builder()
            .unwrap()
            .network_manager(other.network_manager().unwrap())
            .packet_socket_factory(other.packet_socket_factory().unwrap())
            .build()
            .unwrap();
        let source = foreign.create_video_source().unwrap();
        let video = foreign
            .create_video_track("controlled-remembered", &source)
            .unwrap();
        let selected = video.as_media_track();
        let caller = world
            .create_queue("not-a-legacy-continuation", QueuePriority::Normal)
            .unwrap();
        let ran = Rc::new(Cell::new(false));
        let flag = ran.clone();
        assert!(caller.post_local(move || flag.set(true)).unwrap());
        let now = world.now();
        let id = peer
            .request_legacy_stats_with_track(Some(&selected), LegacyStatsOutputLevel::Debug)
            .unwrap();
        assert_eq!(peer.event_observation().pending_operations, 1);
        drop(selected);
        drop(video);
        drop(source);
        drop(foreign);
        peer.close().unwrap();
        assert_eq!(world.now(), now);
        assert!(!ran.get(), "close dispatched a caller-owned queue");
        let mut terminal = 0;
        while let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::OperationComplete(done) if done.operation_id == id => {
                    terminal += 1;
                    assert!(done.result.is_err());
                }
                PeerConnectionEvent::LegacyStats(value) if value.operation_id == id => {
                    panic!("close secretly completed selected legacy work")
                }
                _ => {}
            }
        }
        assert_eq!(terminal, 1);
        assert!(!ran.get());
        // Only explicit caller progress is allowed to execute this queue.
        world.pump(256);
        assert!(ran.get());
        drop(peer);
        drop(sender);
        drop(audio);
        drop(audio_source);
    }
    // Source/input ownership must not leave a hidden Rc/context cycle.
    let _next = ControlledWorld::acquire(921, Duration::from_secs(3)).unwrap();
}

#[test]
fn close_preserves_sdk_report_copied_before_its_publication_fence() {
    let _guard = CONTROLLED.lock().unwrap_or_else(|e| e.into_inner());
    let world = ControlledWorld::acquire(923, Duration::from_secs(3)).unwrap();
    let network = world.create_network().unwrap();
    let endpoint = network
        .register_endpoint(Ipv4Addr::new(10, 91, 11, 1).into())
        .unwrap();
    let factory = world
        .peer_factory_builder()
        .unwrap()
        .network_manager(endpoint.network_manager().unwrap())
        .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
        .build()
        .unwrap();
    let mut peer = factory.create_peer_connection(Default::default()).unwrap();
    // Warm the SDK collector/cache and retire setup work. The next request's
    // first explicit pump runs PostGetStats, its second would run the fence.
    let warmup = peer
        .request_legacy_stats(LegacyStatsOutputLevel::Standard)
        .unwrap();
    let mut completed = false;
    for _ in 0..1000 {
        world.pump(1);
        while let Some(event) = peer.try_next_event() {
            if matches!(event, PeerConnectionEvent::LegacyStats(ref value) if value.operation_id == warmup)
            {
                completed = true;
            }
        }
        if completed {
            break;
        }
    }
    assert!(completed);
    world.pump(256);
    while peer.try_next_event().is_some() {}
    let id = peer
        .request_legacy_stats(LegacyStatsOutputLevel::Standard)
        .unwrap();
    assert_eq!(world.pump(1).dispatched, 1);
    assert!(
        peer.try_next_event().is_none(),
        "result published before the retirement fence"
    );
    peer.close().unwrap();
    let mut reports = 0;
    while let Some(event) = peer.try_next_event() {
        match event {
            PeerConnectionEvent::LegacyStats(value) if value.operation_id == id => {
                reports += 1;
                assert!(!value.reports.is_empty());
            }
            PeerConnectionEvent::OperationComplete(done) if done.operation_id == id => {
                panic!("already-produced SDK report replaced on close: {done:?}")
            }
            _ => {}
        }
    }
    assert_eq!(reports, 1);
    world.pump(256);
    assert!(
        peer.try_next_event().is_none(),
        "publication fence duplicated a preserved result"
    );
    assert_eq!(peer.event_observation().admitted_operations, 0);
    assert_eq!(world.now(), Duration::from_secs(3));
}
