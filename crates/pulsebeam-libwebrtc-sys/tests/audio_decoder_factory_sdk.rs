//! Actual SDK factory virtual dispatch, not peer installation qualification.
use pulsebeam_webrtc_sys::{
    AudioCodecInfo, AudioCodecPairId, AudioCodecSpec, AudioDecoder, AudioDecoderCreationContext,
    AudioDecoderFactory, AudioDecoderFactoryProvider, AudioDecoderFactoryRegistration,
    AudioFactoryError, AudioFactoryFailureKind, AudioFactoryOperation, CodecError, Environment,
    ManualClock, SdpAudioFormat,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

fn pcm() -> SdpAudioFormat {
    SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1)
}
fn declaration() -> AudioCodecSpec {
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
    Support,
    Current,
    Compatibility(Option<AudioCodecPairId>),
}
struct Provider {
    calls: Arc<Mutex<Vec<Called>>>,
    contexts: Arc<Mutex<Vec<AudioDecoderCreationContext>>>,
}
impl Provider {
    fn product(
        &mut self,
        context: AudioDecoderCreationContext,
    ) -> Result<Option<AudioDecoder>, CodecError> {
        self.contexts.lock().unwrap().push(context.clone());
        match context.format.name.as_slice() {
            b"none" => Ok(None),
            b"error" => Err(CodecError::ConstructionFailed),
            b"panic" => panic!("SDK Create callback"),
            // Deliberately construct a real decoder whose native origin differs
            // from the caller's discovery/request metadata. The wrapper must
            // preserve this actual product, not infer kind/rate/channels by name.
            _ => AudioDecoderFactory::builtin()
                .unwrap()
                .create(&context.environment, &pcm()),
        }
    }
}
impl AudioDecoderFactoryProvider for Provider {
    fn supported_decoders(&mut self) -> Vec<AudioCodecSpec> {
        self.calls.lock().unwrap().push(Called::Discovery);
        vec![declaration()]
    }
    fn is_supported_decoder(&mut self, _format: &SdpAudioFormat) -> bool {
        self.calls.lock().unwrap().push(Called::Support);
        false
    }
    fn create(
        &mut self,
        context: AudioDecoderCreationContext,
    ) -> Result<Option<AudioDecoder>, CodecError> {
        self.calls.lock().unwrap().push(Called::Current);
        self.product(context)
    }
    fn create_with_codec_pair_id(
        &mut self,
        context: AudioDecoderCreationContext,
        pair: Option<AudioCodecPairId>,
    ) -> Result<Option<AudioDecoder>, CodecError> {
        self.calls.lock().unwrap().push(Called::Compatibility(pair));
        self.product(context)
    }
}
fn registration() -> (
    AudioDecoderFactoryRegistration,
    Arc<Mutex<Vec<Called>>>,
    Arc<Mutex<Vec<AudioDecoderCreationContext>>>,
) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let contexts = Arc::new(Mutex::new(Vec::new()));
    (
        AudioDecoderFactoryRegistration::from_provider(Provider {
            calls: calls.clone(),
            contexts: contexts.clone(),
        }),
        calls,
        contexts,
    )
}

#[test]
fn real_sdk_factory_preserves_raw_discovery_overloads_and_actual_product_origin() {
    let (registration, calls, contexts) = registration();
    let observer = registration.failure_observer();
    let input = declaration();
    assert_eq!(
        registration.supported_decoders_via_sdk().unwrap(),
        vec![input.clone()]
    );
    assert!(
        !registration
            .is_supported_decoder_via_sdk(&input.format)
            .unwrap()
    );
    let clock = ManualClock::new(Duration::from_millis(9)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let id = AudioCodecPairId::create();
    let decoder = registration
        .create_via_sdk(&environment, &input.format)
        .unwrap()
        .unwrap();
    let compatible = registration
        .create_with_codec_pair_id_via_sdk(&environment, &input.format, Some(&id))
        .unwrap()
        .unwrap();
    assert!(
        registration
            .create_with_codec_pair_id_via_sdk(&environment, &input.format, None)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            Called::Discovery,
            Called::Support,
            Called::Current,
            Called::Compatibility(Some(id.clone())),
            Called::Compatibility(None)
        ]
    );
    // No stock-parser preflight on arbitrary caller metadata, no implicit
    // support query and no fabricated backend association from the name.
    assert_eq!(decoder.sample_rate_hz(), 8000);
    assert_eq!(decoder.channels(), 1);
    assert_eq!(compatible.sample_rate_hz(), 8000);
    let parsed = decoder.parse_payload(vec![0xff; 160], 123).unwrap();
    assert_eq!(parsed[0].timestamp, 123);
    assert_eq!(parsed[0].frame.as_ref().unwrap().duration(), 160);
    let context = contexts.lock().unwrap().remove(0);
    contexts.lock().unwrap().clear();
    drop(registration);
    drop(environment);
    drop(clock);
    drop(id);
    drop(compatible);
    drop(decoder);
    assert_eq!(observer.failure(), None);
    std::thread::spawn(move || {
        assert_eq!(context.format, input.format);
        assert_eq!(
            context.environment.timestamp().microseconds().unwrap(),
            9000
        );
        let output = parsed[0].frame.as_ref().unwrap().decode().unwrap().unwrap();
        assert_eq!(output.samples, vec![0; 160]);
    })
    .join()
    .unwrap();
}

#[test]
fn native_null_is_unsupported_but_rejection_and_panic_are_typed_per_request() {
    let (registration, calls, _) = registration();
    let environment = Environment::builder().build().unwrap();
    let none = SdpAudioFormat::new(b"none".to_vec(), i32::MIN, usize::MAX);
    assert!(
        registration
            .create_via_sdk(&environment, &none)
            .unwrap()
            .is_none()
    );
    assert_eq!(registration.failure_observer().failure(), None);
    let error = SdpAudioFormat::new(b"error".to_vec(), 0, 0);
    assert_eq!(
        registration.create_via_sdk(&environment, &error).err(),
        Some(AudioFactoryError {
            operation: AudioFactoryOperation::CreateDecoder,
            kind: AudioFactoryFailureKind::Provider(CodecError::ConstructionFailed),
        })
    );
    assert!(
        registration
            .create_via_sdk(&environment, &pcm())
            .unwrap()
            .is_some()
    );
    assert!(!registration.failure_observer().is_disabled());
    let panic_format = SdpAudioFormat::new(b"panic".to_vec(), 0, 0);
    assert_eq!(
        registration
            .create_with_codec_pair_id_via_sdk(&environment, &panic_format, None)
            .err(),
        Some(AudioFactoryError {
            operation: AudioFactoryOperation::CreateDecoderWithCodecPairId,
            kind: AudioFactoryFailureKind::Panicked,
        })
    );
    assert!(registration.failure_observer().is_disabled());
    let before = calls.lock().unwrap().len();
    assert_eq!(
        registration.is_supported_decoder_via_sdk(&pcm()),
        Err(AudioFactoryError {
            operation: AudioFactoryOperation::IsSupportedDecoder,
            kind: AudioFactoryFailureKind::Disabled,
        })
    );
    assert_eq!(
        registration.supported_decoders_via_sdk().err(),
        Some(AudioFactoryError {
            operation: AudioFactoryOperation::SupportedDecoders,
            kind: AudioFactoryFailureKind::Disabled,
        })
    );
    assert_eq!(before, calls.lock().unwrap().len());
}

struct SharedProduct {
    decoder: AudioDecoder,
}
impl AudioDecoderFactoryProvider for SharedProduct {
    fn supported_decoders(&mut self) -> Vec<AudioCodecSpec> {
        vec![]
    }
    fn is_supported_decoder(&mut self, _: &SdpAudioFormat) -> bool {
        true
    }
    fn create(
        &mut self,
        _: AudioDecoderCreationContext,
    ) -> Result<Option<AudioDecoder>, CodecError> {
        Ok(Some(self.decoder.clone()))
    }
}
#[test]
fn sdk_unique_product_handoff_refuses_reuse_without_cloning_or_destroying_original() {
    let environment = Environment::builder().build().unwrap();
    let original = AudioDecoderFactory::builtin()
        .unwrap()
        .create(&environment, &pcm())
        .unwrap()
        .unwrap();
    let registration = AudioDecoderFactoryRegistration::from_provider(SharedProduct {
        decoder: original.clone(),
    });
    assert_eq!(
        registration.create_via_sdk(&environment, &pcm()).err(),
        Some(AudioFactoryError {
            operation: AudioFactoryOperation::CreateDecoder,
            kind: AudioFactoryFailureKind::ProductNotUnique,
        })
    );
    assert!(!registration.failure_observer().is_disabled());
    drop(registration);
    let parsed = original.parse_payload(vec![0xff; 160], 0).unwrap();
    assert_eq!(
        parsed[0]
            .frame
            .as_ref()
            .unwrap()
            .decode()
            .unwrap()
            .unwrap()
            .samples,
        vec![0; 160]
    );
}

struct LegacyOnly {
    pairs: Arc<Mutex<Vec<Option<AudioCodecPairId>>>>,
}
impl AudioDecoderFactoryProvider for LegacyOnly {
    fn supported_decoders(&mut self) -> Vec<AudioCodecSpec> {
        vec![]
    }
    fn is_supported_decoder(&mut self, _: &SdpAudioFormat) -> bool {
        false
    }
    fn create_with_codec_pair_id(
        &mut self,
        context: AudioDecoderCreationContext,
        pair: Option<AudioCodecPairId>,
    ) -> Result<Option<AudioDecoder>, CodecError> {
        self.pairs.lock().unwrap().push(pair);
        AudioDecoderFactory::builtin()
            .unwrap()
            .create(&context.environment, &pcm())
    }
}
#[test]
fn sdk_current_dispatch_preserves_legacy_only_default_delegation_and_pair_identity() {
    let pairs = Arc::new(Mutex::new(Vec::new()));
    let registration = AudioDecoderFactoryRegistration::from_provider(LegacyOnly {
        pairs: pairs.clone(),
    });
    let environment = Environment::builder().build().unwrap();
    let id = AudioCodecPairId::create();
    assert!(
        registration
            .create_via_sdk(&environment, &pcm())
            .unwrap()
            .is_some()
    );
    assert!(
        registration
            .create_with_codec_pair_id_via_sdk(&environment, &pcm(), Some(&id))
            .unwrap()
            .is_some()
    );
    assert_eq!(*pairs.lock().unwrap(), vec![None, Some(id)]);
}
