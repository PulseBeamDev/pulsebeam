//! Stock NS and a disabled stock control, with complete runtime replay.
use pulsebeam_webrtc_sys::*;
use serde::Serialize;
use std::{fmt::Debug, time::Duration};

const WARMUP_BLOCKS: usize = 220;
const WINDOW_BLOCKS: usize = 25;
const WINDOWS: usize = 4;
const INPUT_SEED: u32 = 0x6172_7521;

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
    output: Vec<i16>,
    status: i32,
    #[serde(serialize_with = "serialize_debug")]
    format: AudioProcessingFormatState,
}

#[derive(Debug, PartialEq, Serialize)]
struct Frame {
    input: Vec<i16>,
    control: Processed,
    suppressed: Processed,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct EnergyWindow {
    first_block: usize,
    blocks: usize,
    control: u64,
    suppressed: u64,
}

#[derive(Debug, PartialEq, Serialize)]
struct Observation {
    #[serde(serialize_with = "serialize_debug")]
    formats: AudioProcessingFormats,
    input_seed: u32,
    input_final_state: u32,
    warmup_blocks: usize,
    control: Module,
    suppressed: Module,
    frames: Vec<Frame>,
    energy_windows: Vec<EnergyWindow>,
}

fn module_observation(
    processor: &mut AudioProcessingModule,
    requested_configuration: AudioProcessorConfig,
    formats: AudioProcessingFormats,
) -> Module {
    let initialization_status = processor.initialize_with_formats(formats).unwrap();
    assert_eq!(initialization_status, AudioProcessingStatus::OK);
    let actual_configuration = processor.configuration();
    assert_eq!(actual_configuration, requested_configuration);
    Module {
        requested_configuration,
        actual_configuration,
        initialization_status: initialization_status.0,
        initial_format: processor.format_state(),
    }
}

fn process(
    processor: &mut AudioProcessingModule,
    stream: AudioProcessingStream,
    input: &[i16],
) -> Processed {
    let mut output = input.to_vec();
    let status = processor.process_interleaved(stream, &mut output).unwrap();
    assert_eq!(status, AudioProcessingStatus::OK);
    assert_eq!(&output[160..], &input[160..]);
    Processed {
        output,
        status: status.0,
        format: processor.format_state(),
    }
}

fn energy(samples: &[i16]) -> u64 {
    samples
        .iter()
        .map(|&v| {
            let v = i64::from(v);
            (v * v) as u64
        })
        .sum()
}

fn application<R: SessionRuntime>(runtime: R) -> Observation {
    let factory = runtime.environment_factory().unwrap();
    let environment = runtime.create_environment(&factory).unwrap();
    let mut control_config = AudioProcessorConfig::default();
    control_config.noise_suppression.enabled = false;
    control_config.noise_suppression.level = ApmNoiseSuppressionLevel::VeryHigh;
    let mut suppressed_config = control_config.clone();
    suppressed_config.noise_suppression.enabled = true;
    let mut control =
        AudioProcessingModule::builtin_with_config(&environment, &control_config).unwrap();
    let mut suppressed =
        AudioProcessingModule::builtin_with_config(&environment, &suppressed_config).unwrap();
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
    let control_observation = module_observation(&mut control, control_config, formats);
    let suppressed_observation = module_observation(&mut suppressed, suppressed_config, formats);
    let mut state = INPUT_SEED;
    let mut frames = Vec::new();
    for _ in 0..WARMUP_BLOCKS + WINDOW_BLOCKS * WINDOWS {
        // Fixed caller PCM, not a runtime entropy provider or alternate DSP.
        // Stationary broadband input stays in [-8192, 8191]; all samples are
        // retained so the fixture's recurrence is not a hidden replay input.
        let mut input = (0..160)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (((state >> 16) & 0x3fff) as i32 - 8192) as i16
            })
            .collect::<Vec<_>>();
        input.extend_from_slice(&[789; 9]);
        let control_output = process(&mut control, stream, &input);
        let suppressed_output = process(&mut suppressed, stream, &input);
        frames.push(Frame {
            input,
            control: control_output,
            suppressed: suppressed_output,
        });
    }
    // Measure four windows after both pinned startup phases (50/200 blocks),
    // rather than mistaking the initial FFT overlap delay for suppression.
    let mut energy_windows = Vec::new();
    for window in 0..WINDOWS {
        let first_block = WARMUP_BLOCKS + window * WINDOW_BLOCKS;
        let slice = &frames[first_block..first_block + WINDOW_BLOCKS];
        let control_energy = slice
            .iter()
            .map(|f| energy(&f.control.output[..160]))
            .sum::<u64>();
        let suppressed_energy = slice
            .iter()
            .map(|f| energy(&f.suppressed.output[..160]))
            .sum::<u64>();
        assert!(control_energy > 0 && suppressed_energy > 0);
        assert!(
            suppressed_energy < control_energy,
            "post-warm-up native NS must attenuate stationary input against the disabled native control"
        );
        assert!(
            slice
                .iter()
                .any(|f| f.control.output[..160] != f.suppressed.output[..160])
        );
        energy_windows.push(EnergyWindow {
            first_block,
            blocks: WINDOW_BLOCKS,
            control: control_energy,
            suppressed: suppressed_energy,
        });
    }
    drop(control);
    drop(suppressed);
    Observation {
        formats,
        input_seed: INPUT_SEED,
        input_final_state: state,
        warmup_blocks: WARMUP_BLOCKS,
        control: control_observation,
        suppressed: suppressed_observation,
        frames,
        energy_windows,
    }
}

#[test]
fn native_standalone_ns_attenuates_after_startup_and_replays() {
    let threaded = application(ThreadedRuntime);
    let managed = std::array::from_fn::<_, 2, _>(|_| {
        eprintln!("CONTROLLED_MEDIA_BEGIN");
        let world = ControlledWorld::acquire(1007, Duration::from_secs(7)).unwrap();
        let observation = application(world.runtime().unwrap());
        assert_eq!(world.now(), Duration::from_secs(7));
        drop(world);
        eprintln!("CONTROLLED_MEDIA_END");
        observation
    });
    if let Some(path) = std::env::var_os("PULSEBEAM_STANDALONE_NS_TRACE_PATH") {
        std::fs::write(path, serde_json::to_vec(&(&threaded, &managed)).unwrap()).unwrap();
    }
    assert_eq!(threaded, managed[0]);
    assert_eq!(managed[0], managed[1]);
}
