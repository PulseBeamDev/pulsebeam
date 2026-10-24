//! Persistent standalone SDK caller factories. Real PCM products and owning
//! contexts, not peer/NetEq installation or caller-codec adaptation evidence.
use pulsebeam_webrtc_sys::{
    AudioCodecInfo, AudioCodecPairId, AudioCodecSpec, AudioDecoder, AudioDecoderCreationContext,
    AudioDecoderFactory, AudioDecoderFactoryProvider, AudioDecoderFactoryRegistration,
    AudioEncoder, AudioEncoderCreationContext, AudioEncoderFactory, AudioEncoderFactoryProvider,
    AudioEncoderFactoryRegistration, AudioFactoryError, AudioFactoryFailureKind,
    AudioFactoryOperation, CallerAudioDecoderFactory, CallerAudioEncoderFactory, CodecError,
    Environment, ManualClock, OwnedAudioEncoderOptions, SdpAudioFormat,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
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
fn error(operation: AudioFactoryOperation, kind: AudioFactoryFailureKind) -> AudioFactoryError {
    AudioFactoryError { operation, kind }
}
#[derive(Default)]
struct EncoderEvidence {
    calls: Mutex<Vec<&'static str>>,
    contexts: Mutex<Vec<AudioEncoderCreationContext>>,
    drops: AtomicUsize,
    handles: Mutex<Option<(CallerAudioEncoderFactory, CallerAudioEncoderFactory)>>,
    reentries: Mutex<Vec<AudioFactoryError>>,
}
struct EncoderProvider {
    evidence: Arc<EncoderEvidence>,
    queries: usize,
}
impl Drop for EncoderProvider {
    fn drop(&mut self) {
        self.evidence.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl AudioEncoderFactoryProvider for EncoderProvider {
    fn supported_encoders(&mut self) -> Vec<AudioCodecSpec> {
        self.evidence.calls.lock().unwrap().push("discovery");
        vec![declaration()]
    }
    fn query_encoder(&mut self, format: &SdpAudioFormat) -> Option<AudioCodecInfo> {
        self.evidence.calls.lock().unwrap().push("query");
        if format.name == b"panic" {
            panic!("persistent Query");
        }
        if format.name == b"reenter" {
            let handles = self.evidence.handles.lock().unwrap();
            let (same, other) = handles.as_ref().unwrap();
            let first = same.query_encoder(&pcm()).unwrap_err();
            let second = other.supported_encoders().unwrap_err();
            self.evidence
                .reentries
                .lock()
                .unwrap()
                .extend([first, second]);
        }
        self.queries += 1;
        (self.queries % 2 == 1).then(|| declaration().info)
    }
    fn create(
        &mut self,
        context: AudioEncoderCreationContext,
    ) -> Result<Option<AudioEncoder>, CodecError> {
        self.evidence.calls.lock().unwrap().push("create");
        self.evidence.contexts.lock().unwrap().push(context.clone());
        match context.format.name.as_slice() {
            b"none" => Ok(None),
            b"error" => Err(CodecError::ConstructionFailed),
            _ => AudioEncoderFactory::builtin().unwrap().create_with_options(
                &context.environment,
                &pcm(),
                &context.options,
            ),
        }
    }
}
fn encoder_registration() -> (AudioEncoderFactoryRegistration, Arc<EncoderEvidence>) {
    let evidence = Arc::new(EncoderEvidence::default());
    (
        AudioEncoderFactoryRegistration::from_provider(EncoderProvider {
            evidence: evidence.clone(),
            queries: 0,
        }),
        evidence,
    )
}
#[derive(Default)]
struct DecoderEvidence {
    calls: Mutex<Vec<&'static str>>,
    contexts: Mutex<Vec<AudioDecoderCreationContext>>,
    pairs: Mutex<Vec<Option<AudioCodecPairId>>>,
    drops: AtomicUsize,
    handles: Mutex<Option<(CallerAudioDecoderFactory, CallerAudioDecoderFactory)>>,
    reentries: Mutex<Vec<AudioFactoryError>>,
}
struct DecoderProvider {
    evidence: Arc<DecoderEvidence>,
}
impl Drop for DecoderProvider {
    fn drop(&mut self) {
        self.evidence.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl DecoderProvider {
    fn product(
        &mut self,
        context: AudioDecoderCreationContext,
    ) -> Result<Option<AudioDecoder>, CodecError> {
        self.evidence.contexts.lock().unwrap().push(context.clone());
        match context.format.name.as_slice() {
            b"none" => Ok(None),
            b"error" => Err(CodecError::ConstructionFailed),
            b"panic" => panic!("persistent Create"),
            _ => AudioDecoderFactory::builtin()
                .unwrap()
                .create(&context.environment, &pcm()),
        }
    }
}
impl AudioDecoderFactoryProvider for DecoderProvider {
    fn supported_decoders(&mut self) -> Vec<AudioCodecSpec> {
        self.evidence.calls.lock().unwrap().push("discovery");
        vec![declaration()]
    }
    fn is_supported_decoder(&mut self, format: &SdpAudioFormat) -> bool {
        self.evidence.calls.lock().unwrap().push("support");
        if format.name == b"reenter" {
            let handles = self.evidence.handles.lock().unwrap();
            let (same, other) = handles.as_ref().unwrap();
            self.evidence.reentries.lock().unwrap().extend([
                same.is_supported_decoder(&pcm()).unwrap_err(),
                other.supported_decoders().unwrap_err(),
            ]);
        }
        false
    }
    fn create(
        &mut self,
        context: AudioDecoderCreationContext,
    ) -> Result<Option<AudioDecoder>, CodecError> {
        self.evidence.calls.lock().unwrap().push("current");
        self.product(context)
    }
    fn create_with_codec_pair_id(
        &mut self,
        context: AudioDecoderCreationContext,
        pair: Option<AudioCodecPairId>,
    ) -> Result<Option<AudioDecoder>, CodecError> {
        self.evidence.calls.lock().unwrap().push("compatibility");
        self.evidence.pairs.lock().unwrap().push(pair);
        self.product(context)
    }
}
fn decoder_registration() -> (AudioDecoderFactoryRegistration, Arc<DecoderEvidence>) {
    let evidence = Arc::new(DecoderEvidence::default());
    (
        AudioDecoderFactoryRegistration::from_provider(DecoderProvider {
            evidence: evidence.clone(),
        }),
        evidence,
    )
}
#[test]
fn persistent_encoder_retains_registration_and_exact_per_call_inputs_after_migration() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<CallerAudioEncoderFactory>();
    let (registration, evidence) = encoder_registration();
    let factory = registration.sdk_factory();
    let alias = factory.clone();
    assert!(evidence.calls.lock().unwrap().is_empty()); // No constructor callback.
    let observer = factory.failure_observer();
    drop(registration);
    drop(factory);
    assert_eq!(evidence.drops.load(Ordering::SeqCst), 0);
    let clock = ManualClock::new(Duration::from_millis(13)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let clock2 = ManualClock::new(Duration::from_millis(31)).unwrap();
    let environment2 = Environment::builder().clock(&clock2).build().unwrap();
    let pair = AudioCodecPairId::create();
    let mut options = OwnedAudioEncoderOptions::default();
    options.set_payload_type(i32::MAX);
    options.set_codec_pair_id(Some(&pair));
    let expected_pair = pair.copy_value();
    let format = declaration().format;
    std::thread::spawn(move || {
        assert_eq!(alias.supported_encoders().unwrap(), vec![declaration()]);
        assert_eq!(
            alias.query_encoder(&format).unwrap(),
            Some(declaration().info)
        );
        assert_eq!(alias.query_encoder(&format).unwrap(), None); // No query cache.
        let encoder = alias
            .create(&environment, &format, &options)
            .unwrap()
            .unwrap();
        options.set_payload_type(-1);
        options.set_codec_pair_id(None);
        let second = alias
            .create(&environment2, &format, &options)
            .unwrap()
            .unwrap();
        drop(alias);
        drop(options);
        drop(environment);
        drop(environment2);
        drop(clock);
        drop(clock2);
        drop(pair);
        assert_eq!(encoder.sample_rate_hz(), 8000); // Unlike raw request metadata.
        encoder
            .on_received_uplink_packet_loss_fraction(f32::NAN)
            .unwrap(); // Actual PCM, not Opus guessed by name.
        assert!(encoder.encode(100, &[0; 80]).unwrap().bytes.is_empty());
        let packet = encoder.encode(180, &[0; 80]).unwrap();
        assert_eq!(packet.bytes.len(), 160);
        assert_eq!(packet.info.primary.payload_type, i32::MAX);
        assert!(second.encode(100, &[0; 80]).unwrap().bytes.is_empty());
        assert_eq!(
            second
                .encode(180, &[0; 80])
                .unwrap()
                .info
                .primary
                .payload_type,
            -1
        );
    })
    .join()
    .unwrap();
    assert_eq!(evidence.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        *evidence.calls.lock().unwrap(),
        vec!["discovery", "query", "query", "create", "create"]
    );
    let contexts = evidence.contexts.lock().unwrap();
    assert_eq!(
        contexts[0].environment.timestamp().microseconds().unwrap(),
        13000
    );
    assert_eq!(
        contexts[1].environment.timestamp().microseconds().unwrap(),
        31000
    );
    assert_eq!(contexts[0].options.payload_type(), i32::MAX);
    assert_eq!(contexts[0].options.codec_pair_id().unwrap(), expected_pair);
    assert_eq!(contexts[1].options.payload_type(), -1);
    assert!(contexts[1].options.codec_pair_id().is_none());
    assert_eq!(observer.failure(), None);
}
#[test]
fn persistent_decoder_preserves_both_overloads_pairs_contexts_and_frame_retention() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<CallerAudioDecoderFactory>();
    let (registration, evidence) = decoder_registration();
    let factory = registration.sdk_factory();
    drop(registration);
    assert!(evidence.calls.lock().unwrap().is_empty());
    assert_eq!(factory.supported_decoders().unwrap(), vec![declaration()]);
    let format = declaration().format;
    assert!(!factory.is_supported_decoder(&format).unwrap());
    let clock = ManualClock::new(Duration::from_millis(9)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let pair = AudioCodecPairId::create();
    let decoder = factory.create(&environment, &format).unwrap().unwrap();
    let compatible = factory
        .create_with_codec_pair_id(&environment, &format, Some(&pair))
        .unwrap()
        .unwrap();
    assert!(
        factory
            .create_with_codec_pair_id(&environment, &format, None)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        *evidence.pairs.lock().unwrap(),
        vec![Some(pair.clone()), None]
    );
    // Generate genuine PCM payload, not a substitute wire-codec fixture.
    let encoder = AudioEncoderFactory::builtin()
        .unwrap()
        .create(
            &environment,
            &pcm(),
            &pulsebeam_webrtc_sys::AudioEncoderOptions {
                payload_type: 200,
                codec_pair_id: None,
            },
        )
        .unwrap()
        .unwrap();
    assert!(encoder.encode(100, &[0; 80]).unwrap().bytes.is_empty());
    let payload = encoder.encode(180, &[0; 80]).unwrap().bytes;
    let parsed = decoder.parse_payload(payload, u32::MAX - 79).unwrap();
    drop(encoder);
    drop(decoder);
    drop(compatible);
    drop(factory);
    drop(environment);
    drop(clock);
    drop(pair);
    assert_eq!(evidence.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        *evidence.calls.lock().unwrap(),
        vec![
            "discovery",
            "support",
            "current",
            "compatibility",
            "compatibility"
        ]
    );
    assert_eq!(
        evidence.contexts.lock().unwrap()[0]
            .environment
            .timestamp()
            .microseconds()
            .unwrap(),
        9000
    );
    std::thread::spawn(move || {
        let frame = parsed[0].frame.as_ref().unwrap();
        assert_eq!(parsed[0].timestamp, u32::MAX - 79);
        assert_eq!(frame.duration(), 160);
        assert_eq!(frame.decode().unwrap().unwrap().samples.len(), 160);
    })
    .join()
    .unwrap();
}
#[test]
fn receipts_do_not_turn_callback_rejection_into_null_or_contaminate_later_operations() {
    let (registration, evidence) = encoder_registration();
    let factory = registration.sdk_factory();
    let environment = Environment::builder().build().unwrap();
    let options = OwnedAudioEncoderOptions::default();
    let format = |name: &[u8]| SdpAudioFormat::new(name.to_vec(), 0, 0);
    assert!(
        factory
            .create(&environment, &format(b"none"), &options)
            .unwrap()
            .is_none()
    );
    let first = error(
        AudioFactoryOperation::CreateEncoder,
        AudioFactoryFailureKind::Provider(CodecError::ConstructionFailed),
    );
    assert_eq!(
        factory
            .create(&environment, &format(b"error"), &options)
            .err(),
        Some(first)
    );
    assert!(
        factory
            .create(&environment, &pcm(), &options)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        factory.query_encoder(&pcm()).unwrap(),
        Some(declaration().info)
    );
    assert_eq!(factory.failure_observer().failure(), Some(first));
    assert_eq!(evidence.calls.lock().unwrap().len(), 4);
    let (registration, _) = decoder_registration();
    let factory = registration.sdk_factory();
    assert!(
        factory
            .create(&environment, &format(b"none"))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        factory
            .create_with_codec_pair_id(&environment, &format(b"error"), None)
            .err(),
        Some(error(
            AudioFactoryOperation::CreateDecoderWithCodecPairId,
            AudioFactoryFailureKind::Provider(CodecError::ConstructionFailed)
        ))
    );
    assert!(factory.create(&environment, &pcm()).unwrap().is_some());
    assert!(!factory.is_supported_decoder(&pcm()).unwrap());
}
#[test]
fn same_registration_reentry_via_same_or_different_sdk_handle_is_pre_mutex() {
    let (registration, evidence) = encoder_registration();
    let first = registration.sdk_factory();
    let other = registration.sdk_factory();
    *evidence.handles.lock().unwrap() = Some((first.clone(), other.clone()));
    assert!(
        first
            .query_encoder(&SdpAudioFormat::new(b"reenter".to_vec(), 0, 0))
            .unwrap()
            .is_some()
    );
    assert_eq!(
        *evidence.reentries.lock().unwrap(),
        vec![
            error(
                AudioFactoryOperation::QueryEncoder,
                AudioFactoryFailureKind::ReentrantCall
            ),
            error(
                AudioFactoryOperation::SupportedEncoders,
                AudioFactoryFailureKind::ReentrantCall
            )
        ]
    );
    evidence.handles.lock().unwrap().take(); // Break application's temporary strong cycle.
    assert!(!first.failure_observer().is_disabled());
    assert_eq!(first.query_encoder(&pcm()).unwrap(), None);
    let (registration, evidence) = decoder_registration();
    let first = registration.sdk_factory();
    let other = registration.sdk_factory();
    *evidence.handles.lock().unwrap() = Some((first.clone(), other.clone()));
    assert!(
        !first
            .is_supported_decoder(&SdpAudioFormat::new(b"reenter".to_vec(), 0, 0))
            .unwrap()
    );
    assert_eq!(
        *evidence.reentries.lock().unwrap(),
        vec![
            error(
                AudioFactoryOperation::IsSupportedDecoder,
                AudioFactoryFailureKind::ReentrantCall
            ),
            error(
                AudioFactoryOperation::SupportedDecoders,
                AudioFactoryFailureKind::ReentrantCall
            )
        ]
    );
    evidence.handles.lock().unwrap().take();
    assert_eq!(first.supported_decoders().unwrap(), vec![declaration()]);
}
#[test]
fn persistent_callback_panic_disables_clones_and_preserves_original_failure_observer() {
    let (registration, evidence) = encoder_registration();
    let factory = registration.sdk_factory();
    let alias = factory.clone();
    let observer = factory.failure_observer();
    let first = error(
        AudioFactoryOperation::QueryEncoder,
        AudioFactoryFailureKind::Panicked,
    );
    assert_eq!(
        factory
            .query_encoder(&SdpAudioFormat::new(b"panic".to_vec(), 0, 0))
            .err(),
        Some(first)
    );
    assert_eq!(
        alias.supported_encoders().err(),
        Some(error(
            AudioFactoryOperation::SupportedEncoders,
            AudioFactoryFailureKind::Disabled
        ))
    );
    assert_eq!(evidence.calls.lock().unwrap().len(), 1);
    drop(registration);
    drop(factory);
    drop(alias);
    assert_eq!(observer.failure(), Some(first));
    assert!(observer.is_disabled());
    assert_eq!(evidence.drops.load(Ordering::SeqCst), 1);
    let (registration, evidence) = decoder_registration();
    let factory = registration.sdk_factory();
    let environment = Environment::builder().build().unwrap();
    assert_eq!(
        factory
            .create_with_codec_pair_id(
                &environment,
                &SdpAudioFormat::new(b"panic".to_vec(), 0, 0),
                None
            )
            .err(),
        Some(error(
            AudioFactoryOperation::CreateDecoderWithCodecPairId,
            AudioFactoryFailureKind::Panicked
        ))
    );
    assert_eq!(
        factory.is_supported_decoder(&pcm()).err(),
        Some(error(
            AudioFactoryOperation::IsSupportedDecoder,
            AudioFactoryFailureKind::Disabled
        ))
    );
    assert_eq!(evidence.calls.lock().unwrap().len(), 1);
}
#[test]
fn persistent_concurrent_calls_keep_independent_receipts_and_serialized_provider_state() {
    let (registration, evidence) = encoder_registration();
    let factory = registration.sdk_factory();
    let environment = Environment::builder().build().unwrap();
    // Alternate clones of two distinct SDK instances sharing one registration.
    let other = registration.sdk_factory();
    let threads: Vec<_> = (0..12)
        .map(|index| {
            let factory = if index % 4 < 2 {
                factory.clone()
            } else {
                other.clone()
            };
            let environment = environment.clone();
            std::thread::spawn(move || {
                let options = OwnedAudioEncoderOptions::default();
                let format = if index % 2 == 0 {
                    pcm()
                } else {
                    SdpAudioFormat::new(b"error".to_vec(), 0, 0)
                };
                let result = factory.create(&environment, &format, &options);
                if index % 2 == 0 {
                    assert!(result.unwrap().is_some());
                } else {
                    assert_eq!(
                        result.err(),
                        Some(error(
                            AudioFactoryOperation::CreateEncoder,
                            AudioFactoryFailureKind::Provider(CodecError::ConstructionFailed)
                        ))
                    );
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(evidence.calls.lock().unwrap().len(), 12);
    assert!(!factory.failure_observer().is_disabled());
    assert_eq!(factory.supported_encoders().unwrap(), vec![declaration()]);
}
