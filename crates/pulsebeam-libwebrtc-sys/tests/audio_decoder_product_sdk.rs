//! Actual standalone SDK decoder virtuals. Explicit internal legacy refusal is
//! privately witnessed in the module; no public legacy or installed parity.
use pulsebeam_webrtc_sys::{
    AudioDecoder, AudioDecoderError, AudioDecoderFactory, AudioDecoderFrameProduct,
    AudioDecoderFrameProvider, AudioDecoderProductError, AudioDecoderProductFailureKind,
    AudioDecoderProductOperation, AudioDecoderProvider, AudioDecoderProviderParseResult,
    AudioEncoderFactory, CallerAudioDecoder, DecodableAudioFrame, DecodedAudio, Environment,
    ManualClock, SdpAudioFormat,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
#[derive(Clone, Copy)]
enum Mode {
    Normal,
    NullFrame,
    ParseError,
    ParsePanic,
    EmptyParse,
    PlcError,
    PlcPanic,
    ShortPlc,
    ResetError,
    GetterPanic,
    RawMetadata,
}
#[derive(Default)]
struct Evidence {
    calls: Mutex<Vec<&'static str>>,
    bytes: Mutex<Vec<Vec<u8>>>,
    drops: Mutex<Vec<&'static str>>,
    plcs: AtomicUsize,
}
struct Provider {
    native: AudioDecoder,
    environment: Environment,
    evidence: Arc<Evidence>,
    mode: Mode,
}
impl Drop for Provider {
    fn drop(&mut self) {
        assert_eq!(self.environment.timestamp().microseconds().unwrap(), 17000);
        self.evidence.drops.lock().unwrap().push("decoder");
    }
}
impl AudioDecoderProvider for Provider {
    fn sample_rate_hz(&mut self) -> i32 {
        self.evidence.calls.lock().unwrap().push("rate");
        match self.mode {
            Mode::GetterPanic => panic!("SDK rate callback"),
            Mode::RawMetadata => i32::MIN,
            _ => self.native.sample_rate_hz(),
        }
    }
    fn channels(&mut self) -> usize {
        self.evidence.calls.lock().unwrap().push("channels");
        if matches!(self.mode, Mode::RawMetadata) {
            usize::MAX
        } else {
            self.native.channels()
        }
    }
    fn error_code(&mut self) -> i32 {
        self.evidence.calls.lock().unwrap().push("error-code");
        if matches!(self.mode, Mode::RawMetadata) {
            i32::MIN
        } else {
            self.native.error_code()
        }
    }
    fn reset(&mut self) -> Result<(), AudioDecoderError> {
        self.evidence.calls.lock().unwrap().push("reset");
        if matches!(self.mode, Mode::ResetError) {
            Err(AudioDecoderError::NativeInvariant)
        } else {
            self.native.reset();
            Ok(())
        }
    }
    fn packet_duration(&mut self, encoded: &[u8]) -> i32 {
        self.evidence.calls.lock().unwrap().push("duration");
        self.evidence.bytes.lock().unwrap().push(encoded.to_vec());
        if matches!(self.mode, Mode::RawMetadata) {
            i32::MIN
        } else {
            self.native.packet_info(encoded).unwrap().duration
        }
    }
    fn redundant_packet_duration(&mut self, encoded: &[u8]) -> i32 {
        self.evidence.calls.lock().unwrap().push("redundant");
        self.evidence.bytes.lock().unwrap().push(encoded.to_vec());
        if matches!(self.mode, Mode::RawMetadata) {
            -1
        } else {
            self.native.packet_info(encoded).unwrap().redundant_duration
        }
    }
    fn packet_has_fec(&mut self, encoded: &[u8]) -> bool {
        self.evidence.calls.lock().unwrap().push("fec");
        self.evidence.bytes.lock().unwrap().push(encoded.to_vec());
        self.native.packet_info(encoded).unwrap().has_fec
    }
    fn parse_payload(
        &mut self,
        encoded: Vec<u8>,
        timestamp: u32,
    ) -> Result<Vec<AudioDecoderProviderParseResult>, AudioDecoderError> {
        self.evidence.calls.lock().unwrap().push("parse");
        self.evidence.bytes.lock().unwrap().push(encoded.clone());
        match self.mode {
            Mode::EmptyParse => return Ok(Vec::new()), // Actual caller disposition, not a PCMU packet-decoding claim.
            Mode::ParseError => return Err(AudioDecoderError::InvalidPayload),
            Mode::ParsePanic => panic!("SDK parse callback"),
            _ => (),
        }
        let mut parsed: Vec<_> = self
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
                    })
                }),
            })
            .collect();
        if matches!(self.mode, Mode::NullFrame) {
            parsed.push(AudioDecoderProviderParseResult {
                timestamp: timestamp.wrapping_add(80),
                priority: i32::MAX,
                frame: None,
            });
        }
        Ok(parsed)
    }
    fn generate_plc(
        &mut self,
        requested: usize,
        prefix: &[i16],
        _: usize,
    ) -> Result<Vec<i16>, AudioDecoderError> {
        self.evidence.calls.lock().unwrap().push("plc");
        self.evidence.plcs.fetch_add(1, Ordering::SeqCst);
        match self.mode {
            Mode::PlcError => return Err(AudioDecoderError::InvalidCapacity),
            Mode::PlcPanic => panic!("SDK PLC callback"),
            Mode::ShortPlc => return Ok(vec![42]),
            _ => (),
        }
        let mut buffer = prefix.to_vec();
        self.native.generate_plc(requested, &mut buffer)?;
        assert_eq!(&buffer[..prefix.len()], prefix);
        Ok(buffer.split_off(prefix.len()))
    }
}
struct Frame {
    native: DecodableAudioFrame,
    environment: Environment,
    evidence: Arc<Evidence>,
}
impl Drop for Frame {
    fn drop(&mut self) {
        assert_eq!(self.environment.timestamp().microseconds().unwrap(), 17000);
        self.evidence.drops.lock().unwrap().push("frame");
    }
}
impl AudioDecoderFrameProvider for Frame {
    fn duration(&mut self) -> usize {
        self.native.duration()
    }
    fn is_dtx_packet(&mut self) -> bool {
        self.native.is_dtx_packet()
    }
    fn decode(&mut self, _: usize) -> Result<Option<DecodedAudio>, AudioDecoderError> {
        self.native.decode()
    }
}
fn setup(mode: Mode) -> (CallerAudioDecoder, Vec<u8>, Arc<Evidence>) {
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
    assert_eq!(encoder.frames_in_next_packet(), 2);
    assert!(encoder.encode(100, &[0; 80]).unwrap().bytes.is_empty());
    let payload = encoder.encode(180, &[0; 80]).unwrap().bytes;
    let decoder_factory = AudioDecoderFactory::builtin().unwrap();
    let native = decoder_factory
        .create(&environment, &format)
        .unwrap()
        .unwrap();
    let evidence = Arc::new(Evidence::default());
    let decoder = CallerAudioDecoder::from_provider(
        &environment,
        Provider {
            native,
            environment: environment.clone(),
            evidence: evidence.clone(),
            mode,
        },
    );
    drop(encoder);
    drop(encoder_factory);
    drop(decoder_factory);
    drop(environment);
    drop(clock);
    (decoder, payload, evidence)
}
fn error(
    operation: AudioDecoderProductOperation,
    kind: AudioDecoderProductFailureKind,
) -> AudioDecoderProductError {
    AudioDecoderProductError { operation, kind }
}
#[test]
fn real_sdk_named_metadata_reset_and_packet_members_dispatch_independently() {
    let (decoder, payload, evidence) = setup(Mode::Normal);
    assert_eq!(decoder.sample_rate_hz_via_sdk(), Ok(8000));
    assert_eq!(decoder.channels_via_sdk(), Ok(1));
    assert_eq!(decoder.error_code_via_sdk(), Ok(0));
    assert_eq!(decoder.reset_via_sdk(), Ok(()));
    assert_eq!(decoder.packet_duration_via_sdk(&payload), Ok(160));
    // PCMU overrides the base sentinel and delegates redundant duration to
    // its ordinary PacketDuration, even though this payload has no FEC.
    assert_eq!(decoder.redundant_packet_duration_via_sdk(&payload), Ok(160));
    assert_eq!(decoder.packet_has_fec_via_sdk(&payload), Ok(false));
    assert_eq!(
        *evidence.calls.lock().unwrap(),
        vec![
            "rate",
            "channels",
            "error-code",
            "reset",
            "duration",
            "redundant",
            "fec"
        ]
    );
    assert_eq!(
        *evidence.bytes.lock().unwrap(),
        vec![payload.clone(), payload.clone(), payload]
    );
}
#[test]
fn sdk_parse_roundtrips_exact_nullable_metadata_and_preserves_frames_after_decoder_retirement() {
    let (decoder, payload, evidence) = setup(Mode::NullFrame);
    let observer = decoder.failure_observer();
    let parsed = decoder
        .parse_payload_via_sdk(payload.clone(), u32::MAX - 79)
        .unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].timestamp, u32::MAX - 79);
    assert_eq!(parsed[0].priority, 0);
    assert_eq!(parsed[1].timestamp, 0);
    assert_eq!(parsed[1].priority, i32::MAX);
    assert!(parsed[1].frame.is_none());
    assert_eq!(*evidence.calls.lock().unwrap(), vec!["parse"]);
    assert_eq!(*evidence.bytes.lock().unwrap(), vec![payload]);
    drop(decoder);
    assert!(evidence.drops.lock().unwrap().is_empty());
    std::thread::spawn(move || {
        let frame = parsed[0].frame.as_ref().unwrap();
        assert_eq!(frame.duration_via_sdk(), Ok(160));
        let mut output = [123; 176];
        assert_eq!(
            frame
                .decode_into_via_sdk(&mut output)
                .unwrap()
                .unwrap()
                .num_decoded_samples,
            160
        );
        assert_eq!(&output[..160], &[0; 160]);
        assert_eq!(&output[160..], &[123; 16]);
    })
    .join()
    .unwrap();
    assert_eq!(*evidence.drops.lock().unwrap(), vec!["frame", "decoder"]);
    assert_eq!(observer.failure(), None);
}
#[test]
fn native_zero_length_frame_empty_provider_and_typed_failure_remain_distinct() {
    let (decoder, _, _) = setup(Mode::Normal);
    // Pinned SplitBySamples emits one zero-duration frame for empty PCM input;
    // the original guarded native wrapper retains non-null Buffer capacity.
    let parsed = decoder.parse_payload_via_sdk(Vec::new(), 0).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].timestamp, 0);
    assert_eq!(parsed[0].priority, 0);
    assert_eq!(parsed[0].frame.as_ref().unwrap().duration_via_sdk(), Ok(0));
    let (decoder, payload, _) = setup(Mode::EmptyParse);
    assert!(
        decoder
            .parse_payload_via_sdk(payload, 0)
            .unwrap()
            .is_empty()
    );
    let (decoder, payload, _) = setup(Mode::ParseError);
    assert!(
        matches!(decoder.parse_payload_via_sdk(payload, 0), Err(e) if e == error(
        AudioDecoderProductOperation::ParsePayload, AudioDecoderProductFailureKind::Provider(AudioDecoderError::InvalidPayload)))
    );
    let (decoder, payload, _) = setup(Mode::ParsePanic);
    assert!(
        matches!(decoder.parse_payload_via_sdk(payload, 0), Err(e) if e == error(
        AudioDecoderProductOperation::ParsePayload, AudioDecoderProductFailureKind::Panicked))
    );
    assert_eq!(
        decoder.channels_via_sdk(),
        Err(error(
            AudioDecoderProductOperation::Channels,
            AudioDecoderProductFailureKind::Disabled
        ))
    );
}
#[test]
fn sdk_plc_preserves_prefix_no_output_and_explicit_capacity_failure_receipts() {
    let (decoder, _, evidence) = setup(Mode::Normal);
    let mut prefix = vec![-12, 3, 32767];
    decoder
        .generate_plc_via_sdk(usize::MAX, &mut prefix, 0)
        .unwrap(); // Actual PCM no-output, not fabricated PLC.
    assert_eq!(prefix, vec![-12, 3, 32767]);
    assert_eq!(evidence.plcs.load(Ordering::SeqCst), 1);
    assert_eq!(
        decoder.generate_plc_via_sdk(80, &mut prefix, usize::MAX),
        Err(error(
            AudioDecoderProductOperation::GeneratePlc,
            AudioDecoderProductFailureKind::InvalidCapacity
        ))
    );
    assert_eq!(evidence.plcs.load(Ordering::SeqCst), 1);
    assert_eq!(prefix, vec![-12, 3, 32767]);
    for mode in [Mode::PlcError, Mode::PlcPanic, Mode::ShortPlc] {
        let (decoder, _, _) = setup(mode);
        let mut prefix = vec![44, 5];
        let kind = match mode {
            Mode::PlcError => {
                AudioDecoderProductFailureKind::Provider(AudioDecoderError::InvalidCapacity)
            }
            Mode::PlcPanic => AudioDecoderProductFailureKind::Panicked,
            _ => AudioDecoderProductFailureKind::InvalidOutput,
        };
        assert_eq!(
            decoder.generate_plc_via_sdk(80, &mut prefix, 80),
            Err(error(AudioDecoderProductOperation::GeneratePlc, kind))
        );
        assert_eq!(prefix, vec![44, 5]);
    }
}
#[test]
fn sdk_getter_and_reset_failures_do_not_expose_scalar_or_void_success() {
    let (decoder, _, _) = setup(Mode::GetterPanic);
    assert_eq!(
        decoder.sample_rate_hz_via_sdk(),
        Err(error(
            AudioDecoderProductOperation::SampleRate,
            AudioDecoderProductFailureKind::Panicked
        ))
    );
    assert_eq!(
        decoder.error_code_via_sdk(),
        Err(error(
            AudioDecoderProductOperation::ErrorCode,
            AudioDecoderProductFailureKind::Disabled
        ))
    );
    let (decoder, _, _) = setup(Mode::ResetError);
    assert_eq!(
        decoder.reset_via_sdk(),
        Err(error(
            AudioDecoderProductOperation::Reset,
            AudioDecoderProductFailureKind::Provider(AudioDecoderError::NativeInvariant)
        ))
    );
    assert_eq!(decoder.sample_rate_hz_via_sdk(), Ok(8000)); // Fresh receipt, not stale reset failure.
}
#[test]
fn sdk_metadata_is_observed_without_stock_rate_channel_or_negative_sentinel_whitelists() {
    let (decoder, _, _) = setup(Mode::RawMetadata);
    assert_eq!(decoder.sample_rate_hz_via_sdk(), Ok(i32::MIN));
    assert_eq!(decoder.channels_via_sdk(), Ok(usize::MAX));
    assert_eq!(decoder.error_code_via_sdk(), Ok(i32::MIN));
    assert_eq!(decoder.packet_duration_via_sdk(&[0xff, 0]), Ok(i32::MIN));
    assert_eq!(
        decoder.redundant_packet_duration_via_sdk(&[0xff, 0]),
        Ok(-1)
    );
    assert_eq!(decoder.failure_observer().failure(), None); // Observation is not native consumer admission.
}
