//! Native GC1 startup-floor and full-scale PCM detector feedback, not speech quality.
use pulsebeam_webrtc_sys::*;
use serde::Serialize;
use std::{fmt::Debug, time::Duration};

fn serialize_debug<T: Debug, S: serde::Serializer>(
    value: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format!("{value:?}"))
}

#[derive(Debug, PartialEq, Serialize)]
struct Module {
    #[serde(serialize_with = "serialize_debug")]
    requested_configuration: AudioProcessorConfig,
    #[serde(serialize_with = "serialize_debug")]
    actual_configuration: AudioProcessorConfig,
    initialization_status: i32,
    #[serde(serialize_with = "serialize_debug")]
    initial_format: AudioProcessingFormatState,
}

#[derive(Debug, PartialEq, Serialize)]
struct Processed {
    applied_level: i32,
    #[serde(serialize_with = "serialize_debug")]
    set_level_result: Result<(), AudioProcessorError>,
    recommendation_before: i32,
    recommendation_after: i32,
    initial_output_bits: Vec<u32>,
    output_bits: Vec<u32>,
    status: i32,
    #[serde(serialize_with = "serialize_debug")]
    format: AudioProcessingFormatState,
}

#[derive(Debug, PartialEq, Serialize)]
struct Frame {
    input_bits: Vec<u32>,
    control: Processed,
    active: Processed,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
enum Stimulus {
    StartupFloor,
    FullScaleClipping,
}

#[derive(Debug, PartialEq, Serialize)]
struct Observation {
    stimulus: Stimulus,
    #[serde(serialize_with = "serialize_debug")]
    formats: AudioProcessingFormats,
    control: Module,
    active: Module,
    frames: Vec<Frame>,
}

fn initialize(
    processor: &mut AudioProcessingModule,
    requested_configuration: AudioProcessorConfig,
    formats: AudioProcessingFormats,
) -> Module {
    let status = processor.initialize_with_formats(formats).unwrap();
    assert_eq!(status, AudioProcessingStatus::OK);
    let actual_configuration = processor.configuration();
    assert_eq!(actual_configuration, requested_configuration);
    Module {
        requested_configuration,
        actual_configuration,
        initialization_status: status.0,
        initial_format: processor.format_state(),
    }
}

fn process(
    processor: &mut AudioProcessingModule,
    stream: AudioProcessingStream,
    input: &[f32],
    applied_level: i32,
) -> Processed {
    let set_level_result = processor.set_stream_analog_level(applied_level);
    assert_eq!(set_level_result, Ok(()));
    let recommendation_before = processor.recommended_stream_analog_level();
    let mut output = vec![0.5_f32; 160];
    output.extend_from_slice(&[-0.25; 9]);
    let initial_output_bits = output.iter().map(|v| v.to_bits()).collect();
    let status = processor
        .process_planar(stream, input, stream, &mut output)
        .unwrap();
    assert_eq!(status, AudioProcessingStatus::OK);
    assert_eq!(&output[160..], &[-0.25; 9]);
    assert!(
        output[..160]
            .iter()
            .all(|v| v.is_finite() && v.abs() <= 1.0)
    );
    assert!(
        output[..160]
            .iter()
            .any(|v| v.to_bits() != 0.5_f32.to_bits())
    );
    let recommendation_after = processor.recommended_stream_analog_level();
    assert!((0..=255).contains(&recommendation_after));
    Processed {
        applied_level,
        set_level_result,
        recommendation_before,
        recommendation_after,
        initial_output_bits,
        output_bits: output.iter().map(|v| v.to_bits()).collect(),
        status: status.0,
        format: processor.format_state(),
    }
}

fn application<R: SessionRuntime>(runtime: R, stimulus: Stimulus) -> Observation {
    let factory = runtime.environment_factory().unwrap();
    let environment = runtime.create_environment(&factory).unwrap();
    let mut active_config = AudioProcessorConfig::default();
    active_config.echo_canceller.enabled = false;
    active_config.noise_suppression.enabled = false;
    active_config.gain_controller2.enabled = false;
    active_config.pre_amplifier.enabled = false;
    active_config.capture_level_adjustment.enabled = false;
    active_config.gain_controller1.enabled = true;
    active_config.gain_controller1.mode = ApmGainController1Mode::AdaptiveAnalog;
    active_config
        .gain_controller1
        .analog_gain_controller
        .enabled = true;
    active_config
        .gain_controller1
        .analog_gain_controller
        .enable_digital_adaptive = true;
    let mut control_config = active_config.clone();
    control_config.gain_controller1.enabled = false;
    let mut control =
        AudioProcessingModule::builtin_with_config(&environment, &control_config).unwrap();
    let mut active =
        AudioProcessingModule::builtin_with_config(&environment, &active_config).unwrap();
    drop(factory);
    drop(environment);
    let stream = AudioProcessingStream {
        sample_rate_hz: 16000,
        channels: 1,
    };
    let formats = AudioProcessingFormats {
        input: stream,
        output: stream,
        reverse_input: stream,
        reverse_output: stream,
    };
    let control_observation = initialize(&mut control, control_config, formats);
    let active_observation = initialize(&mut active, active_config, formats);
    if stimulus == Stimulus::FullScaleClipping {
        let analog = active_observation
            .actual_configuration
            .gain_controller1
            .analog_gain_controller;
        assert_eq!(analog.clipped_level_min, 70);
        assert_eq!(analog.clipped_level_step, 15);
        assert_eq!(analog.clipped_ratio_threshold, 0.1);
        assert_eq!(analog.clipped_wait_frames, 300);
        assert!(!analog.clipping_predictor.enabled);
    }
    let initial_level = match stimulus {
        Stimulus::StartupFloor => 3,
        Stimulus::FullScaleClipping => 255,
    };
    let mut control_level = initial_level;
    let mut active_level = initial_level;
    let mut frames = Vec::new();
    for index in 0..4 {
        // Prime the clipping case with silence: AnalyzePreProcess precedes
        // MonoAgc's first Process(), which initializes its stored input level.
        // +1.0 public PCM maps to +32768 internally, above the clip threshold.
        // The repeated full-scale frame also probes immediate wait suppression.
        let full_scale = stimulus == Stimulus::FullScaleClipping && matches!(index, 1 | 2);
        let mut input = vec![if full_scale { 1.0_f32 } else { 0.0 }; 160];
        input.extend_from_slice(&[-0.0; 9]);
        let control_frame = process(&mut control, stream, &input, control_level);
        let active_frame = process(&mut active, stream, &input, active_level);
        assert_eq!(control_frame.recommendation_after, control_level);
        match stimulus {
            Stimulus::StartupFloor if index == 0 => assert!(
                active_frame.recommendation_after > active_level,
                "actual GC1 startup-floor feedback must exceed the supplied low level"
            ),
            Stimulus::FullScaleClipping => assert_eq!(
                active_frame.recommendation_after,
                if index == 0 { 255 } else { 240 },
                "native clipping response after priming, then bounded wait suppression"
            ),
            _ => {}
        }
        // The caller applies the real previous recommendation before the next
        // native block. No synthetic microphone or audio-gain model is added.
        control_level = control_frame.recommendation_after;
        active_level = active_frame.recommendation_after;
        frames.push(Frame {
            input_bits: input.iter().map(|v| v.to_bits()).collect(),
            control: control_frame,
            active: active_frame,
        });
    }
    drop(control);
    drop(active);
    Observation {
        stimulus,
        formats,
        control: control_observation,
        active: active_observation,
        frames,
    }
}

fn replay(stimulus: Stimulus, suffix: &str) {
    let threaded = application(ThreadedRuntime, stimulus);
    let managed = std::array::from_fn::<_, 2, _>(|_| {
        eprintln!("CONTROLLED_MEDIA_BEGIN");
        let world = ControlledWorld::acquire(1008, Duration::from_secs(7)).unwrap();
        let observation = application(world.runtime().unwrap(), stimulus);
        assert_eq!(world.now(), Duration::from_secs(7));
        drop(world);
        eprintln!("CONTROLLED_MEDIA_END");
        observation
    });
    if let Some(mut path) = std::env::var_os("PULSEBEAM_STANDALONE_AGC_TRACE_PATH") {
        path.push(suffix);
        std::fs::write(path, serde_json::to_vec(&(&threaded, &managed)).unwrap()).unwrap();
    }
    assert_eq!(threaded, managed[0]);
    assert_eq!(managed[0], managed[1]);
}

#[test]
fn native_gc1_startup_feedback_and_planar_output_replay() {
    replay(Stimulus::StartupFloor, "");
}

#[test]
fn native_gc1_full_scale_clipping_feedback_and_planar_output_replay() {
    replay(Stimulus::FullScaleClipping, ".clipping");
}
