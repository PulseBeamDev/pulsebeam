//! Actual native nonzero float-planar downmix/resampling in both APM directions.
use pulsebeam_webrtc_sys::*;
use serde::Serialize;
use std::{fmt::Debug, time::Duration};

const BLOCKS: usize = 32;
const INPUT_SEEDS: [[u32; 2]; 2] = [[0x1549_7ac3, 0x9234_5def], [0x8273_6519, 0x51ab_9827]];

fn serialize_debug<T: Debug, S: serde::Serializer>(
    value: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format!("{value:?}"))
}

#[derive(Debug, PartialEq, Serialize)]
struct Processed {
    input_bits: Vec<u32>,
    initial_output_bits: Vec<u32>,
    output_bits: Vec<u32>,
    status: i32,
    #[serde(serialize_with = "serialize_debug")]
    format: AudioProcessingFormatState,
}

#[derive(Debug, PartialEq, Serialize)]
struct Frame {
    reverse: Processed,
    capture: Processed,
}

#[derive(Debug, PartialEq, Serialize)]
struct Observation {
    #[serde(serialize_with = "serialize_debug")]
    formats: AudioProcessingFormats,
    #[serde(serialize_with = "serialize_debug")]
    requested_configuration: AudioProcessorConfig,
    #[serde(serialize_with = "serialize_debug")]
    actual_configuration: AudioProcessorConfig,
    initialization_status: i32,
    #[serde(serialize_with = "serialize_debug")]
    initial_format: AudioProcessingFormatState,
    input_seeds: [[u32; 2]; 2],
    input_final_states: [[u32; 2]; 2],
    frames: Vec<Frame>,
}

fn format(processor: &AudioProcessingModule) -> AudioProcessingFormatState {
    let state = processor.format_state();
    assert_eq!(state.input_channels, 2);
    assert_eq!(state.processing_channels, 1);
    assert_eq!(state.output_channels, 1);
    state
}

fn process(
    processor: &mut AudioProcessingModule,
    input_stream: AudioProcessingStream,
    output_stream: AudioProcessingStream,
    states: &mut [u32; 2],
    reverse: bool,
) -> Processed {
    let input_frames = (input_stream.sample_rate_hz / 100) as usize;
    let output_frames = (output_stream.sample_rate_hz / 100) as usize;
    let mut input = Vec::with_capacity(input_frames * 2 + 9);
    // Channel-major normalized fixed caller PCM, not runtime entropy or DSP.
    for state in states {
        for _ in 0..input_frames {
            *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            input.push((((*state >> 16) & 0x3fff) as i32 - 8192) as f32 / 32768.0);
        }
    }
    assert_ne!(
        &input[..input_frames],
        &input[input_frames..input_frames * 2]
    );
    input.extend_from_slice(&[-0.0; 9]);
    let mut output = vec![0.5_f32; output_frames];
    output.extend_from_slice(&[-0.25; 9]);
    let initial_output_bits = output.iter().map(|v| v.to_bits()).collect();
    // This selects a caller operation, never a runtime-dependent implementation.
    let status = if reverse {
        processor.process_reverse_planar(input_stream, &input, output_stream, &mut output)
    } else {
        processor.process_planar(input_stream, &input, output_stream, &mut output)
    }
    .unwrap();
    assert_eq!(status, AudioProcessingStatus::OK);
    assert_eq!(&output[output_frames..], &[-0.25; 9]);
    let prefix = &output[..output_frames];
    assert!(prefix.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
    assert!(prefix.iter().any(|v| v.abs() > 0.0));
    assert!(prefix.iter().any(|v| v.to_bits() != 0.5_f32.to_bits()));
    assert!(prefix.windows(2).any(|v| v[0].to_bits() != v[1].to_bits()));
    Processed {
        input_bits: input.iter().map(|v| v.to_bits()).collect(),
        initial_output_bits,
        output_bits: output.iter().map(|v| v.to_bits()).collect(),
        status: status.0,
        format: format(processor),
    }
}

fn application<R: SessionRuntime>(runtime: R) -> Observation {
    let factory = runtime.environment_factory().unwrap();
    let environment = runtime.create_environment(&factory).unwrap();
    let mut requested_configuration = AudioProcessorConfig::default();
    requested_configuration.pre_amplifier.enabled = false;
    requested_configuration.capture_level_adjustment.enabled = false;
    requested_configuration.high_pass_filter.enabled = false;
    requested_configuration.echo_canceller.enabled = false;
    requested_configuration.noise_suppression.enabled = false;
    requested_configuration.transient_suppression.enabled = false;
    requested_configuration.gain_controller1.enabled = false;
    requested_configuration.gain_controller2.enabled = false;
    let mut processor =
        AudioProcessingModule::builtin_with_config(&environment, &requested_configuration).unwrap();
    drop(factory);
    drop(environment);
    let formats = AudioProcessingFormats {
        input: AudioProcessingStream {
            sample_rate_hz: 48000,
            channels: 2,
        },
        output: AudioProcessingStream {
            sample_rate_hz: 16000,
            channels: 1,
        },
        reverse_input: AudioProcessingStream {
            sample_rate_hz: 16000,
            channels: 2,
        },
        reverse_output: AudioProcessingStream {
            sample_rate_hz: 48000,
            channels: 1,
        },
    };
    let status = processor.initialize_with_formats(formats).unwrap();
    assert_eq!(status, AudioProcessingStatus::OK);
    let actual_configuration = processor.configuration();
    assert_eq!(actual_configuration, requested_configuration);
    let initial_format = format(&processor);
    let mut states = INPUT_SEEDS;
    let mut frames = Vec::new();
    for _ in 0..BLOCKS {
        let reverse = process(
            &mut processor,
            formats.reverse_input,
            formats.reverse_output,
            &mut states[1],
            true,
        );
        let capture = process(
            &mut processor,
            formats.input,
            formats.output,
            &mut states[0],
            false,
        );
        frames.push(Frame { reverse, capture });
    }
    drop(processor);
    Observation {
        formats,
        requested_configuration,
        actual_configuration,
        initialization_status: status.0,
        initial_format,
        input_seeds: INPUT_SEEDS,
        input_final_states: states,
        frames,
    }
}

#[test]
fn native_nonzero_multirate_capture_and_reverse_planar_replay() {
    let threaded = application(ThreadedRuntime);
    let managed = std::array::from_fn::<_, 2, _>(|_| {
        eprintln!("CONTROLLED_MEDIA_BEGIN");
        let world = ControlledWorld::acquire(1010, Duration::from_secs(7)).unwrap();
        let observation = application(world.runtime().unwrap());
        assert_eq!(world.now(), Duration::from_secs(7));
        drop(world);
        eprintln!("CONTROLLED_MEDIA_END");
        observation
    });
    if let Some(path) = std::env::var_os("PULSEBEAM_APM_CONVERSION_TRACE_PATH") {
        std::fs::write(path, serde_json::to_vec(&(&threaded, &managed)).unwrap()).unwrap();
    }
    assert_eq!(threaded, managed[0]);
    assert_eq!(managed[0], managed[1]);
}
