//! Native SFrame source fixtures. Stock creation preserves errors/side effects;
//! these do not claim that the SDK has implemented an encryption pipeline.
use pulsebeam_webrtc_sys::*;
use std::{sync::Arc, thread};
fn send_sync<T: Send + Sync>() {}

#[test]
fn native_init_preserves_complete_enum_domain_copy_assignment_and_migration() {
    send_sync::<SframeEncrypterInit>();
    for mode in [
        SframeMode::PerFrame,
        SframeMode::PerPacket,
        SframeMode::Other(i32::MIN),
        SframeMode::Other(i32::MAX),
    ] {
        for suite in [
            SframeCipherSuite::Aes128CtrHmacSha256_80,
            SframeCipherSuite::Aes128CtrHmacSha256_64,
            SframeCipherSuite::Aes128CtrHmacSha256_32,
            SframeCipherSuite::Aes128GcmSha256_128,
            SframeCipherSuite::Aes256GcmSha512_128,
            SframeCipherSuite::Other(i32::MIN),
            SframeCipherSuite::Other(i32::MAX),
        ] {
            assert_eq!(SframeMode::from_raw(mode.raw()), mode);
            assert_eq!(SframeCipherSuite::from_raw(suite.raw()), suite);
            let mut init = SframeEncrypterInit::new(mode, suite);
            let copy = init.clone();
            init.set_mode(SframeMode::Other(-7));
            init.set_cipher_suite(SframeCipherSuite::Other(-8));
            assert_eq!(copy.mode(), mode);
            assert_eq!(copy.cipher_suite(), suite);
            init.clone_from(&copy);
            drop(copy);
            let init = Arc::new(init);
            let snapshot = init.clone();
            thread::spawn(move || {
                assert_eq!(snapshot.mode(), mode);
                assert_eq!(snapshot.cipher_suite(), suite);
                snapshot.as_ref().clone()
            })
            .join()
            .unwrap();
            assert_eq!(init.mode(), mode);
            assert_eq!(init.cipher_suite(), suite);
        }
    }
}
fn unsupported(error: SframeCreateError, message: &str) {
    match error {
        SframeCreateError::Native(error) => {
            assert_eq!(error.kind(), RtcErrorType::UnsupportedOperation);
            assert!(!error.is_ok());
            assert_eq!(error.message(), message);
        }
        error => panic!("expected pinned SDK error, got {error:?}"),
    }
}
#[test]
fn migrated_actor_preserves_native_creation_errors_and_owner_side_effects() {
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .encoded_audio()
        .build()
        .unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    let source = session.create_opus_source(1).unwrap();
    let transceiver = session
        .publish_opus(peer, source, "sframe", RtpTransceiverDirection::SendReceive)
        .unwrap();
    let sender = session.sender(transceiver).unwrap();
    let receiver = session.receiver(transceiver).unwrap();
    let mut second = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    let init =
        SframeEncrypterInit::new(SframeMode::PerFrame, SframeCipherSuite::Aes128GcmSha256_128);
    assert!(matches!(
        second.create_sframe_encrypter(sender, &init),
        Err(SframeCreateError::Binding(_))
    ));
    assert!(matches!(
        second.create_sframe_decrypter(receiver, SframeCipherSuite::Aes128GcmSha256_128),
        Err(SframeCreateError::Binding(_))
    ));
    drop(second);
    assert_eq!(session.sframe_enabled(transceiver).unwrap(), None);
    unsupported(
        session.create_sframe_encrypter(sender, &init).unwrap_err(),
        "Sframe encrypter not yet implemented",
    );
    assert_eq!(session.sframe_enabled(transceiver).unwrap(), Some(true));
    // Native TryToEnableSframe was invoked even though creation failed.
    let mut session = thread::spawn(move || {
        let mut session = session;
        unsupported(
            session
                .create_sframe_decrypter(receiver, SframeCipherSuite::Aes256GcmSha512_128)
                .unwrap_err(),
            "Sframe decrypter not yet implemented",
        );
        assert_eq!(session.sframe_enabled(transceiver).unwrap(), Some(true));
        session
    })
    .join()
    .unwrap();
    session.close_peer(peer).unwrap();
    // Close stops media but does not clear the SDK's owning transceiver graph.
    unsupported(
        session.create_sframe_encrypter(sender, &init).unwrap_err(),
        "Sframe encrypter not yet implemented",
    );
    thread::spawn(move || drop(session)).join().unwrap();
}

fn complete_local(
    peer: &PeerConnection,
    operation: OperationId,
    removed: &mut Vec<RtpReceiver>,
) -> Option<SessionDescription> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        while let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::OperationComplete(done) if done.operation_id == operation => {
                    return done.result.unwrap();
                }
                PeerConnectionEvent::TrackRemoved(receiver) => removed.push(receiver),
                // Drop TrackAdded transceivers: only endpoint handles may retain
                // SDK graph owners across the second completed negotiation.
                _ => (),
            }
        }
        assert!(std::time::Instant::now() < deadline);
        thread::yield_now();
    }
}
fn negotiate_local(alice: &PeerConnection, bob: &PeerConnection, removed: &mut Vec<RtpReceiver>) {
    let offer = complete_local(alice, alice.create_offer().unwrap(), removed).unwrap();
    complete_local(
        alice,
        alice.set_local_description(offer.clone()).unwrap(),
        removed,
    );
    complete_local(bob, bob.set_remote_description(offer).unwrap(), removed);
    let answer = complete_local(bob, bob.create_answer().unwrap(), removed).unwrap();
    complete_local(
        bob,
        bob.set_local_description(answer.clone()).unwrap(),
        removed,
    );
    complete_local(
        alice,
        alice.set_remote_description(answer).unwrap(),
        removed,
    );
}
fn disabled(error: SframeCreateError) {
    match error {
        SframeCreateError::Native(error) => {
            assert_eq!(error.kind(), RtcErrorType::InvalidModification);
            assert_eq!(
                error.message(),
                "Cannot enable Sframe after it has been disabled by a completed negotiation."
            );
        }
        error => panic!("expected native disabled error, got {error:?}"),
    }
}
#[test]
fn surviving_endpoint_and_queued_remove_receiver_retain_sdk_transceiver_after_negotiation_removal()
{
    let factory = PeerConnectionFactory::builder()
        .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
        .build()
        .unwrap();
    let mut alice = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let bob = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let _channel = alice
        .create_data_channel("sframe-owner", DataChannelConfiguration::default())
        .unwrap();
    let source = factory.create_encoded_audio_source(1).unwrap();
    let track = factory
        .create_encoded_audio_track("sframe-owner", &source)
        .unwrap();
    let transceiver = alice
        .add_audio_transceiver(&track, RtpTransceiverDirection::SendReceive)
        .unwrap();
    let sender = transceiver.sender();
    let receiver = transceiver.receiver();
    let mut removed = Vec::new();
    negotiate_local(&alice, &bob, &mut removed);
    assert_eq!(transceiver.sframe_enabled(), Some(false));
    transceiver.stop_standard().unwrap();
    // No local transceiver handle remains to mask endpoint owner retention.
    drop(transceiver);
    negotiate_local(&alice, &bob, &mut removed);
    assert!(alice.audio_transceivers().unwrap().is_empty());
    assert!(bob.audio_transceivers().unwrap().is_empty());
    let init =
        SframeEncrypterInit::new(SframeMode::PerFrame, SframeCipherSuite::Aes128GcmSha256_128);
    disabled(sender.create_sframe_encrypter(&init).unwrap_err());
    disabled(
        receiver
            .create_sframe_decrypter(init.cipher_suite())
            .unwrap_err(),
    );
    // OnRemoveTrack retained the owner while it was still in the SDK list.
    assert!(!removed.is_empty());
    for receiver in removed {
        disabled(
            receiver
                .create_sframe_decrypter(init.cipher_suite())
                .unwrap_err(),
        );
    }
}
