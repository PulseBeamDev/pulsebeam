use pulsebeam_webrtc_sys::*;

fn stream(sample_rate_hz: i32, channels: usize) -> AudioProcessingStream {
    AudioProcessingStream {
        sample_rate_hz,
        channels,
    }
}
fn processor(config: AudioProcessingConfig) -> AudioProcessingModule {
    let environment = Environment::builder().build().unwrap();
    AudioProcessingModule::builtin(&environment, config).unwrap()
}

#[test]
fn native_controls_and_processing_outlive_outer_environment() {
    let mut p = processor(AudioProcessingConfig::default());
    assert_eq!(p.initialize().unwrap(), AudioProcessingStatus::OK);
    p.set_output_will_be_muted(true);
    assert!(p.output_will_be_muted());
    p.set_output_will_be_muted(false);
    assert!(!p.output_will_be_muted());
    assert_eq!(
        p.set_stream_delay_ms(-1),
        AudioProcessingStatus::BAD_STREAM_PARAMETER_WARNING
    );
    assert_eq!(p.stream_delay_ms(), 0);
    assert_eq!(
        p.set_stream_delay_ms(501),
        AudioProcessingStatus::BAD_STREAM_PARAMETER_WARNING
    );
    assert_eq!(p.stream_delay_ms(), 500);
    assert_eq!(p.set_stream_delay_ms(17), AudioProcessingStatus::OK);
    assert_eq!(p.stream_delay_ms(), 17);
    p.set_stream_analog_level(73).unwrap();
    assert_eq!(p.recommended_stream_analog_level(), 73);
    assert_eq!(
        p.set_stream_analog_level(256),
        Err(AudioProcessorError::InvalidAnalogLevel)
    );
    assert_eq!(p.recommended_stream_analog_level(), 73);
    p.set_stream_key_pressed(true);
    let mut samples = vec![1200; 487];
    assert_eq!(
        p.process_interleaved(stream(48_000, 1), &mut samples)
            .unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(samples, vec![1200; 487]);
    let format = p.format_state();
    assert_eq!(format.input_channels, 1);
    assert_eq!(format.output_channels, 1);
    assert_eq!(
        p.process_reverse_interleaved(stream(48_000, 1), &mut samples)
            .unwrap(),
        AudioProcessingStatus::OK
    );
}

#[test]
fn actual_gain_effect_changes_pcm_and_preserves_tails() {
    let format = stream(16_000, 1);
    let input: Vec<i16> = (0..160)
        .map(|i| if i % 16 < 8 { 600 } else { -600 })
        .collect();
    let mut p = processor(AudioProcessingConfig::default());
    let mut plain = input.clone();
    assert_eq!(
        p.process_interleaved(format, &mut plain).unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(plain, input);
    p.configure_effects(AudioProcessingConfig {
        gain_control: GainControl::FixedDigital,
        ..Default::default()
    })
    .unwrap();
    let mut gained = Vec::new();
    for _ in 0..12 {
        gained = input.clone();
        gained.extend([31415; 9]);
        assert_eq!(
            p.process_interleaved(format, &mut gained).unwrap(),
            AudioProcessingStatus::OK
        );
    }
    let energy = |s: &[i16]| s.iter().map(|&v| i64::from(v) * i64::from(v)).sum::<i64>();
    assert!(energy(&gained[..160]) > energy(&input));
    assert_eq!(&gained[160..], &[31415; 9]);
    p.configure_effects(AudioProcessingConfig::default())
        .unwrap();
    let mut reset = input.clone();
    assert_eq!(
        p.process_interleaved(format, &mut reset).unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(reset, input);
}

#[test]
fn native_planar_layout_conversion_and_error_output_are_real() {
    let mut p = processor(AudioProcessingConfig::default());
    let mut input = vec![0.125; 480];
    input.extend([0.375; 480]);
    let mut output = vec![0.875; 169];
    assert_eq!(
        p.process_planar(stream(48_000, 2), &input, stream(16_000, 1), &mut output)
            .unwrap(),
        AudioProcessingStatus::OK
    );
    assert!(output[..160].iter().any(|&v| v != 0.875));
    assert!(output[..160].iter().all(|v| v.is_finite()));
    assert!(output[80..160].iter().all(|&v| (v - 0.25).abs() < 0.005));
    assert_eq!(&output[160..], &[0.875; 9]);
    // Unsupported high input rate with a different output rate: native error
    // branch writes silence, but the caller's suffix remains untouched.
    let high = vec![0.5; 3841];
    output.fill(0.875);
    assert_eq!(
        p.process_planar(stream(384_100, 1), &high, stream(16_000, 1), &mut output)
            .unwrap(),
        AudioProcessingStatus::BAD_SAMPLE_RATE
    );
    assert_eq!(&output[..160], &[0.0; 160]);
    assert_eq!(&output[160..], &[0.875; 9]);
    // Channel mismatch at equal rates broadcasts the first source channel.
    let mut mismatched = vec![0.875; 480 * 3 + 9];
    assert_eq!(
        p.process_reverse_planar(
            stream(48_000, 2),
            &input,
            stream(48_000, 3),
            &mut mismatched
        )
        .unwrap(),
        AudioProcessingStatus::BAD_CHANNEL_COUNT
    );
    assert_eq!(&mismatched[..1440], &[0.125; 1440]);
    assert_eq!(&mismatched[1440..], &[0.875; 9]);
    assert_eq!(
        p.analyze_reverse_planar(stream(48_000, 2), &input).unwrap(),
        AudioProcessingStatus::OK
    );
}

#[test]
fn shape_failures_do_not_touch_output_and_invalid_native_formats_stay_errors() {
    let mut p = processor(AudioProcessingConfig::default());
    let mut short = vec![999; 79];
    assert_eq!(
        p.process_interleaved(stream(8_000, 1), &mut short),
        Err(AudioProcessorError::BufferTooShort)
    );
    assert_eq!(short, vec![999; 79]);
    let mut untouched = vec![999; 9];
    assert_eq!(
        p.process_interleaved(stream(0, 1), &mut untouched).unwrap(),
        AudioProcessingStatus::BAD_SAMPLE_RATE
    );
    assert_eq!(untouched, vec![999; 9]);
    assert_eq!(
        p.process_interleaved(stream(48_000, 0), &mut untouched)
            .unwrap(),
        AudioProcessingStatus::BAD_CHANNEL_COUNT
    );
    assert_eq!(untouched, vec![999; 9]);
    let mut output = vec![0.875; 80];
    assert_eq!(
        p.process_planar(
            stream(8_000, 1),
            &[f32::NAN; 80],
            stream(8_000, 1),
            &mut output
        ),
        Err(AudioProcessorError::InvalidFloatInput)
    );
    assert_eq!(output, vec![0.875; 80]);
    // An uninterpretable input layout makes native valid output silence.
    assert_eq!(
        p.process_planar(stream(8_000, 0), &[], stream(8_000, 1), &mut output)
            .unwrap(),
        AudioProcessingStatus::BAD_CHANNEL_COUNT
    );
    assert_eq!(output, vec![0.0; 80]);
}

#[test]
fn unsafe_trials_are_refused_without_replacing_environment_or_live_config() {
    let mut trials = FieldTrials::default();
    trials
        .set(
            "WebRTC-Audio-2ndAgcMinMicLevelExperiment",
            "Enabled-99999999999999999999999",
        )
        .unwrap();
    trials
        .set(
            "WebRTC-Aec3SuppressorTuningOverride",
            "nearend_tuning_mask_lf_enr_transparent:2,nearend_tuning_mask_lf_enr_suppress:1",
        )
        .unwrap();
    let env = Environment::builder().field_trials(trials).build().unwrap();
    let mut p = AudioProcessingModule::builtin(&env, AudioProcessingConfig::default()).unwrap();
    let analog = AudioProcessingConfig {
        gain_control: GainControl::AdaptiveAnalog,
        ..Default::default()
    };
    assert_eq!(
        p.configure_effects(analog),
        Err(AudioProcessorError::UnsafeFieldTrial)
    );
    assert!(matches!(
        AudioProcessingModule::builtin(&env, analog),
        Err(AudioProcessorError::UnsafeFieldTrial)
    ));
    let echo = AudioProcessingConfig {
        echo_cancellation: true,
        ..Default::default()
    };
    assert_eq!(
        p.configure_effects(echo),
        Err(AudioProcessorError::UnsafeFieldTrial)
    );
    assert!(matches!(
        AudioProcessingModule::builtin(&env, echo),
        Err(AudioProcessorError::UnsafeFieldTrial)
    ));
    let mut pcm = vec![900; 80];
    assert_eq!(
        p.process_interleaved(stream(8000, 1), &mut pcm).unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(pcm, vec![900; 80]);
    // Valid text outside the AGC experiment's 0..255 range is ignored by the
    // real SDK. It is not an unsafe scanf conversion and remains admissible.
    let mut safe = FieldTrials::default();
    safe.set(
        "WebRTC-Audio-2ndAgcMinMicLevelExperiment",
        "Enabled-256suffix",
    )
    .unwrap();
    safe.set(
        "WebRTC-Aec3SuppressorTuningOverride",
        "nearend_tuning_mask_lf_enr_transparent:0.1,nearend_tuning_mask_lf_enr_suppress:0.3",
    )
    .unwrap();
    let env = Environment::builder().field_trials(safe).build().unwrap();
    assert!(AudioProcessingModule::builtin(&env, analog).is_ok());
    assert!(AudioProcessingModule::builtin(&env, echo).is_ok());
}

#[test]
fn aec_numeric_parser_representations_are_guarded_before_native_scan() {
    for (name, group) in [
        (
            "WebRTC-Aec3SuppressorTuningOverride",
            "dominant_nearend_detection_hold_duration:99999999999999999999999",
        ),
        (
            "WebRTC-Aec3SuppressorTuningOverride",
            "nearend_tuning_mask_lf_enr_suppress:1e100",
        ),
        (
            "WebRTC-Aec3SuppressorNearendLfMaskSuppressOverride",
            "1e99999",
        ),
        ("WebRTC-Aec3UseNearendReverbLen", "default_len:1e100"),
        ("WebRTC-Aec3UseNearendReverbLen", "nearend_len:1e100"),
    ] {
        let mut trials = FieldTrials::default();
        trials.set(name, group).unwrap();
        let env = Environment::builder().field_trials(trials).build().unwrap();
        assert!(matches!(
            AudioProcessingModule::builtin(
                &env,
                AudioProcessingConfig {
                    echo_cancellation: true,
                    ..Default::default()
                }
            ),
            Err(AudioProcessorError::UnsafeFieldTrial)
        ));
    }
}

#[test]
fn native_statistics_defaults_preserve_absent_observations() {
    let value = AudioProcessorStatistics::default();
    assert!(!value.typing_noise_detected);
    let apm = &value.apm_statistics;
    assert_eq!(apm.voice_detected, None);
    assert_eq!(apm.echo_return_loss, None);
    assert_eq!(apm.echo_return_loss_enhancement, None);
    assert_eq!(apm.divergent_filter_fraction, None);
    assert_eq!(apm.delay_median_ms, None);
    assert_eq!(apm.delay_standard_deviation_ms, None);
    assert_eq!(apm.residual_echo_likelihood, None);
    assert_eq!(apm.residual_echo_likelihood_recent_max, None);
    assert_eq!(apm.delay_ms, None);
    assert_eq!(apm, &AudioProcessingStats::default());
    assert_eq!(value.clone(), value);
}

#[test]
fn stock_track_native_null_is_not_replaced_by_voice_engine_statistics() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut source = factory.create_audio_source().unwrap();
    let track = factory.create_audio_track("processor", &source).unwrap();
    assert!(track.processor().is_none());
    let mut options = AudioSourceOptions::default();
    options.echo_cancellation = Some(true);
    source.set_options(&options).unwrap();
    assert!(track.processor().is_none());
    source.close().unwrap();
    drop((factory, source));
    assert_eq!(track.state(), AudioTrackState::Ended);
    assert!(track.processor().is_none());
}
