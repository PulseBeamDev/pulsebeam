//! Native render/capture, linear AEC output and complete metric replay.
use pulsebeam_webrtc_sys::*;
use serde::Serialize;
use std::{fmt::Debug, time::Duration};

fn serialize_debug<T: Debug, S: serde::Serializer>(
    value: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format!("{value:?}"))
}

// Preserve presence and every IEEE bit, including NaN payloads and signed zero.
// Exhaustive destructuring makes adding a native statistic a compile-time duty.
#[derive(Debug, Eq, PartialEq, Serialize)]
struct StatisticsBits {
    #[serde(serialize_with = "serialize_debug")]
    failure: Option<AudioProcessorError>,
    voice_detected: Option<bool>,
    echo_return_loss: Option<u64>,
    echo_return_loss_enhancement: Option<u64>,
    divergent_filter_fraction: Option<u64>,
    delay_median_ms: Option<i32>,
    delay_standard_deviation_ms: Option<i32>,
    residual_echo_likelihood: Option<u64>,
    residual_echo_likelihood_recent_max: Option<u64>,
    delay_ms: Option<i32>,
}
impl From<AudioProcessingStatistics> for StatisticsBits {
    fn from(value: AudioProcessingStatistics) -> Self {
        let AudioProcessingStatistics {
            failure,
            voice_detected,
            echo_return_loss,
            echo_return_loss_enhancement,
            divergent_filter_fraction,
            delay_median_ms,
            delay_standard_deviation_ms,
            residual_echo_likelihood,
            residual_echo_likelihood_recent_max,
            delay_ms,
        } = value;
        Self {
            failure,
            voice_detected,
            echo_return_loss: echo_return_loss.map(f64::to_bits),
            echo_return_loss_enhancement: echo_return_loss_enhancement.map(f64::to_bits),
            divergent_filter_fraction: divergent_filter_fraction.map(f64::to_bits),
            delay_median_ms,
            delay_standard_deviation_ms,
            residual_echo_likelihood: residual_echo_likelihood.map(f64::to_bits),
            residual_echo_likelihood_recent_max: residual_echo_likelihood_recent_max
                .map(f64::to_bits),
            delay_ms,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct LinearOutput {
    #[serde(serialize_with = "serialize_debug")]
    result: Result<bool, AudioProcessorError>,
    input_bits: Vec<Vec<u32>>,
    output_bits: Vec<Vec<u32>>,
}
fn linear_output(processor: &mut AudioProcessingModule) -> LinearOutput {
    let mut rows = [[9.0_f32; 160]; 2];
    let input_bits = rows
        .iter()
        .map(|row| row.iter().map(|v| v.to_bits()).collect())
        .collect();
    let result = processor.linear_aec_output(&mut rows);
    assert_eq!(
        rows[1], [9.0; 160],
        "mono native output must preserve the extra caller row"
    );
    let output_bits = rows
        .iter()
        .map(|row| row.iter().map(|v| v.to_bits()).collect())
        .collect();
    LinearOutput {
        result,
        input_bits,
        output_bits,
    }
}

#[derive(Debug, PartialEq, Serialize)]
struct Frame {
    render_input: Vec<i16>,
    render_output: Vec<i16>,
    render_status: i32,
    capture_input: Vec<i16>,
    capture_output: Vec<i16>,
    capture_status: i32,
    linear: LinearOutput,
    #[serde(serialize_with = "serialize_debug")]
    format: AudioProcessingFormatState,
}

#[derive(Debug, PartialEq, Serialize)]
struct Observation {
    #[serde(serialize_with = "serialize_debug")]
    initial_configuration: AudioProcessorConfig,
    #[serde(serialize_with = "serialize_debug")]
    requested_configuration: AudioProcessorConfig,
    #[serde(serialize_with = "serialize_debug")]
    actual_configuration: AudioProcessorConfig,
    #[serde(serialize_with = "serialize_debug")]
    apply_result: Result<(), AudioProcessorError>,
    delay_status: i32,
    stream_delay_ms: i32,
    initial_statistics: StatisticsBits,
    initial_linear: LinearOutput,
    pre_capture_linear: LinearOutput,
    frames: Vec<Frame>,
    statistics: StatisticsBits,
    statistics_remote_false: StatisticsBits,
}

fn application<R: SessionRuntime>(runtime: R) -> Observation {
    let factory = runtime.environment_factory().unwrap();
    let environment = runtime.create_environment(&factory).unwrap();
    let mut processor = AudioProcessingModule::builtin_with_linear_echo(
        &environment,
        &AudioProcessorConfig::default(),
    )
    .unwrap();
    drop(factory);
    drop(environment);
    // The native module retains this exact runtime context after the original
    // configuring owners are released. No peer, ADM, worker or pump is needed.
    let initial_configuration = processor.configuration();
    let initial_statistics = StatisticsBits::from(processor.statistics());
    assert_eq!(
        initial_statistics,
        StatisticsBits::from(AudioProcessingStatistics::default())
    );
    let initial_linear = linear_output(&mut processor);
    assert!(matches!(
        initial_linear.result,
        Ok(false) | Err(AudioProcessorError::UnqualifiedLinearOutputQuery)
    ));
    assert_eq!(initial_linear.input_bits, initial_linear.output_bits);
    let mut requested_configuration = AudioProcessorConfig::default();
    requested_configuration.echo_canceller.enabled = true;
    requested_configuration
        .echo_canceller
        .export_linear_aec_output = true;
    let apply_result = processor.apply_config(&requested_configuration);
    assert_eq!(apply_result, Ok(()));
    let actual_configuration = processor.configuration();
    assert_eq!(actual_configuration, requested_configuration);
    let delay_status = processor.set_stream_delay_ms(37);
    assert_eq!(delay_status, AudioProcessingStatus::OK);
    let stream_delay_ms = processor.stream_delay_ms();
    assert_eq!(stream_delay_ms, 37);
    let pre_capture_linear = linear_output(&mut processor);
    assert_eq!(pre_capture_linear.result, Ok(true));
    assert_eq!(
        pre_capture_linear.output_bits[0],
        vec![0.0_f32.to_bits(); 160]
    );
    let stream = AudioProcessingStream {
        sample_rate_hz: 16000,
        channels: 1,
    };
    let mut frames = Vec::new();
    for frame in 0..20 {
        let mut render_input = (0..160)
            .map(|i| (((i + frame * 160) % 67) as i16 - 33) * 100)
            .collect::<Vec<_>>();
        let mut capture_input = render_input.clone();
        render_input.extend_from_slice(&[321; 9]);
        capture_input.extend_from_slice(&[-654; 9]);
        let mut render_output = render_input.clone();
        let mut capture_output = capture_input.clone();
        let render_status = processor
            .process_reverse_interleaved(stream, &mut render_output)
            .unwrap();
        let capture_status = processor
            .process_interleaved(stream, &mut capture_output)
            .unwrap();
        assert_eq!(render_status, AudioProcessingStatus::OK);
        assert_eq!(capture_status, AudioProcessingStatus::OK);
        assert_eq!(&render_output[160..], &render_input[160..]);
        assert_eq!(&capture_output[160..], &capture_input[160..]);
        let linear = linear_output(&mut processor);
        assert_eq!(linear.result, Ok(true));
        assert!(linear.output_bits[0].iter().all(|&bits| {
            let value = f32::from_bits(bits);
            value.is_finite() && value.abs() <= 1.0
        }));
        frames.push(Frame {
            render_input,
            render_output,
            render_status: render_status.0,
            capture_input,
            capture_output,
            capture_status: capture_status.0,
            linear,
            format: processor.format_state(),
        });
    }
    assert!(
        frames[11..].iter().any(|frame| frame.linear.output_bits[0]
            .iter()
            .any(|&bits| f32::from_bits(bits).abs() > 0.001)),
        "actual warmed-up native linear output, not only config readback"
    );
    assert!(
        frames
            .iter()
            .any(|frame| frame.capture_input[..160] != frame.capture_output[..160])
    );
    let statistics = StatisticsBits::from(processor.statistics());
    assert_eq!(statistics.failure, None);
    assert!(statistics.echo_return_loss.is_some());
    assert!(statistics.echo_return_loss_enhancement.is_some());
    assert!(statistics.delay_ms.is_some());
    assert_eq!(statistics.voice_detected, None);
    let statistics_remote_false =
        StatisticsBits::from(processor.statistics_for_remote_tracks(false));
    assert_eq!(statistics_remote_false, statistics);
    drop(processor);
    Observation {
        initial_configuration,
        requested_configuration,
        actual_configuration,
        apply_result,
        delay_status: delay_status.0,
        stream_delay_ms,
        initial_statistics,
        initial_linear,
        pre_capture_linear,
        frames,
        statistics,
        statistics_remote_false,
    }
}

#[test]
fn native_standalone_aec_render_capture_linear_output_and_metrics_replay() {
    let threaded = application(ThreadedRuntime);
    let managed = std::array::from_fn::<_, 2, _>(|_| {
        eprintln!("CONTROLLED_MEDIA_BEGIN");
        let world = ControlledWorld::acquire(1006, Duration::from_secs(7)).unwrap();
        let observation = application(world.runtime().unwrap());
        assert_eq!(world.now(), Duration::from_secs(7));
        drop(world);
        eprintln!("CONTROLLED_MEDIA_END");
        observation
    });
    if let Some(path) = std::env::var_os("PULSEBEAM_STANDALONE_AEC_TRACE_PATH") {
        std::fs::write(path, serde_json::to_vec(&(&threaded, &managed)).unwrap()).unwrap();
    }
    assert_eq!(managed[0], managed[1]);
    assert_eq!(threaded, managed[0]);
}
