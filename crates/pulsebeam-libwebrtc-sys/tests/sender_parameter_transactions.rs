use pulsebeam_webrtc_sys::*;
use std::time::{Duration, Instant};

#[test]
fn native_sender_transactions_preserve_staleness_failure_and_one_shot_completion() {
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .encoded_audio()
        .build()
        .unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    let source = session.create_opus_source(1).unwrap();
    let transceiver = session
        .publish_opus(
            peer,
            source,
            "transactions",
            RtpTransceiverDirection::SendOnly,
        )
        .unwrap();
    let sender = session.sender(transceiver).unwrap();
    let stale = session.get_sender_parameters(sender).unwrap();
    let current = session.get_sender_parameters(sender).unwrap();
    let outcome = session.set_sender_parameters_value(sender, &stale).unwrap();
    assert!(!outcome.is_ok());
    assert!(!outcome.message_bytes().is_empty());
    let initial = current.encodings();
    let mut changed = session.get_sender_parameters(sender).unwrap();
    let mut encodings = changed.encodings();
    encodings[0].set_active(false);
    changed.set_encodings(&encodings);
    assert!(
        session
            .set_sender_parameters_value(sender, &changed)
            .unwrap()
            .is_ok()
    );
    assert!(!session.get_sender_parameters(sender).unwrap().encodings()[0].active());
    let mut invalid = session.get_sender_parameters(sender).unwrap();
    let mut encodings = invalid.encodings();
    // Native validation explicitly rejects a scale below one even before
    // negotiation. A negative max bitrate alone is accepted on that path.
    encodings[0].set_scale_resolution_down_by(Some(0.5));
    invalid.set_encodings(&encodings);
    let failure = session
        .set_sender_parameters_value(sender, &invalid)
        .unwrap();
    assert!(!failure.is_ok());
    let effective = session.get_sender_parameters(sender).unwrap().encodings();
    assert!(!effective[0].active());
    assert_eq!(
        effective[0].scale_resolution_down_by(),
        initial[0].scale_resolution_down_by()
    );

    let mut fresh = session.get_sender_parameters(sender).unwrap();
    let mut encodings = fresh.encodings();
    encodings[0].set_active(true);
    fresh.set_encodings(&encodings);
    let operation = session.set_sender_parameters_async(sender, &fresh).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !session
        .sender_parameters_operation_ready(operation)
        .unwrap()
    {
        assert!(
            Instant::now() < deadline,
            "native async completion exhausted"
        );
        std::thread::yield_now();
    }
    assert!(
        session
            .take_sender_parameters_result(operation)
            .unwrap()
            .unwrap()
            .is_ok()
    );
    assert!(
        session
            .take_sender_parameters_result(operation)
            .unwrap()
            .is_none()
    );
    assert!(session.get_sender_parameters(sender).unwrap().encodings()[0].active());
    session
        .release_sender_parameters_operation(operation)
        .unwrap();
    session.shutdown().unwrap();
}
