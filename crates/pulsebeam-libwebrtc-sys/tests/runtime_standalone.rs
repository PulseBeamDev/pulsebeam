//! One standalone application procedure, without a PeerConnectionFactory.
use pulsebeam_webrtc_sys::*;
use serde::Serialize;

#[path = "support/standalone_mixing.rs"]
mod mixing;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

struct Output(Arc<Mutex<Vec<u8>>>);
impl EventLogOutputSink for Output {
    fn is_active(&self) -> bool {
        true
    }
    fn write(&mut self, bytes: &[u8]) -> bool {
        self.0.lock().unwrap().extend_from_slice(bytes);
        true
    }
}

struct Source(Vec<i16>);
impl AudioMixerSourceProvider for Source {
    fn preferred_sample_rate(&mut self) -> Result<i32, AudioMixerError> {
        Ok(8000)
    }
    fn ssrc(&mut self) -> Result<i32, AudioMixerError> {
        Ok(42)
    }
    fn audio_frame(&mut self, rate: i32) -> Result<MixerAudioFrame, AudioMixerError> {
        assert_eq!(rate, 8000);
        let mut frame = AudioFrame::new();
        frame
            .update_frame(
                0,
                Some(&self.0),
                self.0.len(),
                rate,
                AudioFrameSpeechType::NORMAL_SPEECH,
                AudioFrameVadActivity::ACTIVE,
                1,
            )
            .unwrap();
        Ok(MixerAudioFrame::Normal(frame))
    }
}

// Typed equality covers every config member. The optional trace renders the
// complete plain-value Debug snapshot, not the SDK's shorter ToString subset.
fn serialize_config<S: serde::Serializer>(
    config: &AudioProcessorConfig,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format!("{config:?}"))
}

#[derive(Debug, PartialEq, Serialize)]
struct GainObservation {
    #[serde(serialize_with = "serialize_config")]
    configuration: AudioProcessorConfig,
    status: i32,
    input: Vec<i16>,
    output: Vec<i16>,
}

#[derive(Debug, PartialEq, Serialize)]
struct Observation {
    processed: Vec<i16>,
    mixed: Vec<i16>,
    encoded: Vec<u8>,
    decoded: Vec<i16>,
    multi_source: mixing::Observation,
    gain_initialization_status: i32,
    active_processing: Vec<GainObservation>,
}

fn application<R: SessionRuntime>(runtime: R, mut progress: impl FnMut()) -> Observation {
    let mut factory = runtime.environment_factory().unwrap();
    factory
        .set_field_trials(FieldTrials::parse("StandaloneConstruction/Enabled/").unwrap())
        .unwrap();
    let environment = runtime.create_environment(&factory).unwrap();
    assert_eq!(
        environment.field_trials().lookup("StandaloneConstruction"),
        "Enabled"
    );
    // The native products retain the selected context, not this configuring owner.
    drop(factory);

    let mut processor =
        AudioProcessingModule::builtin(&environment, AudioProcessingConfig::default()).unwrap();
    assert_eq!(processor.initialize().unwrap(), AudioProcessingStatus::OK);
    let mut gain_config = AudioProcessorConfig::default();
    gain_config.pre_amplifier.enabled = true;
    gain_config.pre_amplifier.fixed_gain_factor = 2.0;
    gain_config.capture_level_adjustment.enabled = true;
    gain_config.capture_level_adjustment.pre_gain_factor = 3.0;
    gain_config.capture_level_adjustment.post_gain_factor = 0.5;
    let mut gain_processor =
        AudioProcessingModule::builtin_with_config(&environment, &gain_config).unwrap();
    let format = SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1);
    let encoder = AudioEncoderFactory::builtin()
        .unwrap()
        .create(
            &environment,
            &format,
            &AudioEncoderOptions {
                payload_type: 0,
                ..Default::default()
            },
        )
        .unwrap()
        .expect("native PCMU encoder");
    let decoder = AudioDecoderFactory::builtin()
        .unwrap()
        .create(&environment, &format)
        .unwrap()
        .expect("native PCMU decoder");
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let mut logger = RtcEventLogFactoryHandle::builtin()
        .create(&environment)
        .unwrap();
    logger
        .start(
            EventLogOutput::from_sink(Output(bytes.clone())),
            EventLogOutputPeriod::Immediate,
        )
        .unwrap();
    logger
        .log(RtcEvent::from_data(RtcEventData::AlrState { in_alr: true }).unwrap())
        .unwrap();
    drop(environment);

    // Both processing and live replacement use the same native product after
    // the configuring factory and original environment have been released.
    let gain_initialization_status = gain_processor.initialize().unwrap();
    assert_eq!(gain_initialization_status, AudioProcessingStatus::OK);
    let mut active_processing = Vec::new();
    for (pre_gain, expected) in [(3.0, 3000), (2.0, 2000)] {
        if pre_gain != gain_config.capture_level_adjustment.pre_gain_factor {
            gain_config.capture_level_adjustment.pre_gain_factor = pre_gain;
            gain_processor.apply_config(&gain_config).unwrap();
        }
        let configuration = gain_processor.configuration();
        assert_eq!(configuration, gain_config);
        let input = vec![1000; 169];
        let mut output = input.clone();
        let status = gain_processor
            .process_interleaved(
                AudioProcessingStream {
                    sample_rate_hz: 16000,
                    channels: 1,
                },
                &mut output,
            )
            .unwrap();
        assert_eq!(status, AudioProcessingStatus::OK);
        assert_eq!(&output[..160], &[expected; 160]);
        assert_eq!(&output[160..], &input[160..], "caller tail is untouched");
        active_processing.push(GainObservation {
            configuration,
            status: status.0,
            input,
            output,
        });
    }

    let mut processed: Vec<i16> = (0..80).map(|i| (i * 173 - 6800) as i16).collect();
    assert_eq!(
        processor
            .process_interleaved(
                AudioProcessingStream {
                    sample_rate_hz: 8000,
                    channels: 1
                },
                &mut processed,
            )
            .unwrap(),
        AudioProcessingStatus::OK
    );
    let mut mixer = AudioMixer::stock().unwrap();
    let source = AudioMixerSource::from_provider(Source(processed.clone()));
    assert!(mixer.add_source(&source).unwrap());
    drop(source);
    let mixed = mixer.mix(1).unwrap().data_view().unwrap().to_vec();
    assert_eq!(mixed, processed);

    let mut encoded = Vec::new();
    let mut decoded = Vec::new();
    for block in 0..encoder.maximum_frames_in_packet() {
        let packet = encoder.encode((block * 80) as u32, &mixed).unwrap();
        if packet.bytes.is_empty() {
            continue;
        }
        for frame in decoder.parse_payload(packet.bytes.clone(), 0).unwrap() {
            decoded.extend(frame.frame.unwrap().decode().unwrap().unwrap().samples);
        }
        encoded.extend(packet.bytes);
    }
    assert!(!encoded.is_empty());
    assert!(!decoded.is_empty());
    assert!(decoded.iter().any(|&sample| sample != 0));
    // The stock SDK mixer is context-free. Keep its ordinary constructor,
    // inside the same generic native application rather than inventing an
    // environment dependency or a managed-only media procedure.
    let multi_source = mixing::run();
    let mut stop = logger.stop_async().unwrap();
    loop {
        if let Some(result) = stop.try_take() {
            result.unwrap();
            break;
        }
        progress();
    }
    drop(logger);
    assert!(!bytes.lock().unwrap().is_empty(), "actual stock log output");
    Observation {
        processed,
        mixed,
        encoded,
        decoded,
        multi_source,
        gain_initialization_status: gain_initialization_status.0,
        active_processing,
    }
}

#[test]
fn same_standalone_procedure_uses_native_processing_mixing_codecs_and_logging() {
    let deadline = Instant::now() + Duration::from_secs(15);
    let real = application(ThreadedRuntime, || {
        assert!(Instant::now() < deadline, "threaded native stop stalled");
        std::thread::yield_now();
    });
    let mut observations = Vec::new();
    for _ in 0..2 {
        eprintln!("CONTROLLED_MEDIA_BEGIN");
        let world = ControlledWorld::acquire(1001, Duration::from_secs(7)).unwrap();
        let mut turns = 0;
        let managed = application(world.runtime().unwrap(), || {
            turns += 1;
            assert!(turns < 4096, "managed native stop stalled");
            assert!(!world.pump(64).blocked);
        });
        assert_eq!(managed, real);
        assert_eq!(world.now(), Duration::from_secs(7));
        // Retire the final context/products without advancing virtual time.
        drop(world);
        eprintln!("CONTROLLED_MEDIA_END");
        observations.push(managed);
    }
    if let Some(path) = std::env::var_os("PULSEBEAM_STANDALONE_MIXING_TRACE_PATH") {
        std::fs::write(path, serde_json::to_vec(&(&real, &observations)).unwrap()).unwrap();
    }
}

#[test]
fn failed_standalone_configuration_does_not_poison_runtime_authority() {
    let world = ControlledWorld::acquire(1002, Duration::from_secs(7)).unwrap();
    let runtime = world.runtime().unwrap();
    let mut closed = runtime.environment_factory().unwrap();
    closed.close();
    assert_eq!(
        runtime.create_environment(&closed).err(),
        Some(BuildEnvironmentError::Closed)
    );
    let factory = runtime.environment_factory().unwrap();
    let environment = runtime.create_environment(&factory).unwrap();
    assert_eq!(environment.now(), world.now());
    drop(environment);
    drop(factory);
    drop(runtime);
    drop(world);
    let next = ControlledWorld::acquire(1003, Duration::from_secs(9)).unwrap();
    assert_eq!(next.now(), Duration::from_secs(9));
}
