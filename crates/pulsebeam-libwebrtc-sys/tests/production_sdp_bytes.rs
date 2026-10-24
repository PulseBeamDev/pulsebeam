use pulsebeam_webrtc_sys::*;
use std::time::{Duration, Instant};

fn drain_syntax_failure(
    session: &mut ProductionSession,
    peer: SessionPeerId,
    operation: OperationId,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = session.try_peer_event(peer).unwrap() {
            match event {
                SessionEvent::OperationComplete(done) if done.operation_id == operation => {
                    let error = done.result.unwrap_err();
                    assert_eq!(error.kind, PeerErrorKind::Syntax);
                    assert!(!error.message.is_empty());
                    break;
                }
                SessionEvent::OperationFailedBytes {
                    operation_id,
                    error,
                } if operation_id == operation => {
                    assert_eq!(error.kind, PeerErrorKind::Syntax);
                    assert!(!error.message.is_empty());
                    break;
                }
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "native byte SDP failure timeout");
        std::thread::yield_now();
    }
}

#[test]
fn byte_sdp_input_native_failures_and_final_drop_follow_migrated_actor_ownership() {
    let mut session = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    let invalid = SessionDescriptionBytes {
        kind: SessionDescriptionType::Offer,
        sdp: vec![0xff, 0, b'\r', b'\n'],
    };
    let first = session
        .set_remote_description_bytes(peer, invalid.clone())
        .unwrap();
    let (mut session, invalid) = std::thread::spawn(move || {
        drain_syntax_failure(&mut session, peer, first);
        // Native failed parsing did not apply a remote description or fabricate
        // a successful setter echo. Byte snapshots stay independently owned.
        assert_eq!(
            session.descriptions_bytes(peer).unwrap(),
            PeerDescriptionBytes::default()
        );
        let second = session
            .set_local_description_bytes(peer, invalid.clone())
            .unwrap();
        drain_syntax_failure(&mut session, peer, second);
        (session, invalid)
    })
    .join()
    .unwrap();

    let pending = session
        .set_remote_description_bytes(peer, invalid.clone())
        .unwrap();
    std::thread::spawn(move || {
        session.close_peer(peer).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut terminal = false;
        while !terminal {
            if let Some(event) = session.try_peer_event(peer).unwrap() {
                terminal = match event {
                    SessionEvent::OperationComplete(done) if done.operation_id == pending => {
                        let error = done.result.unwrap_err();
                        assert!(matches!(error.kind, PeerErrorKind::Syntax | PeerErrorKind::Closed)); true
                    }
                    SessionEvent::OperationFailedBytes { operation_id, error } if operation_id == pending => {
                        assert_eq!(error.kind, PeerErrorKind::Syntax); true
                    }
                    _ => false,
                };
            }
            assert!(Instant::now() < deadline, "close lost accepted byte SDP terminal");
            std::thread::yield_now();
        }
        while let Some(event) = session.try_peer_event(peer).unwrap() {
            assert!(!matches!(&event, SessionEvent::OperationComplete(done) if done.operation_id == pending));
            assert!(!matches!(&event, SessionEvent::OperationFailedBytes { operation_id, .. } if *operation_id == pending));
        }
        assert_eq!(session.set_remote_description_bytes(peer, invalid.clone()).unwrap_err().kind, PeerErrorKind::Closed);
        session.shutdown().unwrap(); drop(session);
        assert_eq!(invalid.sdp, vec![0xff, 0, b'\r', b'\n']);
    }).join().unwrap();
}
