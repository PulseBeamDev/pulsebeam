//! Actual SDK encoder product forwarding, not native factory/peer qualification.
use crate::{
    AudioEncoder, AudioEncoderError, AudioEncoderFactory, AudioEncoderSdkFailure,
    AudioEncoderSdkFailureObserver, AudioEncoderSdkOperation, Environment, FieldTrials,
    OwnedAudioEncoderOptions, SdpAudioFormat, ffi,
};

fn forward(source: AudioEncoder) -> cxx::UniquePtr<ffi::NativeAudioEncoder> {
    let transfer = match source.try_into_factory_transfer() {
        Ok(transfer) => transfer,
        Err(_) => panic!("unique encoder required"),
    };
    let native = ffi::audio_encoder_forward_product(Box::new(transfer));
    assert!(!native.is_null());
    native
}
fn create(
    factory: &AudioEncoderFactory,
    env: &Environment,
    format: &SdpAudioFormat,
    payload: i32,
) -> AudioEncoder {
    let mut options = OwnedAudioEncoderOptions::default();
    options.set_payload_type(payload);
    factory
        .create_with_options(env, format, &options)
        .unwrap()
        .unwrap()
}

#[test]
fn sdk_encode_preserves_existing_buffer_prefix_and_original_packet_accumulation() {
    let env = Environment::builder().build().unwrap();
    let factory = AudioEncoderFactory::builtin().unwrap();
    let format = SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1);
    let original = create(&factory, &env, &format, 200);
    let forwarded = forward(create(&factory, &env, &format, 200));
    let native = forwarded.as_ref().unwrap();
    let prefix = [0x11, 0x22, 0xff];
    for (timestamp, audio) in [(100, vec![0; 80]), (180, vec![123; 80])] {
        let reference = original.encode(timestamp, &audio).unwrap();
        let actual = ffi::audio_encoder_forward_encode_sdk(native, timestamp, &audio, &prefix);
        assert_eq!(actual.status, 0);
        assert_eq!(&actual.bytes[..prefix.len()], &prefix);
        assert_eq!(&actual.bytes[prefix.len()..], &reference.bytes);
        assert_eq!(
            actual.primary.encoded_bytes,
            reference.info.primary.encoded_bytes
        );
        assert_eq!(
            actual.primary.encoded_timestamp,
            reference.info.primary.encoded_timestamp
        );
        assert_eq!(
            actual.primary.payload_type,
            reference.info.primary.payload_type
        );
        assert_eq!(
            actual.primary.send_even_if_empty,
            reference.info.primary.send_even_if_empty
        );
        assert_eq!(actual.primary.speech, reference.info.primary.speech);
        assert_eq!(
            actual.primary.encoder_type,
            reference.info.primary.encoder_type.0
        );
        assert_eq!(actual.redundant.len(), reference.info.redundant.len());
    }
    assert_eq!(
        ffi::audio_encoder_forward_encode_sdk(native, 260, &[0; 79], &prefix).status,
        1
    );
    let observer = AudioEncoderSdkFailureObserver::from_native(
        ffi::audio_encoder_sdk_failure_observer(native),
    );
    assert_eq!(observer.failure(), None);
    // Restoring the Rust owner must preserve the exact guarded state, not make
    // a fresh codec or mistake the SDK envelope for an Opus implementation.
    let restored = AudioEncoder::from_native(forwarded, env.clone());
    assert_eq!(restored.sample_rate_hz(), 8000);
    assert_eq!(restored.channels(), 1);
    assert_eq!(
        restored.rtp_timestamp_rate_hz(),
        original.rtp_timestamp_rate_hz()
    );
    assert_eq!(restored.bitrate_range(), original.bitrate_range());
    assert_eq!(restored.frame_length_range(), original.frame_length_range());
    assert_eq!(
        restored.network_adaptation_statistics(),
        original.network_adaptation_statistics()
    );
    assert!(restored.encode(260, &[0; 80]).unwrap().bytes.is_empty());
    let twice = forward(restored);
    let output =
        ffi::audio_encoder_forward_encode_sdk(twice.as_ref().unwrap(), 340, &[0; 80], &prefix);
    assert_eq!(output.status, 0);
    assert_eq!(output.primary.encoded_timestamp, 260);
    assert_eq!(output.primary.payload_type, 200);
    assert_eq!(output.primary.encoded_bytes, 160);
    drop(twice);
    drop(original);
    drop(factory);
    drop(env);
    assert_eq!(observer.failure(), None);
}

#[test]
fn sdk_void_controls_keep_actual_opus_guards_and_first_failure_after_retirement() {
    let trials = FieldTrials::parse("WebRTC-Audio-OpusBitrateMultipliers/Enabled-1-1e35/").unwrap();
    let env = Environment::builder().field_trials(trials).build().unwrap();
    let factory = AudioEncoderFactory::builtin().unwrap();
    let format = SdpAudioFormat::new(b"opus".to_vec(), 48000, 2);
    let forwarded = forward(create(&factory, &env, &format, 111));
    let native = forwarded.as_ref().unwrap();
    let observer = AudioEncoderSdkFailureObserver::from_native(
        ffi::audio_encoder_sdk_failure_observer(native),
    );
    let original_bitrate = ffi::audio_encoder_info(native).target_bitrate_bps;
    ffi::audio_encoder_forward_target_sdk(native, 6000);
    let failure = AudioEncoderSdkFailure {
        operation: AudioEncoderSdkOperation::TARGET_AUDIO_BITRATE,
        error: AudioEncoderError::InvalidControl,
    };
    assert_eq!(observer.failure(), Some(failure));
    assert_eq!(
        ffi::audio_encoder_info(native).target_bitrate_bps,
        original_bitrate
    );
    // Public Rust checks must still use the original Opus implementation and
    // multipliers; a proxy cannot silently bypass that operation-local guard.
    assert!(!ffi::audio_encoder_set_target_bitrate(native, 6000));
    ffi::audio_encoder_forward_loss_sdk(native, f32::NAN);
    assert_eq!(observer.failure(), Some(failure));
    assert!(!ffi::audio_encoder_forward_adaptor_sdk(
        native,
        b"arbitrary\0\xff"
    ));
    assert_eq!(observer.failure(), Some(failure));
    let clone = observer.clone();
    drop(forwarded);
    drop(factory);
    drop(env);
    std::thread::spawn(move || assert_eq!(clone.failure(), Some(failure)))
        .join()
        .unwrap();
}

#[test]
fn missing_clock_and_pending_ana_admission_are_not_silent_sdk_success() {
    // A default system environment now retains its actual safe clock proof.
    // Use an explicitly unbounded, non-monotonic provider for this refusal.
    struct UnprovenClock;
    impl crate::ClockProvider for UnprovenClock {
        fn current_time(&self) -> crate::FiniteTimestamp {
            crate::FiniteTimestamp::from_microseconds(0).unwrap()
        }
        fn convert_timestamp_to_ntp_time(&self, _: crate::Timestamp) -> crate::NtpTime {
            crate::NtpTime::from_raw(0)
        }
    }
    let clock = crate::Clock::from_provider(UnprovenClock);
    let env = Environment::builder().clock_handle(&clock).build().unwrap();
    let factory = AudioEncoderFactory::builtin().unwrap();
    let format = SdpAudioFormat::new(b"opus".to_vec(), 48000, 2);
    let loss = forward(create(&factory, &env, &format, 111));
    let observer = AudioEncoderSdkFailureObserver::from_native(
        ffi::audio_encoder_sdk_failure_observer(loss.as_ref().unwrap()),
    );
    ffi::audio_encoder_forward_loss_sdk(loss.as_ref().unwrap(), 0.1);
    assert_eq!(
        observer.failure(),
        Some(AudioEncoderSdkFailure {
            operation: AudioEncoderSdkOperation::PACKET_LOSS,
            error: AudioEncoderError::UnsafeClock,
        })
    );
    let ana = forward(create(&factory, &env, &format, 111));
    let observer = AudioEncoderSdkFailureObserver::from_native(
        ffi::audio_encoder_sdk_failure_observer(ana.as_ref().unwrap()),
    );
    assert!(!ffi::audio_encoder_forward_adaptor_sdk(
        ana.as_ref().unwrap(),
        b""
    ));
    assert_eq!(
        observer.failure(),
        Some(AudioEncoderSdkFailure {
            operation: AudioEncoderSdkOperation::ENABLE_ADAPTOR,
            error: AudioEncoderError::UnsafeClock,
        })
    );
    // The actual underlying encoder continues to produce real codec output;
    // rejection does not reset it or inject synthetic EncodedInfo.
    let shape = ffi::audio_encoder_info(ana.as_ref().unwrap());
    let audio = vec![0; shape.sample_rate_hz as usize / 100 * shape.num_channels];
    let first = ffi::audio_encoder_forward_encode_sdk(ana.as_ref().unwrap(), 100, &audio, &[]);
    assert_eq!(first.status, 0);
    let second = ffi::audio_encoder_forward_encode_sdk(ana.as_ref().unwrap(), 580, &audio, &[]);
    assert_eq!(second.status, 0);
    assert!(!second.bytes.is_empty());
}
