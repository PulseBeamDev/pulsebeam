use pulsebeam_webrtc_sys::*;
use std::time::{Duration, Instant};

fn completion(peer: &PeerConnection, operation: OperationId) -> Option<SessionDescription> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::OperationComplete(done) if done.operation_id == operation => {
                    return done.result.unwrap();
                }
                PeerConnectionEvent::OperationFailedBytes {
                    operation_id,
                    error,
                } if operation_id == operation => panic!("byte native error: {error:?}"),
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "native operation timeout");
        std::thread::yield_now();
    }
}
#[test]
fn native_ice_snapshots_keep_legacy_standardized_and_trickle_presence_distinct() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut alice = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let mut bob = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let initial = bob.ice_snapshot();
    assert_eq!(initial.legacy_state, PeerIceConnectionState::New);
    assert_eq!(initial.standardized_state, PeerIceConnectionState::New);
    assert_eq!(initial.can_trickle_ice_candidates, None);
    let _channel = alice
        .create_data_channel("ice-snapshot", DataChannelConfiguration::default())
        .unwrap();
    let offer = completion(&alice, alice.create_offer().unwrap()).unwrap();
    assert!(offer.sdp.contains("a=ice-options:trickle"));
    completion(&bob, bob.set_remote_description(offer).unwrap());
    assert_eq!(bob.can_trickle_ice_candidates(), Some(true));
    bob.close().unwrap();
    assert_eq!(bob.ice_connection_state(), PeerIceConnectionState::Closed);
    assert_eq!(
        bob.standardized_ice_connection_state(),
        PeerIceConnectionState::Closed
    );
    drop(bob);
    assert_eq!(initial.can_trickle_ice_candidates, None);
    alice.close().unwrap();
}
#[test]
fn ice_state_queries_and_byte_candidate_failures_follow_migrated_actor_ownership() {
    let mut session = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    let invalid = IceCandidateBytes {
        sdp_mid: vec![0xff, 0],
        sdp_mline_index: i32::MIN,
        // A wrong non-UTF-8 attribute reaches the native parser's actual
        // byte-bearing "Expected candidate got ..." explanation.
        candidate: vec![0xfe, b':', b'x'],
    };
    let (mut session, invalid) = std::thread::spawn(move || {
        let state = session.peer_ice_snapshot(peer).unwrap();
        assert_eq!(state.legacy_state, PeerIceConnectionState::New);
        assert_eq!(session.peer_can_trickle_ice_candidates(peer).unwrap(), None);
        let error = session
            .add_ice_candidate_sync_bytes(peer, invalid.clone())
            .unwrap_err();
        assert_eq!(error.kind, PeerErrorKind::Syntax);
        assert!(error.message.contains(&0xfe));
        assert!(String::from_utf8(error.message).is_err());
        assert_eq!(
            session
                .remove_ice_candidate_bytes(peer, invalid.clone())
                .unwrap_err()
                .kind,
            PeerErrorKind::Syntax
        );
        let operation = session
            .add_ice_candidate_bytes(peer, invalid.clone())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(event) = session.try_peer_event(peer).unwrap() {
                match event {
                    SessionEvent::OperationComplete(done) if done.operation_id == operation => {
                        panic!("native byte candidate error was projected to text: {done:?}");
                    }
                    SessionEvent::OperationFailedBytes {
                        operation_id,
                        error,
                    } if operation_id == operation => {
                        assert_eq!(error.kind, PeerErrorKind::Syntax);
                        assert!(error.message.contains(&0xfe));
                        assert!(String::from_utf8(error.message).is_err());
                        break;
                    }
                    _ => {}
                }
            }
            assert!(
                Instant::now() < deadline,
                "native candidate failure timeout"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            session
                .peer_event_observation(peer)
                .unwrap()
                .admitted_operations,
            0
        );
        (session, invalid)
    })
    .join()
    .unwrap();
    std::thread::spawn(move || {
        session.close_peer(peer).unwrap();
        assert_eq!(
            session.peer_ice_connection_state(peer).unwrap(),
            PeerIceConnectionState::Closed
        );
        assert_eq!(
            session
                .peer_standardized_ice_connection_state(peer)
                .unwrap(),
            PeerIceConnectionState::Closed
        );
        assert_eq!(
            session
                .add_ice_candidate_sync_bytes(peer, invalid.clone())
                .unwrap_err()
                .kind,
            PeerErrorKind::Closed
        );
        session.shutdown().unwrap();
        drop(session);
        assert_eq!(invalid.sdp_mid, [0xff, 0]);
    })
    .join()
    .unwrap();
}
#[test]
fn candidate_text_projection_is_checked_and_open_state_values_are_not_narrowed() {
    let bytes = IceCandidateBytes {
        sdp_mid: vec![0xff, 0],
        sdp_mline_index: -7,
        candidate: b"candidate:x".to_vec(),
    };
    assert!(IceCandidate::try_from(bytes.clone()).is_err());
    assert_eq!(bytes.sdp_mline_index, -7);
    let text = IceCandidate {
        sdp_mid: "a\0b".into(),
        sdp_mline_index: 0,
        candidate: "candidate:example".into(),
    };
    assert_eq!(
        IceCandidate::try_from(IceCandidateBytes::from(text.clone())).unwrap(),
        text
    );
    for raw in [i32::MIN, -1, 0, 7, 8, i32::MAX] {
        assert_eq!(PeerIceConnectionState::from_raw(raw).raw(), raw);
    }
}
