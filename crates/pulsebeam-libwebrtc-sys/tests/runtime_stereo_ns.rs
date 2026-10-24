//! Native stereo float-planar NS with linked-channel filtering and full replay.
use pulsebeam_webrtc_sys::*;
use serde::Serialize;
use std::{fmt::Debug, time::Duration};

const WARMUP_BLOCKS: usize = 220;
const WINDOW_BLOCKS: usize = 25;
const WINDOWS: usize = 4;
const INPUT_SEEDS: [u32; 2] = [0x6172_7521, 0x93a4_5217];

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
    suppressed: Processed,
}

#[derive(Debug, PartialEq, Serialize)]
struct EnergyWindow {
    first_block: usize,
    blocks: usize,
    control_energy_bits: [u64; 2],
    suppressed_energy_bits: [u64; 2],
}

#[derive(Debug, PartialEq, Serialize)]
struct Observation {
    #[serde(serialize_with = "serialize_debug")]
    formats: AudioProcessingFormats,
    input_seeds: [u32; 2],
    input_final_states: [u32; 2],
    warmup_blocks: usize,
    control: Module,
    suppressed: Module,
    frames: Vec<Frame>,
    energy_windows: Vec<EnergyWindow>,
}

fn format(processor: &AudioProcessingModule) -> AudioProcessingFormatState {
    let state = processor.format_state();
    assert_eq!(state.processing_rate_hz, 16000);
    assert_eq!(state.split_rate_hz, 16000);
    assert_eq!(state.input_channels, 2);
    assert_eq!(state.processing_channels, 2);
    assert_eq!(state.output_channels, 2);
    state
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
        initial_format: format(processor),
    }
}

fn process(
    processor: &mut AudioProcessingModule,
    stream: AudioProcessingStream,
    input: &[f32],
) -> Processed {
    let mut output = vec![0.5_f32; 320];
    output.extend_from_slice(&[-0.25; 9]);
    let initial_output_bits = output.iter().map(|v| v.to_bits()).collect();
    let status = processor
        .process_planar(stream, input, stream, &mut output)
        .unwrap();
    assert_eq!(status, AudioProcessingStatus::OK);
    assert_eq!(&output[320..], &[-0.25; 9]);
    assert!(
        output[..320]
            .iter()
            .all(|v| v.is_finite() && v.abs() <= 1.0)
    );
    Processed {
        initial_output_bits,
        output_bits: output.iter().map(|v| v.to_bits()).collect(),
        status: status.0,
        format: format(processor),
    }
}

fn energy(bits: &[u32]) -> f64 {
    bits.iter()
        .map(|&v| {
            let v = f64::from(f32::from_bits(v));
            v * v
        })
        .sum()
}

fn application<R: SessionRuntime>(runtime: R) -> Observation {
    let factory = runtime.environment_factory().unwrap();
    let environment = runtime.create_environment(&factory).unwrap();
    let mut control_config = AudioProcessorConfig::default();
    control_config.pipeline.multi_channel_capture = true;
    control_config.pre_amplifier.enabled = false;
    control_config.capture_level_adjustment.enabled = false;
    control_config.high_pass_filter.enabled = false;
    control_config.echo_canceller.enabled = false;
    control_config.transient_suppression.enabled = false;
    control_config.gain_controller1.enabled = false;
    control_config.gain_controller2.enabled = false;
    control_config.noise_suppression.enabled = false;
    control_config.noise_suppression.level = ApmNoiseSuppressionLevel::VeryHigh;
    let mut suppressed_config = control_config;
    suppressed_config.noise_suppression.enabled = true;
    let mut control =
        AudioProcessingModule::builtin_with_config(&environment, &control_config).unwrap();
    let mut suppressed =
        AudioProcessingModule::builtin_with_config(&environment, &suppressed_config).unwrap();
    drop(factory);
    drop(environment);
    let stream = AudioProcessingStream {
        sample_rate_hz: 16000,
        channels: 2,
    };
    let formats = AudioProcessingFormats {
        input: stream,
        output: stream,
        reverse_input: stream,
        reverse_output: stream,
    };
    let control_observation = initialize(&mut control, control_config, formats);
    let suppressed_observation = initialize(&mut suppressed, suppressed_config, formats);
    let mut states = INPUT_SEEDS;
    let mut frames = Vec::new();
    for _ in 0..WARMUP_BLOCKS + WINDOW_BLOCKS * WINDOWS {
        // Distinct fixed caller channels, not runtime entropy or replacement DSP.
        // Channel-major normalized samples are exact binary-scaled integers.
        let mut input = Vec::with_capacity(329);
        for state in &mut states {
            for _ in 0..160 {
                *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                input.push((((*state >> 16) & 0x3fff) as i32 - 8192) as f32 / 32768.0);
            }
        }
        assert_ne!(&input[..160], &input[160..320]);
        input.extend_from_slice(&[-0.0; 9]);
        let control_output = process(&mut control, stream, &input);
        let suppressed_output = process(&mut suppressed, stream, &input);
        frames.push(Frame {
            input_bits: input.iter().map(|v| v.to_bits()).collect(),
            control: control_output,
            suppressed: suppressed_output,
        });
    }
    let mut energy_windows = Vec::new();
    for window in 0..WINDOWS {
        let first_block = WARMUP_BLOCKS + window * WINDOW_BLOCKS;
        let slice = &frames[first_block..first_block + WINDOW_BLOCKS];
        let mut control_energy_bits = [0; 2];
        let mut suppressed_energy_bits = [0; 2];
        for channel in 0..2 {
            let range = channel * 160..(channel + 1) * 160;
            let control_energy = slice
                .iter()
                .map(|f| energy(&f.control.output_bits[range.clone()]))
                .sum::<f64>();
            let suppressed_energy = slice
                .iter()
                .map(|f| energy(&f.suppressed.output_bits[range.clone()]))
                .sum::<f64>();
            assert!(control_energy > 0.0 && suppressed_energy > 0.0);
            assert!(
                suppressed_energy < control_energy,
                "each post-startup native stereo NS output must attenuate against its disabled native control"
            );
            assert!(
                slice.iter().any(|f| f.control.output_bits[range.clone()]
                    != f.suppressed.output_bits[range.clone()])
            );
            control_energy_bits[channel] = control_energy.to_bits();
            suppressed_energy_bits[channel] = suppressed_energy.to_bits();
        }
        // Distinct output is a bounded observation, not channel independence:
        // stock multichannel NS combines channel Wiener filters by their minimum.
        assert!(
            slice
                .iter()
                .any(|f| f.suppressed.output_bits[..160] != f.suppressed.output_bits[160..320])
        );
        energy_windows.push(EnergyWindow {
            first_block,
            blocks: WINDOW_BLOCKS,
            control_energy_bits,
            suppressed_energy_bits,
        });
    }
    drop(control);
    drop(suppressed);
    Observation {
        formats,
        input_seeds: INPUT_SEEDS,
        input_final_states: states,
        warmup_blocks: WARMUP_BLOCKS,
        control: control_observation,
        suppressed: suppressed_observation,
        frames,
        energy_windows,
    }
}

#[test]
fn native_stereo_planar_ns_attenuates_after_startup_and_replays() {
    let threaded = application(ThreadedRuntime);
    let managed = std::array::from_fn::<_, 2, _>(|_| {
        eprintln!("CONTROLLED_MEDIA_BEGIN");
        let world = ControlledWorld::acquire(1009, Duration::from_secs(7)).unwrap();
        let observation = application(world.runtime().unwrap());
        assert_eq!(world.now(), Duration::from_secs(7));
        drop(world);
        eprintln!("CONTROLLED_MEDIA_END");
        observation
    });
    if let Some(path) = std::env::var_os("PULSEBEAM_STEREO_NS_TRACE_PATH") {
        std::fs::write(path, serde_json::to_vec(&(&threaded, &managed)).unwrap()).unwrap();
    }
    assert_eq!(threaded, managed[0]);
    assert_eq!(managed[0], managed[1]);
}
