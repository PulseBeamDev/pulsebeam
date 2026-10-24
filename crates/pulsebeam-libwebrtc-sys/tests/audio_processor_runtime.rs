use AudioProcessingRuntimeSetting as Setting;
use pulsebeam_webrtc_sys::*;
fn stream() -> AudioProcessingStream {
    AudioProcessingStream {
        sample_rate_hz: 16000,
        channels: 1,
    }
}
fn processor(config: &AudioProcessorConfig) -> AudioProcessingModule {
    AudioProcessingModule::builtin_with_config(&Environment::builder().build().unwrap(), config)
        .unwrap()
}
fn capture(p: &mut AudioProcessingModule) -> Vec<i16> {
    let mut data = vec![1000; 169];
    assert_eq!(
        p.process_interleaved(stream(), &mut data).unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(&data[160..], &[1000; 9]);
    data
}

#[test]
fn native_void_and_boolean_settings_affect_real_pcm_after_capture_drain() {
    let mut c = AudioProcessorConfig::default();
    c.pre_amplifier.enabled = true;
    let mut p = processor(&c);
    p.set_runtime_setting(Setting::CapturePreGain(2.0)).unwrap();
    assert_eq!(p.configuration().pre_amplifier.fixed_gain_factor, 1.0);
    let first = capture(&mut p);
    assert!(first[0] > 1000 && first[0] < 2000);
    assert_eq!(first[159], 2000);
    assert_eq!(&capture(&mut p)[..160], &[2000; 160]);
    assert_eq!(p.configuration().pre_amplifier.fixed_gain_factor, 2.0);
    assert!(
        p.post_runtime_setting(Setting::CapturePostGain(0.5))
            .unwrap()
    );
    c = p.configuration();
    c.capture_level_adjustment.enabled = true;
    c.capture_level_adjustment.pre_gain_factor = 2.0;
    p.apply_config(&c).unwrap();
    capture(&mut p);
    assert_eq!(&capture(&mut p)[..160], &[2000; 160]);
    assert_eq!(
        p.configuration().capture_level_adjustment.post_gain_factor,
        0.5
    );
}

#[test]
fn native_queue_capacity_and_drain_are_not_cached_success() {
    let mut c = AudioProcessorConfig::default();
    c.pre_amplifier.enabled = true;
    let mut p = processor(&c);
    for _ in 0..100 {
        assert!(
            p.post_runtime_setting(Setting::CapturePreGain(2.0))
                .unwrap()
        );
    }
    assert!(
        !p.post_runtime_setting(Setting::CapturePreGain(3.0))
            .unwrap()
    );
    capture(&mut p);
    assert_eq!(p.configuration().pre_amplifier.fixed_gain_factor, 2.0);
    assert!(
        p.post_runtime_setting(Setting::CapturePreGain(3.0))
            .unwrap()
    );
    capture(&mut p);
    assert_eq!(&capture(&mut p)[..160], &[3000; 160]);
}

#[test]
fn inactive_payloads_remain_verbatim_and_config_activation_checks_pending_values() {
    let mut p = processor(&AudioProcessorConfig::default());
    assert!(
        p.post_runtime_setting(Setting::CapturePreGain(f32::NAN))
            .unwrap()
    );
    let mut enabled = AudioProcessorConfig::default();
    enabled.pre_amplifier.enabled = true;
    assert_eq!(
        p.apply_config(&enabled),
        Err(AudioProcessorError::UnsafeQueuedRuntimeSetting)
    );
    assert_eq!(p.configuration(), AudioProcessorConfig::default());
    assert_eq!(p.initialize().unwrap(), AudioProcessingStatus::OK);
    let mut render = vec![0; 160];
    p.process_reverse_interleaved(stream(), &mut render)
        .unwrap();
    assert_eq!(
        p.apply_config(&enabled),
        Err(AudioProcessorError::UnsafeQueuedRuntimeSetting)
    );
    capture(&mut p); // The native handler ignores the still-inactive NaN.
    p.apply_config(&enabled).unwrap();
    assert_eq!(&capture(&mut p)[..160], &[1000; 160]);
}

#[test]
fn failed_capture_does_not_claim_a_queue_drain() {
    let mut p = processor(&AudioProcessorConfig::default());
    assert!(
        p.post_runtime_setting(Setting::CapturePreGain(f32::NAN))
            .unwrap()
    );
    let mut enabled = AudioProcessorConfig::default();
    enabled.pre_amplifier.enabled = true;
    let mut short = [];
    assert_eq!(
        p.process_interleaved(stream(), &mut short),
        Err(AudioProcessorError::BufferTooShort)
    );
    assert_eq!(
        p.apply_config(&enabled),
        Err(AudioProcessorError::UnsafeQueuedRuntimeSetting)
    );
    let mut data = vec![1000; 79];
    let unsupported = AudioProcessingStream {
        sample_rate_hz: 7999,
        channels: 1,
    };
    assert_ne!(
        p.process_interleaved(unsupported, &mut data).unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(
        p.apply_config(&enabled),
        Err(AudioProcessorError::UnqualifiedQueuedRuntimeSetting)
    );
    capture(&mut p);
    p.apply_config(&enabled).unwrap();
}

#[test]
fn void_set_has_no_enqueue_ack_even_after_native_queue_overflow() {
    let mut p = processor(&AudioProcessorConfig::default());
    for _ in 0..100 {
        assert!(
            p.post_runtime_setting(Setting::CaptureOutputUsed(false))
                .unwrap()
        );
    }
    p.set_runtime_setting(Setting::CapturePreGain(f32::NAN))
        .unwrap();
    let mut enabled = AudioProcessorConfig::default();
    enabled.pre_amplifier.enabled = true;
    // The SDK dropped this setting, but Set did not expose its acknowledgment.
    assert_eq!(
        p.apply_config(&enabled),
        Err(AudioProcessorError::UnqualifiedQueuedRuntimeSetting)
    );
    capture(&mut p);
    p.apply_config(&enabled).unwrap();
    assert_eq!(&capture(&mut p)[..160], &[1000; 160]);
}

#[test]
fn playout_volume_preserves_partial_native_enqueue_and_open_integer_values() {
    let mut p = processor(&AudioProcessorConfig::default());
    for _ in 0..100 {
        assert!(
            p.post_runtime_setting(Setting::CaptureOutputUsed(true))
                .unwrap()
        );
    }
    // Capture queue is full, but the render half succeeds. False is not rollback.
    assert!(
        !p.post_runtime_setting(Setting::PlayoutVolumeChange(i32::MIN))
            .unwrap()
    );
    capture(&mut p);
    for _ in 0..99 {
        assert!(
            p.post_runtime_setting(Setting::CustomRenderSetting(f32::NAN))
                .unwrap()
        );
    }
    assert!(
        !p.post_runtime_setting(Setting::CustomRenderSetting(1.0))
            .unwrap()
    );
    let mut render = vec![0; 160];
    p.process_reverse_interleaved(stream(), &mut render)
        .unwrap();
    assert!(
        p.post_runtime_setting(Setting::PlayoutAudioDeviceChange(AudioPlayoutDeviceInfo {
            id: -1,
            max_volume: i32::MAX
        }))
        .unwrap()
    );
    assert!(
        p.post_runtime_setting(Setting::PlayoutVolumeChange(i32::MAX))
            .unwrap()
    );
    assert_eq!(
        AudioPlayoutDeviceInfo::default(),
        AudioPlayoutDeviceInfo {
            id: 0,
            max_volume: 0
        }
    );
}

#[test]
fn current_gain_arithmetic_is_checked_before_enqueue() {
    let mut c = AudioProcessorConfig::default();
    c.pre_amplifier.enabled = true;
    c.capture_level_adjustment.enabled = true;
    c.capture_level_adjustment.pre_gain_factor = 2.0;
    let mut p = processor(&c);
    assert_eq!(
        p.post_runtime_setting(Setting::CapturePreGain(f32::MAX)),
        Err(AudioProcessorError::UnsafeRuntimeSetting)
    );
    assert_eq!(
        p.set_runtime_setting(Setting::CapturePostGain(f32::NAN)),
        Err(AudioProcessorError::UnsafeRuntimeSetting)
    );
    assert_eq!(
        p.post_runtime_setting(Setting::CompressionGainDb(91)),
        Err(AudioProcessorError::UnsafeRuntimeSetting)
    );
    assert_eq!(
        p.post_runtime_setting(Setting::CaptureFixedPostGainDb(f32::INFINITY)),
        Err(AudioProcessorError::UnsafeRuntimeSetting)
    );
    assert_eq!(p.configuration(), c);
    assert_eq!(&capture(&mut p)[..160], &[2000; 160]);
}

#[test]
fn acknowledged_later_gain_supersedes_earlier_possible_and_known_payloads() {
    let mut p = processor(&AudioProcessorConfig::default());
    p.set_runtime_setting(Setting::CapturePreGain(f32::INFINITY))
        .unwrap();
    assert!(
        p.post_runtime_setting(Setting::CapturePreGain(f32::NAN))
            .unwrap()
    );
    assert!(
        p.post_runtime_setting(Setting::CapturePreGain(2.0))
            .unwrap()
    );
    let mut enabled = AudioProcessorConfig::default();
    enabled.pre_amplifier.enabled = true;
    p.apply_config(&enabled).unwrap();
    capture(&mut p);
    assert_eq!(&capture(&mut p)[..160], &[2000; 160]);
    assert_eq!(p.configuration().pre_amplifier.fixed_gain_factor, 2.0);
}

#[test]
fn prospective_gain_product_is_checked_without_changing_raw_config() {
    let mut c = AudioProcessorConfig::default();
    c.pre_amplifier.enabled = true;
    let mut p = processor(&c);
    assert!(
        p.post_runtime_setting(Setting::CapturePreGain(1e20))
            .unwrap()
    );
    c.capture_level_adjustment.enabled = true;
    c.capture_level_adjustment.pre_gain_factor = 1e20;
    assert_eq!(
        p.apply_config(&c),
        Err(AudioProcessorError::UnsafeQueuedRuntimeSetting)
    );
    assert!(!p.configuration().capture_level_adjustment.enabled);
    // A later acknowledged finite factor establishes a safe final target.
    assert!(
        p.post_runtime_setting(Setting::CapturePreGain(1e-20))
            .unwrap()
    );
    p.apply_config(&c).unwrap();
    capture(&mut p);
    let steady = capture(&mut p);
    assert!(steady[..160].iter().all(|v| (999..=1001).contains(v)));
}

#[test]
fn compression_runtime_domain_and_inactive_fixed_gain_are_native() {
    let mut c = AudioProcessorConfig::default();
    c.gain_controller1.enabled = true;
    c.gain_controller1.mode = ApmGainController1Mode::FixedDigital;
    c.gain_controller1.analog_gain_controller.enabled = false;
    let mut p = processor(&c);
    assert!(
        p.post_runtime_setting(Setting::CompressionGainDb(90))
            .unwrap()
    );
    assert!(
        p.post_runtime_setting(Setting::CaptureFixedPostGainDb(90.0))
            .unwrap()
    );
    capture(&mut p);
    assert_eq!(p.configuration().gain_controller1.compression_gain_db, 90);
    // Runtime fixed gain is deliberately ignored without a live GC2.
    assert_eq!(
        p.configuration().gain_controller2.fixed_digital.gain_db,
        0.0
    );
}

#[test]
fn gc2_runtime_ninety_db_remains_usable_without_silent_reinitialization_repair() {
    let mut c = AudioProcessorConfig::default();
    c.gain_controller2.enabled = true;
    c.gain_controller2.adaptive_digital.enabled = false;
    let mut p = processor(&c);
    assert!(
        p.post_runtime_setting(Setting::CaptureFixedPostGainDb(90.0))
            .unwrap()
    );
    capture(&mut p);
    assert_eq!(
        p.configuration().gain_controller2.fixed_digital.gain_db,
        90.0
    );
    let mut data = vec![0; 480];
    let larger = AudioProcessingStream {
        sample_rate_hz: 48000,
        channels: 1,
    };
    match p.initialize() {
        Ok(status) => {
            assert_eq!(status, AudioProcessingStatus::OK);
            assert_eq!(
                p.process_interleaved(larger, &mut data).unwrap(),
                AudioProcessingStatus::OK
            );
        }
        Err(AudioProcessorError::UnqualifiedRuntimeReinitialization) => {
            // This branch is specific to SDK builds with compiled DCHECKs.
            capture(&mut p); // Equal-format live processing is still supported.
            data.fill(17);
            assert_eq!(
                p.process_interleaved(larger, &mut data),
                Err(AudioProcessorError::UnqualifiedRuntimeReinitialization)
            );
            assert!(data.iter().all(|v| *v == 17));
            let formats = AudioProcessingFormats {
                input: stream(),
                output: stream(),
                reverse_input: stream(),
                reverse_output: stream(),
            };
            assert_eq!(
                p.initialize_with_formats(formats),
                Err(AudioProcessorError::UnqualifiedRuntimeReinitialization)
            );
        }
        other => panic!("unexpected initialize result: {other:?}"),
    }
    assert_eq!(
        p.configuration().gain_controller2.fixed_digital.gain_db,
        90.0
    );
}
