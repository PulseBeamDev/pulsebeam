//! Native SFrame key-management and SDK negotiation proof. Key callbacks are
//! exercised independently of live encrypted-media delivery.
use pulsebeam_webrtc_sys::*;
use std::{
    sync::{Arc, Mutex},
    thread,
};
static TEST_LOCK: Mutex<()> = Mutex::new(());
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
            SframeCipherSuite::Aes256CtrHmacSha512_80,
            SframeCipherSuite::Aes256CtrHmacSha512_64,
            SframeCipherSuite::Aes256CtrHmacSha512_32,
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
#[test]
fn migrated_actor_preserves_native_sframe_creation_side_effects() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
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
    let encrypter = session.create_sframe_encrypter(sender, &init).unwrap();
    assert_eq!(session.sframe_enabled(transceiver).unwrap(), Some(true));
    drop(encrypter);
    // Native TryToEnableSframe succeeds and creates a native key-management
    // product for the actual SDK transceiver.
    let mut session = thread::spawn(move || {
        let mut session = session;
        let decrypter = session
            .create_sframe_decrypter(receiver, SframeCipherSuite::Aes256GcmSha512_128)
            .unwrap();
        assert_eq!(session.sframe_enabled(transceiver).unwrap(), Some(true));
        drop(decrypter);
        session
    })
    .join()
    .unwrap();
    session.close_peer(peer).unwrap();
    thread::spawn(move || drop(session)).join().unwrap();
}

#[test]
fn native_decryption_key_updates_complete_asynchronously() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let world = ControlledWorld::acquire(0x5f4a, std::time::Duration::from_secs(1)).unwrap();
    let network = world.create_network().unwrap();
    let endpoint = network
        .register_endpoint("10.0.0.1".parse().unwrap())
        .unwrap();
    let runtime = world.runtime().unwrap().with_endpoint(&endpoint).unwrap();
    let mut session = Session::with_runtime(runtime, SessionConfig::default())
        .encoded_audio()
        .build()
        .unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    let source = session.create_opus_source(1).unwrap();
    let transceiver = session
        .publish_opus(
            peer,
            source,
            "sframe-key",
            RtpTransceiverDirection::SendReceive,
        )
        .unwrap();
    let receiver = session.receiver(transceiver).unwrap();
    let decrypter = session
        .create_sframe_decrypter(receiver, SframeCipherSuite::Aes128GcmSha256_128)
        .unwrap();
    let key = [0x5a; 16];
    let mut add = session
        .sframe_add_decryption_key(decrypter, 7, &key)
        .unwrap();
    let add_result = (0..64).find_map(|_| {
        assert!(
            !world.pump(256).blocked,
            "controlled SFrame add-key pump blocked"
        );
        add.try_take()
    });
    add_result
        .expect("native SFrame add-key callback did not arrive within 64 bounded pumps")
        .unwrap();

    let mut remove = session.sframe_remove_decryption_key(decrypter, 7).unwrap();
    let remove_result = (0..64).find_map(|_| {
        assert!(
            !world.pump(256).blocked,
            "controlled SFrame remove-key pump blocked"
        );
        remove.try_take()
    });
    remove_result
        .expect("native SFrame remove-key callback did not arrive within 64 bounded pumps")
        .unwrap();
    session.release_sframe_decrypter(decrypter).unwrap();
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
fn stopped(error: SframeCreateError, expected_message: &str) {
    match error {
        SframeCreateError::Native(error) => {
            assert_eq!(error.kind(), RtcErrorType::InvalidState);
            assert_eq!(error.message(), expected_message);
        }
        error => panic!("expected native stopped-endpoint error, got {error:?}"),
    }
}
#[test]
fn surviving_endpoint_and_queued_remove_receiver_retain_sdk_transceiver_after_negotiation_removal()
{
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
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
    stopped(
        sender.create_sframe_encrypter(&init).unwrap_err(),
        "Cannot create an Sframe encryptor on a stopped sender.",
    );
    stopped(
        receiver
            .create_sframe_decrypter(init.cipher_suite())
            .unwrap_err(),
        "Cannot create an Sframe decryptor on a stopped receiver.",
    );
    // OnRemoveTrack retained the owner while it was still in the SDK list.
    assert!(!removed.is_empty());
    for receiver in removed {
        stopped(
            receiver
                .create_sframe_decrypter(init.cipher_suite())
                .unwrap_err(),
            "Cannot create an Sframe decryptor on a stopped receiver.",
        );
    }
}
