use std::collections::HashSet;

use pulsebeam_webrtc_sys::{
    PeerConfiguration, PeerConnectionEvent, PeerConnectionFactory, PeerErrorKind,
    ProductionSession, ProductionSessionConfig, SessionDescription, SessionDescriptionType,
};

#[test]
fn terminal_results_hold_admission_until_observed_and_close_never_duplicates_them() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let invalid = || SessionDescription {
        kind: SessionDescriptionType::Offer,
        sdp: "not SDP".into(),
    };
    let mut accepted = HashSet::new();
    for _ in 0..64 {
        accepted.insert(peer.set_remote_description(invalid()).unwrap());
    }
    let before = peer.event_observation();
    assert_eq!(before.admitted_operations, 64);
    assert_eq!(before.pending_operations, 0);
    for _ in 0..256 {
        assert_eq!(
            peer.create_offer().unwrap_err().kind,
            PeerErrorKind::ResourceExhausted
        );
    }
    assert_eq!(
        peer.request_stats().unwrap_err().kind,
        PeerErrorKind::ResourceExhausted
    );
    assert_eq!(peer.event_observation(), before);
    let Some(PeerConnectionEvent::OperationComplete(first)) = peer.try_next_event() else {
        panic!("missing first retained terminal");
    };
    assert_eq!(first.result.unwrap_err().kind, PeerErrorKind::Syntax);
    assert!(accepted.remove(&first.operation_id));
    assert_eq!(peer.event_observation().admitted_operations, 63);
    accepted.insert(peer.set_remote_description(invalid()).unwrap());
    peer.close().unwrap();
    peer.close().unwrap();
    for _ in 0..128 {
        assert_eq!(peer.create_offer().unwrap_err().kind, PeerErrorKind::Closed);
        assert_eq!(
            peer.request_stats().unwrap_err().kind,
            PeerErrorKind::Closed
        );
    }
    assert_eq!(peer.event_observation().admitted_operations, 64);
    let mut closed = 0;
    while let Some(event) = peer.try_next_event() {
        match event {
            PeerConnectionEvent::OperationComplete(done) => {
                assert!(
                    accepted.remove(&done.operation_id),
                    "duplicate/unaccepted terminal"
                );
                assert_eq!(done.result.unwrap_err().kind, PeerErrorKind::Syntax);
            }
            PeerConnectionEvent::Closed => closed += 1,
            event => panic!("unexpected closed-peer event: {event:?}"),
        }
    }
    assert!(accepted.is_empty());
    assert_eq!(closed, 1);
    assert_eq!(peer.event_observation().admitted_operations, 0);
}

#[test]
fn actor_source_retirement_releases_entries_and_is_idempotent() {
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .encoded_audio()
        .build()
        .unwrap();
    for _ in 0..128 {
        let id = session.create_opus_source(1).unwrap();
        session.close_source(id).unwrap();
        session.close_source(id).unwrap();
        let error = session
            .push_opus(
                id,
                &pulsebeam_webrtc_sys::OpusInputFrame {
                    data: vec![0xf8, 0xff, 0xfe],
                    rtp_timestamp: 0,
                    samples_per_channel: 960,
                },
            )
            .unwrap_err();
        assert_eq!(error.kind, PeerErrorKind::InvalidParameter);
    }
    session.shutdown().unwrap();
}
