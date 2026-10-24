use pulsebeam_webrtc_sys::*;
fn env() -> Environment {
    Environment::builder().build().unwrap()
}
fn stream(rate: i32, channels: usize) -> AudioProcessingStream {
    AudioProcessingStream {
        sample_rate_hz: rate,
        channels,
    }
}

#[test]
fn config_values_preserve_sdk_defaults_and_distinct_native_equality() {
    let config = AudioProcessorConfig::default();
    assert_eq!(config.pipeline.maximum_internal_processing_rate, 32000);
    assert!(config.pipeline.multi_channel_capture);
    assert!(config.gain_controller1.analog_gain_controller.enabled);
    assert_eq!(config.gain_controller1.compression_gain_db, 9);
    assert_eq!(config.gain_controller2.adaptive_digital.max_gain_db, 50.0);
    let p = AudioProcessingModule::builtin_with_config(&env(), &config).unwrap();
    assert_eq!(p.configuration(), config);
    assert!(
        config
            .to_native_string()
            .contains("AudioProcessing::Config")
    );
    let a = config.gain_controller1;
    let mut b = a;
    b.analog_gain_controller.clipping_predictor.enabled = true;
    assert_ne!(a, b);
    // The pin's equality omits enabled, even though the value contains it.
    assert!(a.native_equals(&b));
    b.analog_gain_controller.clipping_predictor.window_length += 1;
    assert!(!a.native_equals(&b));
}

#[test]
fn full_config_drives_real_pre_and_post_gain_on_same_module() {
    let mut c = AudioProcessorConfig::default();
    c.pre_amplifier.enabled = true;
    c.pre_amplifier.fixed_gain_factor = 2.0;
    c.capture_level_adjustment.enabled = true;
    c.capture_level_adjustment.pre_gain_factor = 3.0;
    c.capture_level_adjustment.post_gain_factor = 0.5;
    let mut p = AudioProcessingModule::builtin_with_config(&env(), &c).unwrap();
    let mut pcm = vec![1000; 169];
    assert_eq!(
        p.process_interleaved(stream(16000, 1), &mut pcm).unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(&pcm[..160], &[3000; 160]);
    assert_eq!(&pcm[160..], &[1000; 9]);
    c.capture_level_adjustment.pre_gain_factor = 2.0;
    p.apply_config(&c).unwrap();
    pcm.fill(1000);
    assert_eq!(
        p.process_interleaved(stream(16000, 1), &mut pcm).unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(&pcm[..160], &[2000; 160]);
    assert_eq!(&pcm[160..], &[1000; 9]);
    assert_eq!(p.configuration(), c);
}

#[test]
fn unsafe_active_fields_fail_before_mutation_but_native_gc2_reset_is_preserved() {
    let mut p =
        AudioProcessingModule::builtin_with_config(&env(), &AudioProcessorConfig::default())
            .unwrap();
    let before = p.configuration();
    let mut invalid = before;
    invalid.gain_controller1.enabled = true;
    invalid.gain_controller1.analog_gain_controller.enabled = false;
    invalid.gain_controller1.target_level_dbfs = 32;
    assert_eq!(
        p.apply_config(&invalid),
        Err(AudioProcessorError::InvalidConfiguration)
    );
    assert_eq!(p.configuration(), before);
    assert!(matches!(
        AudioProcessingModule::builtin_with_config(&env(), &invalid),
        Err(AudioProcessorError::InvalidConfiguration)
    ));
    invalid = before;
    invalid.pre_amplifier.enabled = true;
    invalid.pre_amplifier.fixed_gain_factor = f32::MAX;
    invalid.capture_level_adjustment.enabled = true;
    invalid.capture_level_adjustment.pre_gain_factor = 2.0;
    assert_eq!(
        p.apply_config(&invalid),
        Err(AudioProcessorError::InvalidConfiguration)
    );
    assert_eq!(p.configuration(), before);
    // GC2 ApplyConfig itself validates and resets. Do not pre-reject this
    // recoverable native disposition as if it were constructor admission.
    invalid = before;
    invalid.gain_controller2.enabled = true;
    invalid.gain_controller2.fixed_digital.gain_db = 50.0;
    assert!(matches!(
        AudioProcessingModule::builtin_with_config(&env(), &invalid),
        Err(AudioProcessorError::InvalidConfiguration)
    ));
    p.apply_config(&invalid).unwrap();
    assert_eq!(
        p.configuration().gain_controller2,
        ApmGainController2::default()
    );
}

#[test]
fn predictor_integer_and_float_narrowing_are_guarded_only_when_reached() {
    let mut c = AudioProcessorConfig::default();
    c.gain_controller1.enabled = true;
    let cp = &mut c.gain_controller1.analog_gain_controller.clipping_predictor;
    cp.enabled = true;
    cp.reference_window_length = i32::MAX;
    cp.reference_window_delay = 1;
    assert!(matches!(
        AudioProcessingModule::builtin_with_config(&env(), &c),
        Err(AudioProcessorError::InvalidConfiguration)
    ));
    let cp = &mut c.gain_controller1.analog_gain_controller.clipping_predictor;
    cp.reference_window_length = 5;
    cp.reference_window_delay = 5;
    cp.mode = ApmClippingPredictorMode::AdaptiveStepClippingPeakPrediction;
    cp.clipping_threshold = f32::NAN;
    assert!(matches!(
        AudioProcessingModule::builtin_with_config(&env(), &c),
        Err(AudioProcessorError::InvalidConfiguration)
    ));
    // Event mode never narrows this float. NaN gives its actual no-prediction
    // comparisons; disabled raw fields likewise are not consuming admission.
    c.gain_controller1
        .analog_gain_controller
        .clipping_predictor
        .mode = ApmClippingPredictorMode::ClippingEventPrediction;
    assert!(AudioProcessingModule::builtin_with_config(&env(), &c).is_ok());
    c.gain_controller1.enabled = false;
    c.gain_controller1
        .analog_gain_controller
        .clipping_predictor
        .reference_window_length = i32::MAX;
    c.gain_controller1
        .analog_gain_controller
        .clipping_predictor
        .reference_window_delay = 1;
    assert!(AudioProcessingModule::builtin_with_config(&env(), &c).is_ok());
}

#[test]
fn gc2_uses_actual_fp_operations_not_an_arbitrary_db_cap() {
    let mut c = AudioProcessorConfig::default();
    c.gain_controller2.enabled = true;
    c.gain_controller2.adaptive_digital.enabled = true;
    c.gain_controller2.adaptive_digital.max_gain_db = f32::MAX;
    let mut p = AudioProcessingModule::builtin_with_config(&env(), &c).unwrap();
    p.set_stream_analog_level(100).unwrap();
    let mut pcm = vec![0; 160];
    assert_eq!(
        p.process_interleaved(stream(16000, 1), &mut pcm).unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(pcm, vec![0; 160]);
    c.gain_controller2.adaptive_digital.initial_gain_db = 200.0;
    assert_eq!(
        p.apply_config(&c),
        Err(AudioProcessorError::UnqualifiedGainRange)
    );
    assert_eq!(
        p.configuration()
            .gain_controller2
            .adaptive_digital
            .initial_gain_db,
        15.0
    );
    c.gain_controller2.adaptive_digital.initial_gain_db = 800.0;
    assert_eq!(
        p.apply_config(&c),
        Err(AudioProcessorError::InvalidConfiguration)
    );
    assert_eq!(
        p.configuration()
            .gain_controller2
            .adaptive_digital
            .initial_gain_db,
        15.0
    );
}

#[test]
fn gc2_exact_admitted_boundary_runs_native_history_and_refuses_next_float() {
    let boundary = 90.308_998_699_194_36_f32;
    let mut c = AudioProcessorConfig::default();
    c.gain_controller2.enabled = true;
    c.gain_controller2.adaptive_digital.enabled = true;
    c.gain_controller2.adaptive_digital.initial_gain_db = boundary;
    let mut p = AudioProcessingModule::builtin_with_config(&env(), &c).unwrap();
    p.set_stream_analog_level(100).unwrap();
    for _ in 0..8 {
        let mut pcm: Vec<i16> = (0..160).map(|i| if i % 2 == 0 { 1 } else { -1 }).collect();
        assert_eq!(
            p.process_interleaved(stream(16000, 1), &mut pcm).unwrap(),
            AudioProcessingStatus::OK
        );
        assert!(pcm.iter().any(|&sample| i32::from(sample).abs() > 1));
    }
    assert_eq!(
        p.configuration()
            .gain_controller2
            .adaptive_digital
            .initial_gain_db,
        boundary
    );
    c.gain_controller2.adaptive_digital.initial_gain_db = f32::from_bits(boundary.to_bits() + 1);
    assert_eq!(
        p.apply_config(&c),
        Err(AudioProcessorError::UnqualifiedGainRange)
    );
    assert_eq!(
        p.configuration()
            .gain_controller2
            .adaptive_digital
            .initial_gain_db,
        boundary
    );
}

#[test]
fn explicit_formats_drive_graph_and_refuse_unsafe_layouts_without_synthetic_status() {
    let mut unsupported = AudioProcessorConfig::default();
    unsupported.echo_canceller.enabled = true;
    unsupported.echo_canceller.export_linear_aec_output = true;
    assert!(matches!(
        AudioProcessingModule::builtin_with_config(&env(), &unsupported),
        Err(AudioProcessorError::LinearEchoNotConfigured)
    ));
    let empty = AudioProcessingFormats::default();
    assert_eq!(empty.input, stream(0, 0));
    let mut p =
        AudioProcessingModule::builtin_with_config(&env(), &AudioProcessorConfig::default())
            .unwrap();
    assert_eq!(
        p.initialize_with_formats(empty),
        Err(AudioProcessorError::InvalidDimensions)
    );
    let formats = AudioProcessingFormats {
        input: stream(44100, 2),
        output: stream(16000, 1),
        reverse_input: stream(48000, 2),
        reverse_output: stream(32000, 1),
    };
    assert_eq!(
        p.initialize_with_formats(formats).unwrap(),
        AudioProcessingStatus::OK
    );
    let state = p.format_state();
    assert_eq!(state.input_channels, 2);
    assert_eq!(state.output_channels, 1);
    let mut invalid = formats;
    invalid.reverse_output.channels = 3;
    assert_eq!(
        p.initialize_with_formats(invalid),
        Err(AudioProcessorError::InvalidDimensions)
    );
    assert_eq!(p.format_state(), state);
    let no_render = AudioProcessingFormats {
        input: stream(16000, 1),
        output: stream(16000, 1),
        ..Default::default()
    };
    assert_eq!(
        p.initialize_with_formats(no_render).unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(p.format_state().reverse_channels, 0);
    assert_eq!(
        p.configure_effects(AudioProcessingConfig {
            echo_cancellation: true,
            ..Default::default()
        }),
        Err(AudioProcessorError::InvalidDimensions)
    );
    let mut echo = AudioProcessorConfig::default();
    echo.echo_canceller.enabled = true;
    assert_eq!(
        p.apply_config(&echo),
        Err(AudioProcessorError::InvalidDimensions)
    );
}

#[test]
fn convenience_digital_gain_selects_legacy_digital_not_the_analog_manager() {
    for mode in [GainControl::AdaptiveDigital, GainControl::FixedDigital] {
        let mut trials = FieldTrials::default();
        trials
            .set(
                "WebRTC-Audio-2ndAgcMinMicLevelExperiment",
                "Enabled-99999999999999999999999",
            )
            .unwrap();
        let env = Environment::builder().field_trials(trials).build().unwrap();
        let p = AudioProcessingModule::builtin(
            &env,
            AudioProcessingConfig {
                gain_control: mode,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !p.configuration()
                .gain_controller1
                .analog_gain_controller
                .enabled
        );
    }
}
