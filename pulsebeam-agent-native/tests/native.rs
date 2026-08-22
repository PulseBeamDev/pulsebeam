use pulsebeam_agent_core::{CoreConfig, MediaKind, TrackId, TransportGeneration};
use pulsebeam_agent_native::media::{MediaRoute, RtpPacket, RtpRouter};
use pulsebeam_agent_native::{AgentBuilder, ConnectionState};

#[test]
fn public_native_surface_keeps_core_and_media_ownership_separate() {
    let (agent, runner) = AgentBuilder::new("participant")
        .with_config(CoreConfig::default())
        .build()
        .expect("builder should create a native driver");
    assert_eq!(agent.participant_id().as_str(), "participant");
    assert_eq!(runner.driver().core().state(), ConnectionState::Idle);

    let mut router = RtpRouter::default();
    router.install(MediaRoute {
        logical_mid: "logical".to_owned(),
        physical_mid: "physical".to_owned(),
        track_id: TrackId::from("track"),
        kind: MediaKind::Audio,
        generation: TransportGeneration::new(1),
    });
    let routed = router
        .route(RtpPacket {
            mid: "logical".to_owned(),
            sequence: 7,
            timestamp: 8,
            marker: true,
            payload: vec![9],
        })
        .expect("route should accept a valid RTP packet")
        .expect("installed route should be active");
    assert_eq!(routed.mid, "physical");
}
