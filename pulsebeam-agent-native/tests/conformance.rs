use pulsebeam_agent_core::{MediaKind, MonotonicTime, TrackId, TransportGeneration};
use pulsebeam_agent_native::media::{MediaRoute, RtpPacket, RtpRouter};

#[test]
fn native_router_preserves_logical_to_physical_mapping() {
    let mut router = RtpRouter::new(4);
    let held = RtpPacket {
        mid: String::from("main"),
        sequence: 1,
        timestamp: 10,
        marker: true,
        payload: vec![1, 2, 3],
    };
    assert!(
        router
            .route(held.clone())
            .expect("unknown media is held")
            .is_none()
    );
    let released = router.install(MediaRoute {
        logical_mid: String::from("main"),
        physical_mid: String::from("video-0"),
        track_id: TrackId::from("track-1"),
        kind: MediaKind::Video,
        generation: TransportGeneration::new(1),
    });
    assert_eq!(released, vec![held]);
    let routed = router
        .route(RtpPacket {
            mid: String::from("main"),
            sequence: 2,
            timestamp: 20,
            marker: false,
            payload: vec![4],
        })
        .expect("installed media route is valid")
        .expect("installed media is routed");
    assert_eq!(routed.mid, "video-0");
    assert!(!router.remove("main", TransportGeneration::new(2)));
    assert!(router.remove("main", TransportGeneration::new(1)));
}

#[test]
fn native_generation_and_clock_values_are_owned() {
    let now = MonotonicTime::from_millis(20);
    assert_eq!(
        now.duration_since(MonotonicTime::from_millis(5))
            .as_millis(),
        15
    );
    assert_ne!(TransportGeneration::new(1), TransportGeneration::new(2));
}
