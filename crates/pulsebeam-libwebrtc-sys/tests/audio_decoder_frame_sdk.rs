//! Real SDK EncodedAudioFrame standalone virtuals with genuine PCM delegation.
//! Controlled callback/output metadata faults are not wire-codec/NetEq evidence.
use pulsebeam_webrtc_sys::{
    AudioDecoder, AudioDecoderError, AudioDecoderFactory, AudioDecoderFrameDecodeResult,
    AudioDecoderFrameProduct, AudioDecoderFrameProvider, AudioDecoderProductError,
    AudioDecoderProductFailureKind, AudioDecoderProductOperation, AudioDecoderProvider,
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
#[derive(Clone, Copy)]
enum Mode {
    Real,
    None,
    Error,
    PanicDecode,
    PanicDuration,
    PanicDtx,
    TooLong,
    Short,
    Empty,
    DurationUnknown,
    SpeechValue(i32),
    Reenter,
}
#[derive(Default)]
struct Evidence {
    decodes: AtomicUsize,
    durations: AtomicUsize,
    dtx: AtomicUsize,
    events: Mutex<Vec<&'static str>>,
    slot: Mutex<Option<CallerDecodableAudioFrame>>,
    reentries: Mutex<Vec<AudioDecoderProductError>>,
}
struct Decoder {
    native: AudioDecoder,
    environment: Environment,
    evidence: Arc<Evidence>,
    mode: Mode,
}
impl Drop for Decoder {
    fn drop(&mut self) {
        assert_eq!(self.environment.timestamp().microseconds().unwrap(), 17000);
        self.evidence.events.lock().unwrap().push("decoder-drop");
    }
}
impl AudioDecoderProvider for Decoder {
    fn sample_rate_hz(&mut self) -> i32 {
        self.native.sample_rate_hz()
    }
    fn channels(&mut self) -> usize {
        self.native.channels()
    }
    fn reset(&mut self) -> Result<(), AudioDecoderError> {
        self.native.reset();
        Ok(())
    }
    fn generate_plc(
        &mut self,
        requested: usize,
        prefix: &[i16],
        _: usize,
    ) -> Result<Vec<i16>, AudioDecoderError> {
        let mut buffer = prefix.to_vec();
        self.native.generate_plc(requested, &mut buffer)?;
        Ok(buffer.split_off(prefix.len()))
    }
    fn parse_payload(
        &mut self,
        encoded: Vec<u8>,
        timestamp: u32,
    ) -> Result<Vec<AudioDecoderProviderParseResult>, AudioDecoderError> {
        Ok(self
            .native
            .parse_payload(encoded, timestamp)?
            .into_iter()
            .map(|item| AudioDecoderProviderParseResult {
                timestamp: item.timestamp,
                priority: item.priority,
                frame: item.frame.map(|native| {
                    AudioDecoderFrameProduct::from_provider(Frame {
                        native,
                        environment: self.environment.clone(),
                        evidence: self.evidence.clone(),
                        mode: self.mode,
                    })
                }),
            })
            .collect())
    }
}
struct Frame {
    native: DecodableAudioFrame,
    environment: Environment,
    evidence: Arc<Evidence>,
    mode: Mode,
}
impl Drop for Frame {
    fn drop(&mut self) {
        assert_eq!(self.environment.timestamp().microseconds().unwrap(), 17000);
        self.evidence.events.lock().unwrap().push("frame-drop");
    }
}
impl AudioDecoderFrameProvider for Frame {
    fn duration(&mut self) -> usize {
        self.evidence.durations.fetch_add(1, Ordering::SeqCst);
        match self.mode {
            Mode::PanicDuration => panic!("SDK Duration"),
            Mode::DurationUnknown => 0,
            _ => self.native.duration(),
        }
    }
    fn is_dtx_packet(&mut self) -> bool {
        self.evidence.dtx.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, Mode::PanicDtx) {
            panic!("SDK IsDtxPacket");
        }
        self.native.is_dtx_packet()
    }
    fn decode(&mut self, _: usize) -> Result<Option<DecodedAudio>, AudioDecoderError> {
        self.evidence.decodes.fetch_add(1, Ordering::SeqCst);
        match self.mode {
            Mode::None => return Ok(None),
            Mode::Error => return Err(AudioDecoderError::NativeInvariant),
            Mode::PanicDecode => panic!("SDK Decode"),
            _ => (),
        }
        if matches!(self.mode, Mode::Reenter) {
            let slot = self.evidence.slot.lock().unwrap();
            let e = slot.as_ref().unwrap().duration_via_sdk().unwrap_err();
            self.evidence.reentries.lock().unwrap().push(e);
        }
        let mut output = self.native.decode()?;
        if let Some(output) = &mut output {
            match self.mode {
                Mode::TooLong => output.samples.push(7), // Deliberate owned-count fault.
                Mode::Short => output.samples.truncate(80),
                Mode::Empty => output.samples.clear(),
                Mode::SpeechValue(value) => output.speech_type = AudioSpeechType(value), // Standalone enum-value transport, not a claim about PCM speech semantics.
                _ => (),
            }
        }
        Ok(output)
    }
}
fn setup(mode: Mode) -> (CallerDecodableAudioFrame, Arc<Evidence>) {
    let clock = ManualClock::new(Duration::from_millis(17)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let format = SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1);
    let factory = AudioEncoderFactory::builtin().unwrap();
    let encoder = factory
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
    // Pinned G711 SdpToConfig defaults to 20 ms; EncodeImpl accumulates two
    // 10-ms blocks before emitting. Do not confuse sample coding with packet size.
    assert_eq!(encoder.frames_in_next_packet(), 2);
    assert!(encoder.encode(100, &[0; 80]).unwrap().bytes.is_empty());
    let payload = encoder.encode(180, &[0; 80]).unwrap().bytes;
    let factory2 = AudioDecoderFactory::builtin().unwrap();
    let native = factory2.create(&environment, &format).unwrap().unwrap();
    let evidence = Arc::new(Evidence::default());
    let decoder = CallerAudioDecoder::from_provider(
        &environment,
        Decoder {
            native,
            environment: environment.clone(),
            evidence: evidence.clone(),
            mode,
        },
    );
    let mut parsed = decoder.parse_payload(payload, u32::MAX - 79).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].timestamp, u32::MAX - 79);
    assert_eq!(parsed[0].priority, 0);
    let frame = parsed.remove(0).frame.unwrap();
    drop(decoder);
    drop(encoder);
    drop(factory);
    drop(factory2);
    drop(environment);
    drop(clock);
    (frame, evidence)
}
fn error(
    operation: AudioDecoderProductOperation,
    kind: AudioDecoderProductFailureKind,
) -> AudioDecoderProductError {
    AudioDecoderProductError { operation, kind }
}
#[test]
fn real_sdk_frame_virtuals_preserve_owned_lifetime_exact_capacity_and_untouched_tail() {
    let (frame, evidence) = setup(Mode::Real);
    let observer = frame.failure_observer();
    std::thread::spawn(move || {
        assert_eq!(frame.duration_via_sdk(), Ok(160));
        assert_eq!(frame.duration_via_sdk(), Ok(160));
        assert_eq!(frame.is_dtx_packet_via_sdk(), Ok(false));
        let mut too_small = [123; 159];
        assert_eq!(
            frame.decode_into_via_sdk(&mut too_small),
            Err(error(
                AudioDecoderProductOperation::FrameDecode,
                AudioDecoderProductFailureKind::InvalidCapacity
            ))
        );
        assert_eq!(too_small, [123; 159]);
        let mut output = [321; 176];
        assert_eq!(
            frame.decode_into_via_sdk(&mut output),
            Ok(Some(AudioDecoderFrameDecodeResult {
                num_decoded_samples: 160,
                speech_type: AudioSpeechType::SPEECH,
            }))
        );
        assert_eq!(&output[..160], &[0; 160]);
        assert_eq!(&output[160..], &[321; 16]);
        assert_eq!(
            frame.decode(176),
            Err(error(
                AudioDecoderProductOperation::FrameDecode,
                AudioDecoderProductFailureKind::FrameAlreadyDecoded
            ))
        );
        drop(frame);
    })
    .join()
    .unwrap();
    assert_eq!(evidence.decodes.load(Ordering::SeqCst), 1);
    assert_eq!(evidence.durations.load(Ordering::SeqCst), 4); // Two SDK reads plus preflight of each capacity attempt, no cache.
    assert_eq!(evidence.dtx.load(Ordering::SeqCst), 1);
    assert_eq!(
        *evidence.events.lock().unwrap(),
        vec!["frame-drop", "decoder-drop"]
    );
    assert!(!observer.is_disabled());
}
#[test]
fn none_error_panic_and_malformed_owned_count_do_not_modify_the_caller_span() {
    for mode in [Mode::None, Mode::Error, Mode::PanicDecode, Mode::TooLong] {
        let (frame, evidence) = setup(mode);
        let mut output = [77; 160];
        let expected = match mode {
            Mode::None => Ok(None),
            Mode::Error => Err(error(
                AudioDecoderProductOperation::FrameDecode,
                AudioDecoderProductFailureKind::Provider(AudioDecoderError::NativeInvariant),
            )),
            Mode::PanicDecode => Err(error(
                AudioDecoderProductOperation::FrameDecode,
                AudioDecoderProductFailureKind::Panicked,
            )),
            _ => Err(error(
                AudioDecoderProductOperation::FrameDecode,
                AudioDecoderProductFailureKind::InvalidOutput,
            )),
        };
        assert_eq!(frame.decode_into_via_sdk(&mut output), expected);
        assert_eq!(output, [77; 160]);
        let second = if matches!(mode, Mode::PanicDecode) {
            AudioDecoderProductFailureKind::Disabled
        } else {
            AudioDecoderProductFailureKind::FrameAlreadyDecoded
        };
        assert_eq!(
            frame.decode_into_via_sdk(&mut output),
            Err(error(AudioDecoderProductOperation::FrameDecode, second))
        );
        assert_eq!(output, [77; 160]);
        assert_eq!(evidence.decodes.load(Ordering::SeqCst), 1);
    }
}
#[test]
fn sdk_getter_panic_is_typed_not_unknown_duration_or_false_dtx_success() {
    let (frame, _) = setup(Mode::PanicDuration);
    assert_eq!(
        frame.duration_via_sdk(),
        Err(error(
            AudioDecoderProductOperation::FrameDuration,
            AudioDecoderProductFailureKind::Panicked
        ))
    );
    assert_eq!(
        frame.is_dtx_packet_via_sdk(),
        Err(error(
            AudioDecoderProductOperation::FrameDtx,
            AudioDecoderProductFailureKind::Disabled
        ))
    );
    let (frame, _) = setup(Mode::PanicDtx);
    assert_eq!(
        frame.is_dtx_packet_via_sdk(),
        Err(error(
            AudioDecoderProductOperation::FrameDtx,
            AudioDecoderProductFailureKind::Panicked
        ))
    );
}
#[test]
fn short_empty_and_unknown_duration_follow_the_sdk_without_a_decoded_count_floor() {
    for mode in [Mode::Short, Mode::Empty, Mode::DurationUnknown] {
        let (frame, evidence) = setup(mode);
        let mut output = [88; 176];
        if matches!(mode, Mode::DurationUnknown) {
            assert_eq!(frame.duration_via_sdk(), Ok(0));
        }
        let count = match mode {
            Mode::Short => 80,
            Mode::Empty => 0,
            _ => 160,
        };
        assert_eq!(
            frame
                .decode_into_via_sdk(&mut output)
                .unwrap()
                .unwrap()
                .num_decoded_samples,
            count
        );
        assert_eq!(&output[..count], vec![0; count].as_slice());
        assert_eq!(&output[count..], vec![88; 176 - count].as_slice());
        assert_eq!(evidence.decodes.load(Ordering::SeqCst), 1);
    }
}
#[test]
fn open_rust_speech_values_are_guarded_by_exact_native_enum_cast_domain_before_copy() {
    for value in [0, 1, 2, 3] {
        let (frame, _) = setup(Mode::SpeechValue(value));
        let mut output = [91; 160];
        assert_eq!(
            frame
                .decode_into_via_sdk(&mut output)
                .unwrap()
                .unwrap()
                .speech_type,
            AudioSpeechType(value)
        );
        assert_eq!(output, [0; 160]);
    }
    for value in [i32::MIN, -1, 4, i32::MAX] {
        let (frame, evidence) = setup(Mode::SpeechValue(value));
        let observer = frame.failure_observer();
        let mut output = [92; 160];
        let expected = error(
            AudioDecoderProductOperation::FrameDecode,
            AudioDecoderProductFailureKind::InvalidOutput,
        );
        assert_eq!(frame.decode_into_via_sdk(&mut output), Err(expected));
        assert_eq!(output, [92; 160]);
        assert_eq!(observer.failure(), Some(expected));
        assert!(!observer.is_disabled());
        assert_eq!(evidence.decodes.load(Ordering::SeqCst), 1);
        assert_eq!(
            frame.decode(160),
            Err(error(
                AudioDecoderProductOperation::FrameDecode,
                AudioDecoderProductFailureKind::FrameAlreadyDecoded
            ))
        );
    }
}
#[test]
fn concurrent_sdk_drivers_share_the_original_single_shot_state_and_isolate_receipts() {
    let (frame, evidence) = setup(Mode::Real);
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let frame = frame.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut span = [93; 160];
                barrier.wait();
                (frame.decode_into_via_sdk(&mut span), span)
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|(r, _)| r.is_ok()).count(), 1);
    for (result, span) in results {
        match result {
            Ok(Some(info)) => {
                assert_eq!(info.num_decoded_samples, 160);
                assert_eq!(span, [0; 160]);
            }
            Err(e) => {
                assert_eq!(
                    e,
                    error(
                        AudioDecoderProductOperation::FrameDecode,
                        AudioDecoderProductFailureKind::FrameAlreadyDecoded
                    )
                );
                assert_eq!(span, [93; 160]);
            }
            _ => panic!("unexpected decode disposition"),
        }
    }
    assert_eq!(evidence.decodes.load(Ordering::SeqCst), 1);
}

#[test]
fn sdk_frame_reentry_receipt_is_per_operation_and_does_not_replace_outer_real_output() {
    let (frame, evidence) = setup(Mode::Reenter);
    *evidence.slot.lock().unwrap() = Some(frame.clone());
    let mut output = [95; 160];
    assert_eq!(
        frame
            .decode_into_via_sdk(&mut output)
            .unwrap()
            .unwrap()
            .num_decoded_samples,
        160
    );
    assert_eq!(output, [0; 160]);
    let e = error(
        AudioDecoderProductOperation::FrameDuration,
        AudioDecoderProductFailureKind::ReentrantCall,
    );
    assert_eq!(*evidence.reentries.lock().unwrap(), vec![e]);
    assert_eq!(frame.failure_observer().failure(), Some(e));
    assert!(!frame.failure_observer().is_disabled());
    assert_eq!(frame.duration_via_sdk(), Ok(160));
    evidence.slot.lock().unwrap().take(); // Break temporary application cycle.
}

#[test]
fn rust_first_decode_consumes_the_same_attempt_seen_by_subsequent_sdk_driver() {
    let (frame, evidence) = setup(Mode::Real);
    assert_eq!(frame.decode(160).unwrap().unwrap().samples.len(), 160);
    let mut output = [96; 160];
    assert_eq!(
        frame.decode_into_via_sdk(&mut output),
        Err(error(
            AudioDecoderProductOperation::FrameDecode,
            AudioDecoderProductFailureKind::FrameAlreadyDecoded
        ))
    );
    assert_eq!(output, [96; 160]);
    assert_eq!(evidence.decodes.load(Ordering::SeqCst), 1);
    assert_eq!(evidence.durations.load(Ordering::SeqCst), 1);
}
