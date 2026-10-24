//! Real SDK caller factory virtual dispatch, not installed-consumer evidence.
use pulsebeam_webrtc_sys::{
    AudioCodecInfo, AudioCodecPairId, AudioCodecSpec, AudioEncoder, AudioEncoderCreationContext,
    AudioEncoderFactory, AudioEncoderFactoryProvider, AudioEncoderFactoryRegistration,
    AudioFactoryError, AudioFactoryFailureKind, AudioFactoryOperation, CodecError, Environment,
    ManualClock, OwnedAudioEncoderOptions, SdpAudioFormat,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
fn pcm() -> SdpAudioFormat {
    SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1)
}
fn raw_spec() -> AudioCodecSpec {
    let mut format = SdpAudioFormat::new(vec![0xff, 0, b'x'], i32::MIN, usize::MAX);
    format.parameters.insert(vec![0xfe, 0], vec![0xfd, 0]);
    let mut info = AudioCodecInfo::new(8000, 1, 64000).unwrap();
    info.sample_rate_hz = i32::MIN;
    info.num_channels = usize::MAX;
    info.default_bitrate_bps = i32::MIN;
    info.min_bitrate_bps = i32::MAX;
    info.max_bitrate_bps = -1;
    info.allow_comfort_noise = false;
    info.supports_network_adaption = true;
    AudioCodecSpec { format, info }
}
#[derive(Clone, Debug, Eq, PartialEq)]
enum Called {
    Discovery,
    Query,
    Create,
}
struct Provider {
    calls: Arc<Mutex<Vec<Called>>>,
    contexts: Arc<Mutex<Vec<AudioEncoderCreationContext>>>,
    queries: usize,
}
impl AudioEncoderFactoryProvider for Provider {
    fn supported_encoders(&mut self) -> Vec<AudioCodecSpec> {
        self.calls.lock().unwrap().push(Called::Discovery);
        vec![raw_spec()]
    }
    fn query_encoder(&mut self, format: &SdpAudioFormat) -> Option<AudioCodecInfo> {
        self.calls.lock().unwrap().push(Called::Query);
        if format.name == b"panic-query" {
            panic!("query callback");
        }
        self.queries += 1;
        (self.queries == 1).then(|| raw_spec().info)
    }
    fn create(
        &mut self,
        context: AudioEncoderCreationContext,
    ) -> Result<Option<AudioEncoder>, CodecError> {
        self.calls.lock().unwrap().push(Called::Create);
        self.contexts.lock().unwrap().push(context.clone());
        match context.format.name.as_slice() {
            b"none" => Ok(None),
            b"error" => Err(CodecError::ConstructionFailed),
            b"panic" => panic!("create callback"),
            // Deliberately different from caller declaration/request metadata.
            // Product provenance must come from the actual returned native PCM.
            _ => AudioEncoderFactory::builtin().unwrap().create_with_options(
                &context.environment,
                &pcm(),
                &context.options,
            ),
        }
    }
}
fn registration() -> (
    AudioEncoderFactoryRegistration,
    Arc<Mutex<Vec<Called>>>,
    Arc<Mutex<Vec<AudioEncoderCreationContext>>>,
) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let contexts = Arc::new(Mutex::new(Vec::new()));
    (
        AudioEncoderFactoryRegistration::from_provider(Provider {
            calls: calls.clone(),
            contexts: contexts.clone(),
            queries: 0,
        }),
        calls,
        contexts,
    )
}
#[test]
fn real_sdk_discovery_and_mutable_query_preserve_bytes_and_some_to_none() {
    let (registration, calls, _) = registration();
    let spec = raw_spec();
    assert_eq!(
        registration.supported_encoders_via_sdk().unwrap(),
        vec![spec.clone()]
    );
    assert_eq!(
        registration.query_encoder_via_sdk(&spec.format).unwrap(),
        Some(spec.info)
    );
    assert_eq!(
        registration.query_encoder_via_sdk(&spec.format).unwrap(),
        None
    );
    assert_eq!(
        *calls.lock().unwrap(),
        vec![Called::Discovery, Called::Query, Called::Query]
    );
    assert_eq!(registration.failure_observer().failure(), None);
    // Standalone SDK Query can reject after success without cached substitution.
    // This behavior is NOT admitted into the unsafe voice-engine consumer #37.
}
#[test]
fn real_sdk_create_uses_exact_options_and_retains_actual_product_after_factory_drop() {
    let (registration, calls, contexts) = registration();
    let clock = ManualClock::new(Duration::from_millis(13)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let id = AudioCodecPairId::create();
    let mut options = OwnedAudioEncoderOptions::default();
    options.set_payload_type(i32::MAX);
    options.set_codec_pair_id(Some(&id));
    // Even an Opus-named request is not proof of its returned product's type.
    let format = SdpAudioFormat::new(b"opus".to_vec(), i32::MIN, usize::MAX);
    let encoder = registration
        .create_via_sdk(&environment, &format, &options)
        .unwrap()
        .unwrap();
    assert_eq!(*calls.lock().unwrap(), vec![Called::Create]); // No implicit query.
    assert_eq!(encoder.sample_rate_hz(), 8000);
    assert_eq!(encoder.channels(), 1);
    // PCM has no Opus multiplier/clock guard. Preserve its actual no-op control.
    encoder
        .on_received_uplink_packet_loss_fraction(f32::NAN)
        .unwrap();
    let observer = encoder.sdk_failure_observer();
    options.set_payload_type(-1);
    options.set_codec_pair_id(None);
    let context = contexts.lock().unwrap().remove(0);
    assert_eq!(context.options.codec_pair_id().unwrap(), id);
    assert_eq!(context.options.payload_type(), i32::MAX);
    drop(registration);
    drop(environment);
    drop(clock);
    drop(options);
    drop(id);
    std::thread::spawn(move || {
        assert_eq!(context.format, format);
        assert_eq!(
            context.environment.timestamp().microseconds().unwrap(),
            13000
        );
        assert!(encoder.encode(100, &[0; 80]).unwrap().bytes.is_empty());
        let output = encoder.encode(180, &[0; 80]).unwrap();
        assert_eq!(output.info.primary.payload_type, i32::MAX);
        assert_eq!(output.info.primary.encoded_timestamp, 100);
        assert_eq!(output.bytes.len(), 160);
        assert_eq!(observer.failure(), None);
    })
    .join()
    .unwrap();
}
#[test]
fn native_null_rejection_and_panic_have_distinct_per_operation_results() {
    let (registration, calls, _) = registration();
    let environment = Environment::builder().build().unwrap();
    let options = OwnedAudioEncoderOptions::default();
    let none = SdpAudioFormat::new(b"none".to_vec(), i32::MIN, usize::MAX);
    assert!(
        registration
            .create_via_sdk(&environment, &none, &options)
            .unwrap()
            .is_none()
    );
    assert_eq!(registration.failure_observer().failure(), None);
    let error = SdpAudioFormat::new(b"error".to_vec(), 0, 0);
    assert_eq!(
        registration
            .create_via_sdk(&environment, &error, &options)
            .err(),
        Some(AudioFactoryError {
            operation: AudioFactoryOperation::CreateEncoder,
            kind: AudioFactoryFailureKind::Provider(CodecError::ConstructionFailed),
        })
    );
    assert!(
        registration
            .create_via_sdk(&environment, &pcm(), &options)
            .unwrap()
            .is_some()
    );
    let panic_format = SdpAudioFormat::new(b"panic".to_vec(), 0, 0);
    assert_eq!(
        registration
            .create_via_sdk(&environment, &panic_format, &options)
            .err(),
        Some(AudioFactoryError {
            operation: AudioFactoryOperation::CreateEncoder,
            kind: AudioFactoryFailureKind::Panicked,
        })
    );
    let before = calls.lock().unwrap().len();
    assert_eq!(
        registration.query_encoder_via_sdk(&pcm()).err(),
        Some(AudioFactoryError {
            operation: AudioFactoryOperation::QueryEncoder,
            kind: AudioFactoryFailureKind::Disabled,
        })
    );
    assert_eq!(
        registration.supported_encoders_via_sdk().err(),
        Some(AudioFactoryError {
            operation: AudioFactoryOperation::SupportedEncoders,
            kind: AudioFactoryFailureKind::Disabled,
        })
    );
    assert_eq!(calls.lock().unwrap().len(), before);
}
struct Shared {
    encoder: AudioEncoder,
}
impl AudioEncoderFactoryProvider for Shared {
    fn supported_encoders(&mut self) -> Vec<AudioCodecSpec> {
        vec![]
    }
    fn query_encoder(&mut self, _: &SdpAudioFormat) -> Option<AudioCodecInfo> {
        None
    }
    fn create(
        &mut self,
        _: AudioEncoderCreationContext,
    ) -> Result<Option<AudioEncoder>, CodecError> {
        Ok(Some(self.encoder.clone()))
    }
}
#[test]
fn sdk_product_refuses_shared_mutable_encoder_without_resetting_original() {
    let environment = Environment::builder().build().unwrap();
    let mut options = OwnedAudioEncoderOptions::default();
    options.set_payload_type(200);
    let encoder = AudioEncoderFactory::builtin()
        .unwrap()
        .create_with_options(&environment, &pcm(), &options)
        .unwrap()
        .unwrap();
    assert!(encoder.encode(100, &[0; 80]).unwrap().bytes.is_empty());
    let registration = AudioEncoderFactoryRegistration::from_provider(Shared {
        encoder: encoder.clone(),
    });
    assert_eq!(
        registration
            .create_via_sdk(&environment, &pcm(), &options)
            .err(),
        Some(AudioFactoryError {
            operation: AudioFactoryOperation::CreateEncoder,
            kind: AudioFactoryFailureKind::ProductNotUnique,
        })
    );
    drop(registration);
    let output = encoder.encode(180, &[0; 80]).unwrap();
    assert_eq!(output.info.primary.encoded_timestamp, 100);
    assert_eq!(output.bytes.len(), 160);
}
#[test]
fn query_panic_is_not_reported_as_native_unsupported() {
    let (registration, _, _) = registration();
    let format = SdpAudioFormat::new(b"panic-query".to_vec(), 0, 0);
    let error = AudioFactoryError {
        operation: AudioFactoryOperation::QueryEncoder,
        kind: AudioFactoryFailureKind::Panicked,
    };
    assert_eq!(registration.query_encoder_via_sdk(&format), Err(error));
    assert_eq!(registration.failure_observer().failure(), Some(error));
    assert!(registration.failure_observer().is_disabled());
}
