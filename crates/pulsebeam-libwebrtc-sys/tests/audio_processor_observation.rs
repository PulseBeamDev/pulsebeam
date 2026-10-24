use pulsebeam_webrtc_sys::*;
fn stream() -> AudioProcessingStream {
    AudioProcessingStream {
        sample_rate_hz: 16000,
        channels: 1,
    }
}

#[test]
fn statistics_preserve_absence_and_real_echo_reporting() {
    let environment = Environment::builder().build().unwrap();
    let mut p =
        AudioProcessingModule::builtin(&environment, AudioProcessingConfig::default()).unwrap();
    assert_eq!(p.statistics(), AudioProcessingStatistics::default());
    assert_eq!(
        p.statistics_for_remote_tracks(false),
        AudioProcessingStatistics::default()
    );
    let mut c = AudioProcessorConfig::default();
    c.echo_canceller.enabled = true;
    p.apply_config(&c).unwrap();
    p.set_stream_delay_ms(37);
    for frame in 0..20 {
        let mut render: Vec<i16> = (0..160)
            .map(|i| (((i + frame * 160) % 67) as i16 - 33) * 100)
            .collect();
        let mut capture = render.clone();
        assert_eq!(
            p.process_reverse_interleaved(stream(), &mut render)
                .unwrap(),
            AudioProcessingStatus::OK
        );
        assert_eq!(
            p.process_interleaved(stream(), &mut capture).unwrap(),
            AudioProcessingStatus::OK
        );
    }
    let stats = p.statistics();
    assert!(stats.echo_return_loss.is_some());
    assert!(stats.echo_return_loss_enhancement.is_some());
    assert!(stats.delay_ms.is_some());
    assert!(stats.voice_detected.is_none());
    // This pin's deprecated bool overload delegates to the normal snapshot,
    // rather than implementing the filter described by the public comment.
    assert_eq!(p.statistics_for_remote_tracks(false), stats);
}

#[test]
fn linear_output_uses_native_rows_and_preserves_caller_tails() {
    let environment = Environment::builder().build().unwrap();
    let mut p = AudioProcessingModule::builtin_with_linear_echo(
        &environment,
        &AudioProcessorConfig::default(),
    )
    .unwrap();
    let mut out = [[9.0; 160]; 2];
    assert!(matches!(
        p.linear_aec_output(&mut out),
        Ok(false) | Err(AudioProcessorError::UnqualifiedLinearOutputQuery)
    ));
    assert_eq!(out, [[9.0; 160]; 2]);
    let mut c = AudioProcessorConfig::default();
    c.echo_canceller.enabled = true;
    c.echo_canceller.export_linear_aec_output = true;
    p.apply_config(&c).unwrap();
    assert_eq!(
        p.linear_aec_output(&mut []),
        Err(AudioProcessorError::BufferTooShort)
    );
    // SDK ChannelBuffer initializes its own allocation, independently of any
    // caller staging buffer. Its pre-capture zero output is a native result.
    assert_eq!(p.linear_aec_output(&mut out), Ok(true));
    assert_eq!(out[0], [0.0; 160]);
    assert_eq!(out[1], [9.0; 160]);
    for frame in 0..12 {
        let mut render = vec![0; 160];
        let mut capture: Vec<i16> = (0..160)
            .map(|i| (((i + frame * 160) % 67) as i16 - 33) * 100)
            .collect();
        p.process_reverse_interleaved(stream(), &mut render)
            .unwrap();
        p.process_interleaved(stream(), &mut capture).unwrap();
    }
    out[0].fill(9.0);
    assert_eq!(p.linear_aec_output(&mut out), Ok(true));
    assert!(out[0].iter().all(|x| x.is_finite() && x.abs() <= 1.0));
    assert!(out[0].iter().any(|x| x.abs() > 0.001));
    assert_eq!(out[1], [9.0; 160]);
    // Export-only ApplyConfig changes the stored flag, not the native storage.
    c.echo_canceller.export_linear_aec_output = false;
    p.apply_config(&c).unwrap();
    let retained = p.linear_aec_output(&mut out);
    assert!(matches!(
        retained,
        Ok(true) | Err(AudioProcessorError::UnqualifiedLinearOutputQuery)
    ));
    assert_eq!(p.initialize().unwrap(), AudioProcessingStatus::OK);
    let missing = p.linear_aec_output(&mut out);
    assert!(matches!(
        missing,
        Ok(false) | Err(AudioProcessorError::UnqualifiedLinearOutputQuery)
    ));
    c.echo_canceller.enabled = false;
    p.apply_config(&c).unwrap();
    let before = out;
    assert!(matches!(
        p.linear_aec_output(&mut out),
        Ok(false) | Err(AudioProcessorError::UnqualifiedLinearOutputQuery)
    ));
    assert_eq!(out, before);
    let mut ordinary =
        AudioProcessingModule::builtin_with_config(&environment, &AudioProcessorConfig::default())
            .unwrap();
    c.echo_canceller.enabled = true;
    c.echo_canceller.export_linear_aec_output = true;
    assert_eq!(
        ordinary.apply_config(&c),
        Err(AudioProcessorError::LinearEchoNotConfigured)
    );
    assert_eq!(ordinary.configuration(), AudioProcessorConfig::default());
    c.echo_canceller.enabled = false;
    ordinary.apply_config(&c).unwrap();
    assert_eq!(
        ordinary.configure_effects(AudioProcessingConfig {
            echo_cancellation: true,
            ..Default::default()
        }),
        Err(AudioProcessorError::LinearEchoNotConfigured)
    );
}
