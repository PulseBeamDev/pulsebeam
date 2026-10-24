//! Source fixtures for native frame crypto providers and captive identities, unrun.
use pulsebeam_webrtc_sys::*;
use std::sync::{Arc, atomic::Ordering};
use std::thread;
#[path = "support/frame_crypto.rs"]
mod support;
use support::{Cipher, Trace};

fn send_sync<T: Send + Sync>() {}
#[test]
fn native_decryption_results_preserve_full_status_size_domain_and_is_ok() {
    send_sync::<FrameDecryptionResult>();
    for status in [
        FrameDecryptionStatus::Ok,
        FrameDecryptionStatus::Recoverable,
        FrameDecryptionStatus::FailedToDecrypt,
        FrameDecryptionStatus::Unknown,
        FrameDecryptionStatus::Other(i32::MIN),
        FrameDecryptionStatus::Other(i32::MAX),
    ] {
        assert_eq!(FrameDecryptionStatus::from_raw(status.raw()), status);
        let original = FrameDecryptionResult::new(status, usize::MAX);
        let copy = original.clone();
        drop(original);
        let copy = thread::spawn(move || {
            assert_eq!(copy.status(), status);
            assert_eq!(copy.bytes_written(), usize::MAX);
            assert_eq!(copy.is_ok(), status == FrameDecryptionStatus::Ok);
            copy
        })
        .join()
        .unwrap();
        assert_eq!(copy.bytes_written(), usize::MAX);
        let mut assigned = FrameDecryptionResult::new(FrameDecryptionStatus::Ok, 0);
        assigned.assign(&copy);
        assert_eq!(assigned.status(), status);
        assert_eq!(assigned.bytes_written(), usize::MAX);
        assigned.set_status(FrameDecryptionStatus::Other(i32::MIN));
        assigned.set_bytes_written(0);
        assert_eq!(assigned.status(), FrameDecryptionStatus::Other(i32::MIN));
        assert_eq!(assigned.bytes_written(), 0);
        assert!(!assigned.is_ok());
        assert_eq!(copy.status(), status);
        assert_eq!(copy.bytes_written(), usize::MAX);
    }
}

#[test]
fn encryptor_sdk_aliases_retain_provider_and_preserve_callback_inputs() {
    send_sync::<FrameEncryptor>();
    let trace = Arc::new(Trace::default());
    let encryptor = FrameEncryptor::from_provider(Cipher::encoder(trace.clone()));
    let alias = encryptor.clone();
    assert!(encryptor.same_resource(&alias));
    drop(encryptor);
    assert_eq!(trace.encoder_drops.load(Ordering::SeqCst), 0);
    assert!(matches!(
        alias.encrypt(MediaType::Audio, 1, &[], &[], usize::MAX),
        Err(FrameCryptoOperationError::OutputCapacityTooLarge {
            requested: usize::MAX,
            ..
        })
    ));
    assert_eq!(alias.failure(), None);
    assert!(trace.calls.lock().unwrap().is_empty());
    thread::spawn(move || {
        assert_eq!(
            alias.max_ciphertext_byte_size(MediaType::Other(i32::MIN), 3),
            4
        );
        let value = alias
            .encrypt(
                MediaType::Other(i32::MIN),
                u32::MAX,
                b"ad\0\xff",
                &[0, 127, 255],
                4,
            )
            .unwrap();
        assert_eq!(
            value.result,
            FrameEncryptionResult {
                status: 0,
                bytes_written: 4
            }
        );
        assert_eq!(value.frame, vec![0xa5, 0x5a, 127 ^ 0x5a, 255 ^ 0x5a]);
        let short = alias.encrypt(MediaType::Audio, 1, &[], &[0], 0).unwrap();
        assert_eq!(short.result.status, -57);
        assert_eq!(short.result.bytes_written, 0);
        assert!(short.frame.is_empty());
        assert_eq!(alias.failure(), None);
    })
    .join()
    .unwrap();
    assert_eq!(trace.encoder_drops.load(Ordering::SeqCst), 1);
    let calls = trace.calls.lock().unwrap();
    assert_eq!(calls[0].media, MediaType::Other(i32::MIN));
    assert_eq!(calls[0].ssrc, u32::MAX);
    assert_eq!(calls[0].additional, b"ad\0\xff");
    assert_eq!(calls[0].input, [0, 127, 255]);
}

#[derive(Clone, Copy)]
enum Fault {
    SizePanic,
    CallbackPanic,
    BadWritten,
    HugeSize,
    ZeroFilledPrefix,
    Status(i32),
}
struct Bad(Fault);
impl FrameEncryptorProvider for Bad {
    fn max_ciphertext_byte_size(&self, _: MediaType, size: usize) -> usize {
        match self.0 {
            Fault::SizePanic => panic!("max-size failure"),
            Fault::HugeSize => usize::MAX,
            _ => size,
        }
    }
    fn encrypt(
        &self,
        _: MediaType,
        _: u32,
        _: &[u8],
        _: &[u8],
        output: &mut [u8],
    ) -> FrameEncryptionResult {
        match self.0 {
            Fault::CallbackPanic => panic!("encrypt failure"),
            Fault::BadWritten => FrameEncryptionResult {
                status: 0,
                bytes_written: output.len() + 1,
            },
            Fault::ZeroFilledPrefix => {
                output[0] = 7;
                FrameEncryptionResult {
                    status: 0,
                    bytes_written: output.len(),
                }
            }
            Fault::Status(status) => FrameEncryptionResult {
                status,
                bytes_written: 0,
            },
            _ => FrameEncryptionResult {
                status: 0,
                bytes_written: 0,
            },
        }
    }
}
impl FrameDecryptorProvider for Bad {
    fn max_plaintext_byte_size(&self, _: MediaType, size: usize) -> usize {
        match self.0 {
            Fault::SizePanic => panic!("max-size failure"),
            Fault::HugeSize => usize::MAX,
            _ => size,
        }
    }
    fn decrypt(
        &self,
        _: MediaType,
        _: &[u32],
        _: &[u8],
        _: &[u8],
        output: &mut [u8],
    ) -> FrameDecryptionResult {
        match self.0 {
            Fault::CallbackPanic => panic!("decrypt failure"),
            Fault::BadWritten => {
                FrameDecryptionResult::new(FrameDecryptionStatus::Ok, output.len() + 1)
            }
            Fault::Status(status) => {
                FrameDecryptionResult::new(FrameDecryptionStatus::Other(status), 0)
            }
            _ => FrameDecryptionResult::new(FrameDecryptionStatus::Ok, 0),
        }
    }
}
#[test]
fn unsafe_provider_results_poison_without_silent_success_or_size_clamping() {
    let panic = FrameEncryptor::from_provider(Bad(Fault::SizePanic));
    assert_eq!(panic.max_ciphertext_byte_size(MediaType::Video, 1), 0);
    assert_eq!(panic.failure(), Some(FrameCryptoFailure::ProviderPanicked));
    assert_ne!(
        panic
            .encrypt(MediaType::Video, 0, &[], &[1], 1)
            .unwrap()
            .result
            .status,
        0
    );
    let panic = FrameEncryptor::from_provider(Bad(Fault::CallbackPanic));
    assert_ne!(
        panic
            .encrypt(MediaType::Audio, 0, &[], &[1], 1)
            .unwrap()
            .result
            .status,
        0
    );
    assert_eq!(panic.failure(), Some(FrameCryptoFailure::ProviderPanicked));
    let invalid = FrameEncryptor::from_provider(Bad(Fault::BadWritten));
    assert_ne!(
        invalid
            .encrypt(MediaType::Audio, 0, &[], &[1], 1)
            .unwrap()
            .result
            .status,
        0
    );
    assert_eq!(
        invalid.failure(),
        Some(FrameCryptoFailure::BytesWritten {
            bytes_written: 2,
            capacity: 1
        })
    );
    assert_eq!(
        FrameEncryptor::from_provider(Bad(Fault::HugeSize))
            .max_ciphertext_byte_size(MediaType::Video, 1),
        usize::MAX
    );
    let prefix = FrameEncryptor::from_provider(Bad(Fault::ZeroFilledPrefix))
        .encrypt(MediaType::Audio, 1, &[], &[2], 3)
        .unwrap();
    assert_eq!(prefix.frame, [7, 0, 0]);
    assert_eq!(prefix.result.bytes_written, 3);
    for status in [i32::MIN, -1, 1, i32::MAX] {
        let value = FrameEncryptor::from_provider(Bad(Fault::Status(status)))
            .encrypt(MediaType::Audio, 1, &[], &[], 0)
            .unwrap();
        assert_eq!(value.result.status, status);
    }
}

#[test]
fn actor_installs_native_providers_retains_released_aliases_and_binds_one_receiver() {
    let trace = Arc::new(Trace::default());
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .encoded_audio()
        .build()
        .unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    let source = session.create_opus_source(1).unwrap();
    let transceiver = session
        .publish_opus(peer, source, "crypto", RtpTransceiverDirection::SendReceive)
        .unwrap();
    let sender = session.sender(transceiver).unwrap();
    let receiver = session.receiver(transceiver).unwrap();
    let second_source = session.create_opus_source(1).unwrap();
    let second = session
        .publish_opus(
            peer,
            second_source,
            "crypto-second",
            RtpTransceiverDirection::SendReceive,
        )
        .unwrap();
    let other = session.receiver(second).unwrap();
    assert_eq!(session.sender_frame_encryptor(sender).unwrap(), None);
    assert_eq!(session.receiver_frame_decryptor(receiver).unwrap(), None);
    let encryptor = session
        .create_frame_encryptor(Cipher::encoder(trace.clone()))
        .unwrap();
    let decryptor = session
        .create_frame_decryptor(receiver, Cipher::decoder(trace.clone()))
        .unwrap();
    session
        .set_sender_frame_encryptor(sender, Some(encryptor))
        .unwrap();
    session
        .set_receiver_frame_decryptor(receiver, Some(decryptor))
        .unwrap();
    assert_eq!(
        session.sender_frame_encryptor(sender).unwrap(),
        Some(encryptor)
    );
    assert_eq!(
        session.receiver_frame_decryptor(receiver).unwrap(),
        Some(decryptor)
    );
    assert_eq!(
        session.set_receiver_frame_decryptor(other, Some(decryptor)),
        Err(FrameDecryptorInstallError::WrongReceiver)
    );
    assert_eq!(session.receiver_frame_decryptor(other).unwrap(), None);
    session
        .set_receiver_frame_decryptor(receiver, None)
        .unwrap();
    assert_eq!(
        session.set_receiver_frame_decryptor(other, Some(decryptor)),
        Err(FrameDecryptorInstallError::WrongReceiver)
    );
    session
        .set_receiver_frame_decryptor(receiver, Some(decryptor))
        .unwrap();
    session.release_frame_encryptor(encryptor).unwrap();
    session.release_frame_decryptor(decryptor).unwrap();
    assert_eq!(trace.encoder_drops.load(Ordering::SeqCst), 0);
    assert_eq!(trace.decoder_drops.load(Ordering::SeqCst), 0);
    assert!(session.frame_encryptor_failure(encryptor).is_err());
    let restored_encoder = session.sender_frame_encryptor(sender).unwrap().unwrap();
    let restored_decoder = session.receiver_frame_decryptor(receiver).unwrap().unwrap();
    assert_ne!(restored_encoder, encryptor);
    assert_ne!(restored_decoder, decryptor);
    let caller_thread = thread::current().id();
    let session = thread::spawn(move || {
        let mut session = session;
        let encrypted = session
            .encrypt_frame(
                restored_encoder,
                MediaType::Audio,
                17,
                b"aad\0",
                &[1, 2, 3],
                4,
            )
            .unwrap();
        assert_eq!(
            session
                .frame_decryptor_max_plaintext_byte_size(restored_decoder, MediaType::Video, 4)
                .unwrap(),
            3
        );
        let decrypted = session
            .decrypt_frame(
                restored_decoder,
                MediaType::Video,
                &[0, u32::MAX],
                b"aad\0",
                &encrypted.frame,
                3,
            )
            .unwrap();
        assert!(decrypted.result.is_ok());
        assert_eq!(decrypted.result.bytes_written(), 3);
        assert_eq!(decrypted.frame, [1, 2, 3]);
        session.set_sender_frame_encryptor(sender, None).unwrap();
        session
            .set_receiver_frame_decryptor(receiver, None)
            .unwrap();
        session.release_frame_encryptor(restored_encoder).unwrap();
        session.release_frame_decryptor(restored_decoder).unwrap();
        session
    })
    .join()
    .unwrap();
    assert_eq!(trace.encoder_drops.load(Ordering::SeqCst), 1);
    assert_eq!(trace.decoder_drops.load(Ordering::SeqCst), 1);
    let calls = trace.calls.lock().unwrap();
    assert!(calls.iter().all(|call| call.thread != caller_thread));
    assert_eq!(calls[1].csrcs, [0, u32::MAX]);
    assert_eq!(calls[1].additional, b"aad\0");
    drop(calls);
    drop(session);
}

#[test]
fn actor_video_capacity_guard_and_decrypt_callback_failure_preserve_sdk_status_domain() {
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
            "decrypt-fault",
            RtpTransceiverDirection::SendReceive,
        )
        .unwrap();
    let receiver = session.receiver(transceiver).unwrap();
    let huge = session
        .create_frame_decryptor(receiver, Bad(Fault::HugeSize))
        .unwrap();
    assert!(matches!(
        session.decrypt_frame(huge, MediaType::Video, &[], &[], &[], usize::MAX),
        Err(FrameCryptoOperationError::OutputCapacityTooLarge {
            requested: usize::MAX,
            ..
        })
    ));
    assert_eq!(session.frame_decryptor_failure(huge).unwrap(), None);
    assert_eq!(
        session
            .frame_decryptor_max_plaintext_byte_size(huge, MediaType::Audio, 1)
            .unwrap(),
        usize::MAX
    );
    assert_eq!(
        session
            .frame_decryptor_max_plaintext_byte_size(huge, MediaType::Video, 1)
            .unwrap(),
        0
    );
    assert_eq!(
        session.frame_decryptor_failure(huge).unwrap(),
        Some(FrameCryptoFailure::VideoPlaintextCapacity {
            frame_size: 1,
            capacity: usize::MAX
        })
    );
    assert!(
        !session
            .decrypt_frame(huge, MediaType::Video, &[], &[], &[1], 0)
            .unwrap()
            .result
            .is_ok()
    );
    for fault in [Fault::CallbackPanic, Fault::BadWritten, Fault::SizePanic] {
        let decryptor = session
            .create_frame_decryptor(receiver, Bad(fault))
            .unwrap();
        if matches!(fault, Fault::SizePanic) {
            session
                .frame_decryptor_max_plaintext_byte_size(decryptor, MediaType::Audio, 1)
                .unwrap();
        }
        assert!(
            !session
                .decrypt_frame(decryptor, MediaType::Audio, &[], &[], &[1], 1)
                .unwrap()
                .result
                .is_ok()
        );
        assert!(
            session
                .frame_decryptor_failure(decryptor)
                .unwrap()
                .is_some()
        );
    }
    for status in [i32::MIN, 0, 1, 2, 3, i32::MAX] {
        let decryptor = session
            .create_frame_decryptor(receiver, Bad(Fault::Status(status)))
            .unwrap();
        let result = session
            .decrypt_frame(decryptor, MediaType::Audio, &[], &[], &[], 0)
            .unwrap()
            .result;
        assert_eq!(
            result.status(),
            match status {
                0 => FrameDecryptionStatus::Ok,
                1 => FrameDecryptionStatus::Recoverable,
                2 => FrameDecryptionStatus::FailedToDecrypt,
                3 => FrameDecryptionStatus::Unknown,
                value => FrameDecryptionStatus::Other(value),
            }
        );
        assert_eq!(result.is_ok(), status == 0);
        assert_eq!(session.frame_decryptor_failure(decryptor).unwrap(), None);
    }
}

struct PanickingDrop;
impl Drop for PanickingDrop {
    fn drop(&mut self) {
        panic!("provider destructor");
    }
}
impl FrameEncryptorProvider for PanickingDrop {
    fn max_ciphertext_byte_size(&self, _: MediaType, size: usize) -> usize {
        size
    }
    fn encrypt(
        &self,
        _: MediaType,
        _: u32,
        _: &[u8],
        _: &[u8],
        _: &mut [u8],
    ) -> FrameEncryptionResult {
        FrameEncryptionResult {
            status: 0,
            bytes_written: 0,
        }
    }
}
#[test]
fn sdk_provider_destruction_contains_user_panic() {
    drop(FrameEncryptor::from_provider(PanickingDrop));
}
