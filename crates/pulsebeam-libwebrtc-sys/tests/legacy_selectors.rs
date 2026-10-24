use pulsebeam_webrtc_sys::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

static TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn finish(peer: &PeerConnection, id: OperationId) -> Option<SessionDescription> {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(PeerConnectionEvent::OperationComplete(done)) = peer.try_next_event() {
            if done.operation_id == id {
                return done.result.unwrap();
            }
        }
        assert!(Instant::now() < end, "SDP callback timeout");
        thread::yield_now();
    }
}
fn report(peer: &PeerConnection, id: OperationId) -> PeerLegacyStatsSnapshot {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        match peer.try_next_event() {
            Some(PeerConnectionEvent::LegacyStats(value)) if value.operation_id == id => {
                return value;
            }
            Some(PeerConnectionEvent::OperationComplete(done)) if done.operation_id == id => {
                panic!("unexpected legacy terminal: {done:?}")
            }
            _ => {}
        }
        assert!(Instant::now() < end, "legacy callback timeout");
        thread::yield_now();
    }
}
fn remembered_audio(
    factory: &PeerConnectionFactory,
    peer: &PeerConnection,
    id: &str,
) -> (AudioTrack, RtpTransceiver) {
    let source = factory.create_audio_source().unwrap();
    let track = factory.create_audio_track(id, &source).unwrap();
    let transceiver = peer
        .add_audio_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let offer = finish(peer, peer.create_offer().unwrap()).unwrap();
    finish(peer, peer.set_local_description(offer).unwrap());
    (track, transceiver)
}
fn has_track(snapshot: &PeerLegacyStatsSnapshot, id: &[u8]) -> bool {
    snapshot.reports.iter().any(|r| {
        r.id.kind() == LegacyStatsType::Track
            && r.find(LegacyStatsName::TrackId)
                .is_some_and(|v| match &v.value {
                    LegacyStatsValueData::String(bytes)
                    | LegacyStatsValueData::StaticString(bytes) => bytes == id,
                    _ => false,
                })
    })
}
#[test]
fn native_selector_rejection_does_not_admit_or_poison_stats() {
    let _guard = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let peer = factory.create_peer_connection(Default::default()).unwrap();
    let source = factory.create_video_source().unwrap();
    let video = factory
        .create_video_track("unregistered\0track", &source)
        .unwrap();
    let track = video.as_media_track();
    assert_eq!(track.id(), b"unregistered\0track");
    assert_eq!(track.kind(), MediaTrackKind::Video);
    assert_eq!(track.state(), MediaTrackState::Live);
    assert!(track.same_resource(&video.as_media_track()));
    assert!(track.enabled());
    assert!(track.set_enabled(false));
    assert!(!video.as_media_track().enabled());
    let before = peer.event_observation();
    assert!(matches!(
        peer.request_legacy_stats_with_track(Some(&track), LegacyStatsOutputLevel::Debug),
        Err(LegacyStatsRequestError::NativeRejected)
    ));
    assert_eq!(
        peer.event_observation().admitted_operations,
        before.admitted_operations
    );
    assert_eq!(peer.event_observation().pending_operations, 0);
    let id = peer
        .request_legacy_stats_with_track(None, LegacyStatsOutputLevel::Standard)
        .unwrap();
    assert!(!report(&peer, id).reports.is_empty());
}

struct RoleProbe(Arc<AtomicUsize>);
impl Drop for RoleProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[test]
fn foreign_matching_id_keeps_origin_roles_until_native_capture_retirement() {
    let _guard = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut peer = factory.create_peer_connection(Default::default()).unwrap();
    let (audio, transceiver) = remembered_audio(&factory, &peer, "remembered-audio");
    let original = audio.as_media_track();
    let id = peer
        .request_legacy_stats_with_track(Some(&original), LegacyStatsOutputLevel::Standard)
        .unwrap();
    assert!(has_track(&report(&peer, id), b"remembered-audio"));
    assert!(original.same_resource(&transceiver.sender().track_reference().unwrap()));

    let dropped = Arc::new(AtomicUsize::new(0));
    let signaling = SignalingThread::start().unwrap();
    let probe = RoleProbe(dropped.clone());
    assert!(
        signaling
            .post_delayed(Duration::from_secs(3600), move || drop(probe))
            .unwrap()
    );
    let foreign = PeerConnectionFactory::builder()
        .signaling_thread(signaling)
        .build()
        .unwrap();
    let source = foreign.create_video_source().unwrap();
    // SDK validity is remembered ID, not factory, object identity or even kind.
    let video = foreign
        .create_video_track("remembered-audio", &source)
        .unwrap();
    let selected = video.as_media_track();
    assert!(!selected.same_resource(&original));
    assert_eq!(selected.kind(), MediaTrackKind::Video);
    let id = peer
        .request_legacy_stats_with_track(Some(&selected), LegacyStatsOutputLevel::Debug)
        .unwrap();
    drop(selected);
    drop(video);
    drop(source);
    drop(foreign);
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        0,
        "origin signaling role retired while accepted selection still owns it"
    );
    let snapshot = report(&peer, id);
    assert!(has_track(&snapshot, b"remembered-audio"));
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "consumed retired selection leaked its origin role"
    );
    peer.close().unwrap();
    drop(peer);
    drop(transceiver);
    drop(audio);
    drop(original);
    drop(factory);
    thread::spawn(move || assert!(has_track(&snapshot, b"remembered-audio")))
        .join()
        .unwrap();
}

#[test]
fn controlled_close_and_drop_retire_legacy_posts_without_dispatch_or_time_advance() {
    let _guard = TEST.lock().unwrap_or_else(|e| e.into_inner());
    let world = ControlledWorld::acquire(918, Duration::from_secs(3)).unwrap();
    let network = world.create_network().unwrap();
    let endpoint = network
        .register_endpoint(std::net::Ipv4Addr::new(10, 91, 8, 1).into())
        .unwrap();
    let factory = world
        .peer_factory_builder()
        .unwrap()
        .network_manager(endpoint.network_manager().unwrap())
        .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
        .build()
        .unwrap();
    let mut peer = factory.create_peer_connection(Default::default()).unwrap();
    let before = world.now();
    let id = peer
        .request_legacy_stats(LegacyStatsOutputLevel::Standard)
        .unwrap();
    assert_eq!(peer.event_observation().pending_operations, 1);
    peer.close().unwrap();
    let mut terminals = 0;
    while let Some(event) = peer.try_next_event() {
        match event {
            PeerConnectionEvent::OperationComplete(done) if done.operation_id == id => {
                terminals += 1;
                assert!(done.result.is_err());
            }
            PeerConnectionEvent::LegacyStats(value) if value.operation_id == id => {
                panic!("close secretly dispatched legacy callback")
            }
            _ => {}
        }
    }
    assert_eq!(terminals, 1);
    assert_eq!(world.now(), before);
    // Repeat the real pending-drop path. SDK peer teardown can independently
    // queue lifecycle work; exact removal of a canceled envelope is qualified
    // separately by the isolated driver primitive unit test.
    drop(peer);
    for _ in 0..32 {
        let peer = factory.create_peer_connection(Default::default()).unwrap();
        peer.request_legacy_stats(LegacyStatsOutputLevel::Debug)
            .unwrap();
        drop(peer);
        assert_eq!(world.now(), before);
    }
    world.pump(256);
    assert_eq!(world.now(), before);
}
