//! Source fixtures for direct owning SDK capability installation, unrun.
use pulsebeam_webrtc_sys::*;
use std::{
    sync::{Arc, Condvar, Mutex},
    task::{Context, Wake, Waker},
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
struct Notification {
    ready: Mutex<bool>,
    changed: Condvar,
}
impl Wake for Notification {
    fn wake(self: Arc<Self>) {
        *self.ready.lock().unwrap() = true;
        self.changed.notify_one();
    }
}
fn finish_offer(
    session: &mut ProductionSession,
    peer: SessionPeerId,
    operation: OperationId,
) -> SessionDescription {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        while let Some(event) = session.try_peer_event(peer).unwrap() {
            if let SessionEvent::OperationComplete(done) = event
                && done.operation_id == operation
            {
                return done.result.unwrap().unwrap();
            }
        }
        let notification = Arc::new(Notification::default());
        let waker = Waker::from(notification.clone());
        if session
            .poll_ready(&mut Context::from_waker(&waker))
            .is_ready()
        {
            continue;
        }
        let mut ready = notification.ready.lock().unwrap();
        while !*ready {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "native capability offer did not complete"
            );
            ready = notification
                .changed
                .wait_timeout(ready, remaining)
                .unwrap()
                .0;
        }
    }
}

#[test]
fn full_capability_preferences_preserve_metadata_errors_and_migrated_ownership() {
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
            "capability-values",
            RtpTransceiverDirection::SendOnly,
        )
        .unwrap();
    assert_eq!(
        session.transceiver_media_type(transceiver).unwrap(),
        MediaType::Audio
    );
    assert!(!session.transceiver_stopping(transceiver).unwrap());
    assert!(!session.transceiver_receptive(transceiver).unwrap());
    assert_eq!(session.fired_direction(transceiver).unwrap(), None);
    assert_eq!(session.sframe_enabled(transceiver).unwrap(), None);
    assert_eq!(
        session.direction_value(transceiver).unwrap(),
        RtpTransceiverDirectionValue::SendOnly
    );
    assert_eq!(session.current_direction_value(transceiver).unwrap(), None);
    assert!(matches!(
        session.set_direction_value(transceiver, RtpTransceiverDirectionValue::Other(i32::MIN)),
        Err(RtpTransceiverOperationError::UnsafeDirection(i32::MIN)),
    ));
    assert_eq!(
        session.direction_value(transceiver).unwrap(),
        RtpTransceiverDirectionValue::SendOnly
    );
    let RtpTransceiverOperationError::Native(error) = session
        .set_direction_value(transceiver, RtpTransceiverDirectionValue::Other(4))
        .unwrap_err()
    else {
        panic!("Stopped must reach SDK policy validation");
    };
    assert_eq!(error.kind(), RtcErrorType::InvalidParameter);
    assert_eq!(
        error.message_bytes(),
        b"The set direction 'stopped' is invalid."
    );
    session
        .set_direction_value(transceiver, RtpTransceiverDirectionValue::Inactive)
        .unwrap();
    session
        .set_direction_value(transceiver, RtpTransceiverDirectionValue::Other(1))
        .unwrap();
    assert_eq!(
        session.direction_value(transceiver).unwrap(),
        RtpTransceiverDirectionValue::SendOnly
    );
    session.set_rtp_codec_preferences(transceiver, &[]).unwrap();
    assert!(
        session
            .rtp_codec_preferences(transceiver)
            .unwrap()
            .is_empty()
    );
    let mut codec = session
        .get_rtp_sender_capabilities(MediaType::Audio)
        .unwrap()
        .codecs()
        .into_iter()
        .find(|codec| codec.base_codec().name_bytes() == b"opus")
        .unwrap();
    // These fields are retained preferences, not direct codec-construction inputs.
    codec.set_preferred_payload_type(Some(i32::MIN));
    codec.set_scalability_modes(&[ScalabilityMode::from_raw(255), ScalabilityMode::L1T1]);
    let mut base = codec.base_codec();
    base.set_rtcp_feedback(&[RtcpFeedback::for_type(RtcpFeedbackType::Other(-7))]);
    codec.set_base_codec(&base);
    session
        .set_rtp_codec_preferences(transceiver, &[codec.clone(), codec.clone()])
        .unwrap();
    let stored = session.rtp_codec_preferences(transceiver).unwrap();
    assert_eq!(stored.len(), 1); // SDK deduplication, not a bridge rejection.
    assert_eq!(stored[0].base_codec(), codec.base_codec());
    assert_eq!(stored[0].preferred_payload_type(), Some(i32::MIN));
    assert_eq!(stored[0].scalability_modes(), codec.scalability_modes());

    let mut invalid_codec = codec.clone();
    let mut invalid_base = invalid_codec.base_codec();
    invalid_base.set_kind(MediaType::Other(i32::MIN));
    invalid_codec.set_base_codec(&invalid_base);
    let RtpCapabilityInstallError::Native(error) = session
        .set_rtp_codec_preferences(transceiver, &[invalid_codec.clone()])
        .unwrap_err()
    else {
        panic!("semantic codec rejection must be the owning SDK RTCError");
    };
    assert_eq!(error.kind(), RtcErrorType::InvalidModification);
    assert!(!error.message_bytes().is_empty());
    assert_eq!(invalid_codec.base_codec(), invalid_base);
    assert_eq!(session.rtp_codec_preferences(transceiver).unwrap(), stored);

    let defaults = session
        .header_extension_capabilities_to_negotiate(transceiver)
        .unwrap();
    let mut configured = defaults.clone();
    let index = configured
        .iter()
        .position(|extension| extension.uri_bytes() != b"urn:ietf:params:rtp-hdrext:sdes:mid")
        .unwrap();
    let requested = &mut configured[index];
    requested.set_direction(RtpTransceiverDirectionValue::Other(i32::MIN));
    requested.set_preferred_id(Some(&RtpHeaderExtensionId::new(i32::MIN)));
    requested.set_preferred_encrypt(!defaults[index].preferred_encrypt());
    session
        .set_header_extension_capabilities_to_negotiate(transceiver, &configured)
        .unwrap();
    let actual = session
        .header_extension_capabilities_to_negotiate(transceiver)
        .unwrap();
    assert_eq!(actual[index].direction_raw(), i32::MIN);
    // The SDK setter consumes only directions. Do not claim IDs/encryption were installed.
    assert_eq!(actual[index].preferred_id(), defaults[index].preferred_id());
    assert_eq!(
        actual[index].preferred_encrypt(),
        defaults[index].preferred_encrypt()
    );
    assert_eq!(configured[index].preferred_id().unwrap().value(), i32::MIN);
    assert!(
        session
            .negotiated_header_extension_capabilities(transceiver)
            .unwrap()
            .iter()
            .all(|extension| extension.direction() == RtpTransceiverDirectionValue::Stopped)
    );
    let RtpCapabilityInstallError::Native(error) = session
        .set_header_extension_capabilities_to_negotiate(transceiver, &[])
        .unwrap_err()
    else {
        panic!("header order/size rejection must be the owning SDK RTCError");
    };
    assert_eq!(error.kind(), RtcErrorType::InvalidModification);
    assert_eq!(
        session
            .header_extension_capabilities_to_negotiate(transceiver)
            .unwrap(),
        actual
    );

    // Restore the native header directions before the next SDP operation.
    session
        .set_header_extension_capabilities_to_negotiate(transceiver, &defaults)
        .unwrap();
    let session = thread::spawn(move || {
        let mut session = session;
        assert_eq!(session.rtp_codec_preferences(transceiver).unwrap(), stored);
        assert_eq!(
            session.transceiver_media_type(transceiver).unwrap(),
            MediaType::Audio
        );
        assert!(!session.transceiver_stopping(transceiver).unwrap());
        assert!(!session.transceiver_receptive(transceiver).unwrap());
        assert_eq!(session.fired_direction(transceiver).unwrap(), None);
        assert_eq!(session.sframe_enabled(transceiver).unwrap(), None);
        assert_eq!(
            session.direction_value(transceiver).unwrap(),
            RtpTransceiverDirectionValue::SendOnly
        );
        assert_eq!(session.current_direction_value(transceiver).unwrap(), None);
        let operation = session.create_offer(peer, false).unwrap();
        let offer = finish_offer(&mut session, peer, operation);
        assert!(offer.sdp.to_ascii_lowercase().contains("opus/48000"));
        assert_eq!(session.rtp_codec_preferences(transceiver).unwrap(), stored);
        (session, stored, configured)
    })
    .join()
    .unwrap();
    let (mut session, stored, configured) = session;
    // Public stop is idempotent and native direction errors remain owning SDK errors.
    session.stop_standard(transceiver).unwrap();
    session.stop_standard(transceiver).unwrap();
    assert!(session.transceiver_stopping(transceiver).unwrap());
    assert_eq!(
        session.direction_value(transceiver).unwrap(),
        RtpTransceiverDirectionValue::Stopped
    );
    let RtpTransceiverOperationError::Native(error) = session
        .set_direction_value(transceiver, RtpTransceiverDirectionValue::SendOnly)
        .unwrap_err()
    else {
        panic!("stopping-transceiver direction failure must come from SDK");
    };
    assert_eq!(error.kind(), RtcErrorType::InvalidState);
    // Teardown on another caller thread keeps SDK metadata independent.
    session.close_peer(peer).unwrap();
    assert!(session.transceiver_stopped(transceiver).unwrap());
    assert!(session.transceiver_stopping(transceiver).unwrap());
    assert_eq!(
        session.direction_value(transceiver).unwrap(),
        RtpTransceiverDirectionValue::Stopped
    );
    assert_eq!(
        session.current_direction_value(transceiver).unwrap(),
        Some(RtpTransceiverDirectionValue::Stopped)
    );
    assert!(matches!(
        session.set_rtp_codec_preferences(transceiver, &[]),
        Err(RtpCapabilityInstallError::Binding(PeerError {
            kind: PeerErrorKind::Closed,
            ..
        }))
    ));
    session.shutdown().unwrap();
    drop(session);
    assert_eq!(stored[0].preferred_payload_type(), Some(i32::MIN));
    assert_eq!(stored[0].scalability_modes()[0].raw(), 255);
    assert_eq!(configured[index].direction_raw(), i32::MIN);
}
