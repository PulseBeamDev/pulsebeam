//! Standalone owning caller products delegate to genuine SDK PCM. Fault modes
//! exercise malformed callback/status contracts, not substitute wire codecs.
use pulsebeam_webrtc_sys::{
    AudioDecoder, AudioDecoderError, AudioDecoderFactory, AudioDecoderFrameProduct,
    AudioDecoderFrameProvider, AudioDecoderProductError, AudioDecoderProductFailureKind,
    AudioDecoderProductFailureObserver, AudioDecoderProductOperation, AudioDecoderProvider,
    AudioDecoderProviderParseResult, AudioEncoderFactory, AudioSpeechType, CallerAudioDecoder,
    CallerDecodableAudioFrame, DecodableAudioFrame, DecodedAudio, Environment, ManualClock,
    SdpAudioFormat,
};
use std::{
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy, Eq, PartialEq)]
enum Fault {
    None,
    NegativePriority,
    NullFrame,
    ParseError,
    DecodeNone,
    DecodeError,
    DecodePanic,
    ExcessSamples,
    HugeDuration,
    ShortPlc,
    FrameDropPanic,
    DecoderDropPanic,
}
#[derive(Default)]
struct Evidence {
    events: Mutex<Vec<&'static str>>,
    decodes: AtomicUsize,
    plcs: AtomicUsize,
    slot: Mutex<Option<CallerDecodableAudioFrame>>,
    transfer_observers: Mutex<Vec<AudioDecoderProductFailureObserver>>,
}
struct Pcm {
    decoder: AudioDecoder,
    environment: Environment,
    evidence: Arc<Evidence>,
    fault: Fault,
}
impl Drop for Pcm {
    fn drop(&mut self) {
        assert_eq!(self.environment.timestamp().microseconds().unwrap(), 17000);
        self.evidence.events.lock().unwrap().push("decoder-drop");
        if self.fault == Fault::DecoderDropPanic {
            panic!("caller decoder destructor");
        }
    }
}
impl AudioDecoderProvider for Pcm {
    fn sample_rate_hz(&mut self) -> i32 {
        self.decoder.sample_rate_hz()
    }
    fn channels(&mut self) -> usize {
        self.decoder.channels()
    }
    fn reset(&mut self) -> Result<(), AudioDecoderError> {
        self.decoder.reset();
        Ok(())
    }
    fn packet_duration(&mut self, encoded: &[u8]) -> i32 {
        self.decoder.packet_info(encoded).unwrap().duration
    }
    fn redundant_packet_duration(&mut self, encoded: &[u8]) -> i32 {
        self.decoder
            .packet_info(encoded)
            .unwrap()
            .redundant_duration
    }
    fn packet_has_fec(&mut self, encoded: &[u8]) -> bool {
        self.decoder.packet_info(encoded).unwrap().has_fec
    }
    fn parse_payload(
        &mut self,
        encoded: Vec<u8>,
        timestamp: u32,
    ) -> Result<Vec<AudioDecoderProviderParseResult>, AudioDecoderError> {
        self.evidence.events.lock().unwrap().push("parse-start");
        if let Some(frame) = self.evidence.slot.lock().unwrap().take() {
            drop(frame); // Last owner may be dropped inside its decoder callback.
            assert!(!self.evidence.events.lock().unwrap().contains(&"frame-drop"));
        }
        if self.fault == Fault::ParseError {
            return Err(AudioDecoderError::InvalidPayload);
        }
        let mut parsed: Vec<_> = self
            .decoder
            .parse_payload(encoded, timestamp)?
            .into_iter()
            .map(|item| AudioDecoderProviderParseResult {
                timestamp: item.timestamp,
                priority: if self.fault == Fault::NegativePriority {
                    -1
                } else {
                    item.priority
                },
                frame: item.frame.map(|frame| {
                    let product = AudioDecoderFrameProduct::from_provider(PcmFrame {
                        frame,
                        environment: self.environment.clone(),
                        evidence: self.evidence.clone(),
                        fault: self.fault,
                    });
                    self.evidence
                        .transfer_observers
                        .lock()
                        .unwrap()
                        .push(product.failure_observer());
                    product
                }),
            })
            .collect();
        if self.fault == Fault::NullFrame {
            parsed.push(AudioDecoderProviderParseResult {
                timestamp: timestamp.wrapping_add(7),
                priority: i32::MAX,
                frame: None,
            });
        }
        self.evidence.events.lock().unwrap().push("parse-end");
        Ok(parsed)
    }
    fn generate_plc(
        &mut self,
        requested: usize,
        prefix: &[i16],
        _: usize,
    ) -> Result<Vec<i16>, AudioDecoderError> {
        self.evidence.plcs.fetch_add(1, Ordering::SeqCst);
        if self.fault == Fault::ShortPlc {
            return Ok(vec![42]);
        } // Deliberately invalid output, refused by owner.
        let mut samples = prefix.to_vec();
        self.decoder.generate_plc(requested, &mut samples)?;
        assert_eq!(&samples[..prefix.len()], prefix);
        Ok(samples.split_off(prefix.len()))
    }
}
struct PcmFrame {
    frame: DecodableAudioFrame,
    environment: Environment,
    evidence: Arc<Evidence>,
    fault: Fault,
}
impl Drop for PcmFrame {
    fn drop(&mut self) {
        assert_eq!(self.environment.timestamp().microseconds().unwrap(), 17000);
        self.evidence.events.lock().unwrap().push("frame-drop");
        if self.fault == Fault::FrameDropPanic {
            panic!("caller frame destructor");
        }
    }
}
impl AudioDecoderFrameProvider for PcmFrame {
    fn duration(&mut self) -> usize {
        if self.fault == Fault::HugeDuration {
            usize::MAX
        } else {
            self.frame.duration()
        }
    }
    fn is_dtx_packet(&mut self) -> bool {
        self.frame.is_dtx_packet()
    }
    fn decode(&mut self, _: usize) -> Result<Option<DecodedAudio>, AudioDecoderError> {
        self.evidence.decodes.fetch_add(1, Ordering::SeqCst);
        match self.fault {
            Fault::DecodeNone => return Ok(None),
            Fault::DecodeError => return Err(AudioDecoderError::NativeInvariant),
            Fault::DecodePanic => panic!("caller frame Decode"),
            _ => (),
        }
        assert_eq!(self.environment.timestamp().microseconds().unwrap(), 17000);
        let mut output = self.frame.decode()?;
        if self.fault == Fault::ExcessSamples {
            output.as_mut().unwrap().samples.push(0);
        }
        Ok(output)
    }
}
fn setup(fault: Fault) -> (CallerAudioDecoder, Vec<u8>, Arc<Evidence>) {
    let clock = ManualClock::new(Duration::from_millis(17)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let format = SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1);
    let encoder_factory = AudioEncoderFactory::builtin().unwrap();
    let encoder = encoder_factory
        .create(
            &environment,
            &format,
            &pulsebeam_webrtc_sys::AudioEncoderOptions {
                payload_type: 200,
                codec_pair_id: None,
            },
        )
        .unwrap()
        .unwrap();
    assert!(encoder.encode(100, &[0; 80]).unwrap().bytes.is_empty());
    let payload = encoder.encode(180, &[0; 80]).unwrap().bytes;
    assert_eq!(payload.len(), 160);
    let decoder_factory = AudioDecoderFactory::builtin().unwrap();
    let decoder = decoder_factory
        .create(&environment, &format)
        .unwrap()
        .unwrap();
    let evidence = Arc::new(Evidence::default());
    let product = CallerAudioDecoder::from_provider(
        &environment,
        Pcm {
            decoder,
            environment: environment.clone(),
            evidence: evidence.clone(),
            fault,
        },
    );
    drop(encoder);
    drop(encoder_factory);
    drop(decoder_factory);
    drop(environment);
    drop(clock);
    (product, payload, evidence)
}
fn one_frame(product: &CallerAudioDecoder, payload: Vec<u8>) -> CallerDecodableAudioFrame {
    let mut parsed = product.parse_payload(payload, u32::MAX - 79).unwrap();
    assert_eq!(parsed.len(), 1);
    let item = parsed.remove(0);
    assert_eq!(item.timestamp, u32::MAX - 79);
    assert_eq!(item.priority, 0);
    item.frame.unwrap()
}
fn error(
    operation: AudioDecoderProductOperation,
    kind: AudioDecoderProductFailureKind,
) -> AudioDecoderProductError {
    AudioDecoderProductError { operation, kind }
}
#[test]
fn owning_real_frame_survives_decoder_retirement_and_migrated_use() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<CallerAudioDecoder>();
    send_sync::<CallerDecodableAudioFrame>();
    let (product, payload, evidence) = setup(Fault::None);
    assert_eq!(product.sample_rate_hz(), Ok(8000));
    assert_eq!(product.channels(), Ok(1));
    assert_eq!(product.error_code(), Ok(0));
    assert_eq!(product.packet_duration(&payload), Ok(160));
    // The selected native PCMU decoder overrides PacketDurationRedundant and
    // delegates to PacketDuration; kNotImplemented is only the base default.
    assert_eq!(product.redundant_packet_duration(&payload), Ok(160));
    assert_eq!(product.packet_has_fec(&payload), Ok(false));
    let frame = one_frame(&product, payload);
    assert_eq!(frame.duration(), Ok(160));
    assert_eq!(frame.is_dtx_packet(), Ok(false));
    let observer = product.failure_observer();
    drop(product);
    assert!(!evidence.events.lock().unwrap().contains(&"decoder-drop"));
    std::thread::spawn(move || {
        assert_eq!(frame.sample_rate_hz(), Ok(8000));
        assert_eq!(frame.channels(), Ok(1));
        let decoded = frame.decode(160).unwrap().unwrap();
        assert_eq!(decoded.samples.len(), 160);
        assert_eq!(decoded.speech_type, AudioSpeechType::SPEECH);
        drop(frame);
    })
    .join()
    .unwrap();
    assert_eq!(evidence.decodes.load(Ordering::SeqCst), 1);
    assert_eq!(
        *evidence.events.lock().unwrap(),
        vec!["parse-start", "parse-end", "frame-drop", "decoder-drop"]
    );
    assert_eq!(observer.failure(), None);
}
#[test]
fn insufficient_capacity_is_pre_dispatch_and_can_be_retried_without_consuming_frame() {
    let (product, payload, evidence) = setup(Fault::None);
    let frame = one_frame(&product, payload);
    assert_eq!(
        frame.decode(159),
        Err(error(
            AudioDecoderProductOperation::FrameDecode,
            AudioDecoderProductFailureKind::InvalidCapacity
        ))
    );
    assert_eq!(evidence.decodes.load(Ordering::SeqCst), 0);
    assert_eq!(frame.decode(160).unwrap().unwrap().samples.len(), 160);
    assert_eq!(
        frame.decode(160),
        Err(error(
            AudioDecoderProductOperation::FrameDecode,
            AudioDecoderProductFailureKind::FrameAlreadyDecoded
        ))
    );
    assert_eq!(evidence.decodes.load(Ordering::SeqCst), 1);
}
#[test]
fn ordinary_none_callback_error_and_malformed_output_are_distinct_single_shot_results() {
    for fault in [Fault::DecodeNone, Fault::DecodeError, Fault::ExcessSamples] {
        let (product, payload, evidence) = setup(fault);
        let frame = one_frame(&product, payload);
        let expected = match fault {
            Fault::DecodeNone => Ok(None),
            Fault::DecodeError => Err(error(
                AudioDecoderProductOperation::FrameDecode,
                AudioDecoderProductFailureKind::Provider(AudioDecoderError::NativeInvariant),
            )),
            _ => Err(error(
                AudioDecoderProductOperation::FrameDecode,
                AudioDecoderProductFailureKind::InvalidOutput,
            )),
        };
        assert_eq!(frame.decode(160), expected);
        assert_eq!(
            frame.decode(160),
            Err(error(
                AudioDecoderProductOperation::FrameDecode,
                AudioDecoderProductFailureKind::FrameAlreadyDecoded
            ))
        );
        assert_eq!(evidence.decodes.load(Ordering::SeqCst), 1);
        assert!(!product.failure_observer().is_disabled());
    }
}
#[test]
fn frame_panic_disables_the_shared_decoder_without_returning_native_none() {
    let (product, payload, evidence) = setup(Fault::DecodePanic);
    let frame = one_frame(&product, payload);
    let observer = frame.failure_observer();
    let first = error(
        AudioDecoderProductOperation::FrameDecode,
        AudioDecoderProductFailureKind::Panicked,
    );
    assert_eq!(frame.decode(160), Err(first));
    assert_eq!(
        product.channels(),
        Err(error(
            AudioDecoderProductOperation::Channels,
            AudioDecoderProductFailureKind::Disabled
        ))
    );
    assert_eq!(
        frame.decode(160),
        Err(error(
            AudioDecoderProductOperation::FrameDecode,
            AudioDecoderProductFailureKind::Disabled
        ))
    );
    drop(product);
    drop(frame);
    assert_eq!(observer.failure(), Some(first));
    assert!(observer.is_disabled());
    assert_eq!(evidence.decodes.load(Ordering::SeqCst), 1);
    assert_eq!(
        evidence.events.lock().unwrap().last(),
        Some(&"decoder-drop")
    );
}
#[test]
fn priority_refusal_is_not_null_frame_substitution_and_null_metadata_is_observable() {
    let (product, payload, evidence) = setup(Fault::NegativePriority);
    assert!(
        matches!(product.parse_payload(payload, 55), Err(e) if e == error(
        AudioDecoderProductOperation::ParsePayload, AudioDecoderProductFailureKind::InvalidParseResult))
    );
    assert!(evidence.events.lock().unwrap().contains(&"frame-drop"));
    let (product, payload, _) = setup(Fault::NullFrame);
    let parsed = product.parse_payload(payload, u32::MAX - 3).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[1].timestamp, 3);
    assert_eq!(parsed[1].priority, i32::MAX);
    assert!(parsed[1].frame.is_none());
}
#[test]
fn parse_provider_error_is_not_empty_success() {
    let (product, payload, _) = setup(Fault::ParseError);
    assert!(
        matches!(product.parse_payload(payload, 0), Err(e) if e == error(
        AudioDecoderProductOperation::ParsePayload, AudioDecoderProductFailureKind::Provider(AudioDecoderError::InvalidPayload)))
    );
}
#[test]
fn plc_append_bound_prefix_and_no_output_are_preserved() {
    let (product, _, evidence) = setup(Fault::None);
    let mut prefix = vec![-1000, 3, 32767];
    product.generate_plc(usize::MAX, &mut prefix, 0).unwrap(); // Actual PCM default emits nothing.
    assert_eq!(prefix, vec![-1000, 3, 32767]);
    assert_eq!(evidence.plcs.load(Ordering::SeqCst), 1);
    assert_eq!(
        product.generate_plc(80, &mut prefix, usize::MAX),
        Err(error(
            AudioDecoderProductOperation::GeneratePlc,
            AudioDecoderProductFailureKind::InvalidCapacity
        ))
    );
    assert_eq!(evidence.plcs.load(Ordering::SeqCst), 1);
    let (product, _, _) = setup(Fault::ShortPlc);
    assert_eq!(
        product.generate_plc(80, &mut prefix, 80),
        Err(error(
            AudioDecoderProductOperation::GeneratePlc,
            AudioDecoderProductFailureKind::InvalidOutput
        ))
    );
    assert_eq!(prefix, vec![-1000, 3, 32767]);
}
#[test]
fn raw_duration_is_observable_without_stock_frame_capacity_policy() {
    let (product, payload, evidence) = setup(Fault::HugeDuration);
    let frame = one_frame(&product, payload);
    assert_eq!(frame.duration(), Ok(usize::MAX));
    assert_eq!(
        frame.decode(160),
        Err(error(
            AudioDecoderProductOperation::FrameDecode,
            AudioDecoderProductFailureKind::InvalidCapacity
        ))
    );
    assert_eq!(evidence.decodes.load(Ordering::SeqCst), 0);
}
#[test]
fn cloned_frame_has_one_decode_attempt_across_concurrent_threads() {
    let (product, payload, evidence) = setup(Fault::None);
    let frame = one_frame(&product, payload);
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let frame = frame.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                frame.decode(160)
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|r| matches!(r, Ok(Some(_)))).count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| **r
                == Err(error(
                    AudioDecoderProductOperation::FrameDecode,
                    AudioDecoderProductFailureKind::FrameAlreadyDecoded
                )))
            .count(),
        1
    );
    assert_eq!(evidence.decodes.load(Ordering::SeqCst), 1);
}
#[test]
fn frame_dropped_inside_decoder_callback_retires_after_callback_not_recursively() {
    let (product, payload, evidence) = setup(Fault::None);
    let frame = one_frame(&product, payload.clone());
    *evidence.slot.lock().unwrap() = Some(frame);
    let next = product.parse_payload(payload, 100).unwrap();
    assert_eq!(
        *evidence.events.lock().unwrap(),
        vec![
            "parse-start",
            "parse-end",
            "parse-start",
            "parse-end",
            "frame-drop"
        ]
    );
    drop(next);
    drop(product);
    assert_eq!(
        *evidence.events.lock().unwrap(),
        vec![
            "parse-start",
            "parse-end",
            "parse-start",
            "parse-end",
            "frame-drop",
            "frame-drop",
            "decoder-drop"
        ]
    );
}
#[test]
fn single_frame_or_decoder_destructor_panic_is_retained_after_retirement() {
    for fault in [Fault::FrameDropPanic, Fault::DecoderDropPanic] {
        let (product, payload, evidence) = setup(fault);
        let frame = one_frame(&product, payload);
        let observer = product.failure_observer();
        drop(product);
        drop(frame);
        let operation = if fault == Fault::FrameDropPanic {
            AudioDecoderProductOperation::DestroyFrame
        } else {
            AudioDecoderProductOperation::DestroyDecoder
        };
        assert_eq!(
            observer.failure(),
            Some(error(operation, AudioDecoderProductFailureKind::Panicked))
        );
        assert!(observer.is_disabled());
        let transfer = evidence.transfer_observers.lock().unwrap()[0].clone();
        if fault == Fault::FrameDropPanic {
            assert_eq!(
                transfer.failure(),
                Some(error(
                    AudioDecoderProductOperation::DestroyFrame,
                    AudioDecoderProductFailureKind::Panicked
                ))
            );
            assert!(transfer.is_disabled());
        } else {
            assert_eq!(transfer.failure(), None);
        }
        assert_eq!(
            evidence.events.lock().unwrap().last(),
            Some(&"decoder-drop")
        );
    }
}
