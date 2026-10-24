//! Standalone dispatch fixtures, not SDK factory/peer installation qualification.
use pulsebeam_webrtc_sys::{
    AudioCodecInfo, AudioCodecPairId, AudioCodecSpec, AudioDecoder, AudioDecoderCreationContext,
    AudioDecoderFactory, AudioDecoderFactoryProvider, AudioDecoderFactoryRegistration,
    AudioEncoder, AudioEncoderCreationContext, AudioEncoderFactory, AudioEncoderFactoryProvider,
    AudioEncoderFactoryRegistration, AudioFactoryError, AudioFactoryFailureKind,
    AudioFactoryOperation, CodecError, Environment, ManualClock, OwnedAudioEncoderOptions,
    SdpAudioFormat,
};
use std::{
    cell::Cell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn format() -> SdpAudioFormat {
    SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1)
}
fn raw_spec() -> AudioCodecSpec {
    let mut format = SdpAudioFormat::new(vec![0xff, 0, b'x'], i32::MIN, usize::MAX);
    format.parameters.insert(vec![0xfe, 0], vec![0xfd, 0]);
    let mut info = AudioCodecInfo::new(8000, 1, 64000).unwrap();
    info.sample_rate_hz = i32::MIN;
    info.num_channels = usize::MAX;
    info.min_bitrate_bps = i32::MAX;
    info.default_bitrate_bps = i32::MIN;
    info.max_bitrate_bps = -1;
    AudioCodecSpec { format, info }
}
struct EncoderProvider {
    native: AudioEncoderFactory,
    queries: Arc<AtomicUsize>,
    contexts: Arc<Mutex<Vec<AudioEncoderCreationContext>>>,
    drops: Arc<AtomicUsize>,
}
impl Drop for EncoderProvider {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl AudioEncoderFactoryProvider for EncoderProvider {
    fn supported_encoders(&mut self) -> Vec<AudioCodecSpec> {
        vec![raw_spec()]
    }
    fn query_encoder(&mut self, _format: &SdpAudioFormat) -> Option<AudioCodecInfo> {
        // A real mutable query may reject after success. Standalone dispatch
        // preserves it; this is not admitted into the unsafe voice-engine path.
        (self.queries.fetch_add(1, Ordering::SeqCst) == 0).then(|| raw_spec().info)
    }
    fn create(
        &mut self,
        context: AudioEncoderCreationContext,
    ) -> Result<Option<AudioEncoder>, CodecError> {
        self.contexts.lock().unwrap().push(context.clone());
        match context.format.name.as_slice() {
            b"error" => Err(CodecError::ConstructionFailed),
            b"panic" => panic!("encoder factory callback"),
            _ => self.native.create_with_options(
                &context.environment,
                &context.format,
                &context.options,
            ),
        }
    }
}
fn encoder_registration() -> (
    AudioEncoderFactoryRegistration,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<AudioEncoderCreationContext>>>,
    Arc<AtomicUsize>,
) {
    let queries = Arc::new(AtomicUsize::new(0));
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let registration = AudioEncoderFactoryRegistration::from_provider(EncoderProvider {
        native: AudioEncoderFactory::builtin().unwrap(),
        queries: queries.clone(),
        contexts: contexts.clone(),
        drops: drops.clone(),
    });
    (registration, queries, contexts, drops)
}
#[test]
fn caller_discovery_query_create_are_independent_and_keep_owned_inputs() {
    let (registration, queries, contexts, drops) = encoder_registration();
    let observer = registration.failure_observer();
    let spec = raw_spec();
    assert_eq!(
        registration.supported_encoders().unwrap(),
        vec![spec.clone()]
    );
    assert_eq!(
        registration.query_encoder(&spec.format).unwrap(),
        Some(spec.info)
    );
    assert_eq!(registration.query_encoder(&spec.format).unwrap(), None);
    assert_eq!(observer.failure(), None);

    let clock = ManualClock::new(Duration::from_millis(5)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let mut options = OwnedAudioEncoderOptions::default();
    options.set_payload_type(200);
    let id = AudioCodecPairId::create();
    options.set_codec_pair_id(Some(&id));
    let encoder = registration
        .create(&environment, &format(), &options)
        .unwrap()
        .unwrap();
    assert_eq!(queries.load(Ordering::SeqCst), 2); // No implicit caller Query.
    assert_eq!(options.payload_type(), 200);
    let unknown = SdpAudioFormat::new(b"unknown".to_vec(), 1, 1);
    assert!(
        registration
            .create(&environment, &unknown, &options)
            .unwrap()
            .is_none()
    );
    assert_eq!(observer.failure(), None);
    let error = SdpAudioFormat::new(b"error".to_vec(), 1, 1);
    assert_eq!(
        registration.create(&environment, &error, &options).err(),
        Some(AudioFactoryError {
            operation: AudioFactoryOperation::CreateEncoder,
            kind: AudioFactoryFailureKind::Provider(CodecError::ConstructionFailed),
        })
    );
    // Ordinary provider rejection does not disable unrelated creation.
    assert!(!observer.is_disabled());
    assert!(
        registration
            .create(&environment, &format(), &options)
            .unwrap()
            .is_some()
    );
    options.set_payload_type(-1);
    options.set_codec_pair_id(None);
    drop(registration);
    drop(environment);
    drop(clock);
    drop(options);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let context = contexts.lock().unwrap().remove(0);
    contexts.lock().unwrap().clear();
    assert_eq!(context.options.codec_pair_id().unwrap(), id);
    drop(id);
    std::thread::spawn(move || {
        assert_eq!(context.format, format());
        assert_eq!(context.options.payload_type(), 200);
        assert_eq!(
            context.environment.timestamp().microseconds().unwrap(),
            5000
        );
        assert!(encoder.encode(100, &[0; 80]).unwrap().bytes.is_empty());
        let output = encoder.encode(180, &[0; 80]).unwrap();
        assert_eq!(output.info.primary.payload_type, 200);
        assert_eq!(output.bytes.len(), 160);
    })
    .join()
    .unwrap();
}

#[test]
fn callback_panic_disables_registration_without_turning_failure_into_unsupported() {
    let (registration, queries, contexts, drops) = encoder_registration();
    let observer = registration.failure_observer();
    let environment = Environment::builder().build().unwrap();
    let options = OwnedAudioEncoderOptions::default();
    let panic_format = SdpAudioFormat::new(b"panic".to_vec(), 1, 1);
    let first = AudioFactoryError {
        operation: AudioFactoryOperation::CreateEncoder,
        kind: AudioFactoryFailureKind::Panicked,
    };
    assert_eq!(
        registration
            .create(&environment, &panic_format, &options)
            .err(),
        Some(first)
    );
    assert_eq!(
        registration.query_encoder(&format()),
        Err(AudioFactoryError {
            operation: AudioFactoryOperation::QueryEncoder,
            kind: AudioFactoryFailureKind::Disabled,
        })
    );
    assert_eq!(queries.load(Ordering::SeqCst), 0);
    assert_eq!(contexts.lock().unwrap().len(), 1);
    assert_eq!(observer.failure(), Some(first));
    assert!(observer.is_disabled());
    drop(registration);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure(), Some(first));
}

struct Reentrant {
    registration: Arc<Mutex<Option<AudioEncoderFactoryRegistration>>>,
}
impl AudioEncoderFactoryProvider for Reentrant {
    fn supported_encoders(&mut self) -> Vec<AudioCodecSpec> {
        vec![]
    }
    fn query_encoder(&mut self, _format: &SdpAudioFormat) -> Option<AudioCodecInfo> {
        assert_eq!(
            self.registration
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .supported_encoders(),
            Err(AudioFactoryError {
                operation: AudioFactoryOperation::SupportedEncoders,
                kind: AudioFactoryFailureKind::ReentrantCall
            })
        );
        None
    }
    fn create(
        &mut self,
        _: AudioEncoderCreationContext,
    ) -> Result<Option<AudioEncoder>, CodecError> {
        Ok(None)
    }
}
#[test]
fn same_registration_reentry_returns_error_before_locking_and_can_recover() {
    let slot = Arc::new(Mutex::new(None));
    let registration = AudioEncoderFactoryRegistration::from_provider(Reentrant {
        registration: slot.clone(),
    });
    *slot.lock().unwrap() = Some(registration.clone());
    let observer = registration.failure_observer();
    assert_eq!(registration.query_encoder(&format()), Ok(None));
    assert!(!observer.is_disabled());
    assert_eq!(
        observer.failure().unwrap().kind,
        AudioFactoryFailureKind::ReentrantCall
    );
    assert!(registration.supported_encoders().unwrap().is_empty());
    // Break the application's strong-reference cycle before testing destruction.
    slot.lock().unwrap().take();
    drop(registration);
}

struct MutableOnly {
    calls: Cell<i32>,
}
impl AudioEncoderFactoryProvider for MutableOnly {
    fn supported_encoders(&mut self) -> Vec<AudioCodecSpec> {
        vec![]
    }
    fn query_encoder(&mut self, _format: &SdpAudioFormat) -> Option<AudioCodecInfo> {
        let value = self.calls.get() + 1;
        self.calls.set(value);
        Some(AudioCodecInfo::new(8000, 1, value).unwrap())
    }
    fn create(
        &mut self,
        _: AudioEncoderCreationContext,
    ) -> Result<Option<AudioEncoder>, CodecError> {
        Ok(None)
    }
}
#[test]
fn mutable_send_only_provider_is_serialized_across_caller_threads() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<AudioEncoderFactoryRegistration>();
    send_sync::<AudioDecoderFactoryRegistration>();
    let registration = AudioEncoderFactoryRegistration::from_provider(MutableOnly {
        calls: Cell::new(0),
    });
    let mut threads = Vec::new();
    for _ in 0..4 {
        let registration = registration.clone();
        threads.push(std::thread::spawn(move || {
            (0..25)
                .map(|_| {
                    registration
                        .query_encoder(&format())
                        .unwrap()
                        .unwrap()
                        .default_bitrate_bps
                })
                .collect::<Vec<_>>()
        }));
    }
    let mut values = threads
        .into_iter()
        .flat_map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    values.sort_unstable();
    assert_eq!(values, (1..=100).collect::<Vec<_>>());
}

struct LegacyDecoder {
    pairs: Arc<Mutex<Vec<Option<AudioCodecPairId>>>>,
    contexts: Arc<Mutex<Vec<AudioDecoderCreationContext>>>,
}
impl AudioDecoderFactoryProvider for LegacyDecoder {
    fn supported_decoders(&mut self) -> Vec<AudioCodecSpec> {
        vec![raw_spec()]
    }
    fn is_supported_decoder(&mut self, _format: &SdpAudioFormat) -> bool {
        false
    }
    fn create_with_codec_pair_id(
        &mut self,
        context: AudioDecoderCreationContext,
        pair: Option<AudioCodecPairId>,
    ) -> Result<Option<AudioDecoder>, CodecError> {
        self.pairs.lock().unwrap().push(pair);
        self.contexts.lock().unwrap().push(context.clone());
        AudioDecoderFactory::builtin()
            .unwrap()
            .create(&context.environment, &context.format)
    }
}
#[test]
fn decoder_current_default_forwards_absence_and_legacy_keeps_exact_pair_copy() {
    let pairs = Arc::new(Mutex::new(Vec::new()));
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let registration = AudioDecoderFactoryRegistration::from_provider(LegacyDecoder {
        pairs: pairs.clone(),
        contexts: contexts.clone(),
    });
    let environment = Environment::builder().build().unwrap();
    let id = AudioCodecPairId::create();
    assert_eq!(registration.supported_decoders().unwrap(), vec![raw_spec()]);
    assert!(!registration.is_supported_decoder(&format()).unwrap());
    // Create is not prefiltered using a caller's independent support query.
    let decoder = registration
        .create(&environment, &format())
        .unwrap()
        .unwrap();
    assert!(
        registration
            .create_with_codec_pair_id(&environment, &format(), Some(&id))
            .unwrap()
            .is_some()
    );
    assert!(
        registration
            .create_with_codec_pair_id(&environment, &format(), None)
            .unwrap()
            .is_some()
    );
    assert_eq!(*pairs.lock().unwrap(), vec![None, Some(id.clone()), None]);
    drop(registration);
    drop(environment);
    drop(id);
    let context = contexts.lock().unwrap().remove(0);
    contexts.lock().unwrap().clear();
    std::thread::spawn(move || {
        assert_eq!(context.format, format());
        assert_eq!(decoder.sample_rate_hz(), 8000);
        assert_eq!(decoder.channels(), 1);
        decoder.reset();
    })
    .join()
    .unwrap();
}
struct CurrentDecoder;
impl AudioDecoderFactoryProvider for CurrentDecoder {
    fn supported_decoders(&mut self) -> Vec<AudioCodecSpec> {
        vec![]
    }
    fn is_supported_decoder(&mut self, _: &SdpAudioFormat) -> bool {
        true
    }
    fn create(
        &mut self,
        context: AudioDecoderCreationContext,
    ) -> Result<Option<AudioDecoder>, CodecError> {
        AudioDecoderFactory::builtin()
            .unwrap()
            .create(&context.environment, &context.format)
    }
}
struct NeitherDecoder;
impl AudioDecoderFactoryProvider for NeitherDecoder {
    fn supported_decoders(&mut self) -> Vec<AudioCodecSpec> {
        vec![]
    }
    fn is_supported_decoder(&mut self, _: &SdpAudioFormat) -> bool {
        false
    }
}
#[test]
fn decoder_compatibility_default_forwards_to_current_and_missing_defaults_do_not_recurse() {
    let environment = Environment::builder().build().unwrap();
    let id = AudioCodecPairId::create();
    let current = AudioDecoderFactoryRegistration::from_provider(CurrentDecoder);
    assert!(
        current
            .create_with_codec_pair_id(&environment, &format(), Some(&id))
            .unwrap()
            .is_some()
    );
    let neither = AudioDecoderFactoryRegistration::from_provider(NeitherDecoder);
    for (result, operation) in [
        (
            neither.create(&environment, &format()),
            AudioFactoryOperation::CreateDecoder,
        ),
        (
            neither.create_with_codec_pair_id(&environment, &format(), Some(&id)),
            AudioFactoryOperation::CreateDecoderWithCodecPairId,
        ),
    ] {
        assert_eq!(
            result.err(),
            Some(AudioFactoryError {
                operation,
                kind: AudioFactoryFailureKind::Provider(CodecError::ConstructionFailed)
            })
        );
    }
    assert!(!neither.failure_observer().is_disabled());
}
struct DestructorPanic;
impl AudioEncoderFactoryProvider for DestructorPanic {
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
        Ok(None)
    }
}
impl Drop for DestructorPanic {
    fn drop(&mut self) {
        panic!("factory destructor");
    }
}
#[test]
fn final_provider_destructor_panic_is_observable_after_all_registrations_drop() {
    let registration = AudioEncoderFactoryRegistration::from_provider(DestructorPanic);
    let clone = registration.clone();
    let observer = registration.failure_observer();
    drop(registration);
    assert_eq!(observer.failure(), None);
    drop(clone);
    assert_eq!(
        observer.failure(),
        Some(AudioFactoryError {
            operation: AudioFactoryOperation::Destroy,
            kind: AudioFactoryFailureKind::Panicked
        })
    );
    assert!(observer.is_disabled());
}
