use pulsebeam_webrtc_sys::{NetEqConfig, NetEqMode, NetEqOperation, NetEqReturnCode};

#[test]
fn config_defaults_and_description_are_native_not_synthesized() {
    let config = NetEqConfig::default();
    assert_eq!(
        config,
        NetEqConfig {
            sample_rate_hz: 48_000,
            max_packets_in_buffer: 200,
            max_delay_ms: 0,
            min_delay_ms: 0,
            enable_fast_accelerate: false,
            enable_muted_state: false,
            enable_rtx_handling: false,
            for_test_no_time_stretching: false,
        }
    );
    assert_eq!(
        config.native_description(),
        "sample_rate_hz=48000, max_packets_in_buffer=200, min_delay_ms=0, enable_fast_accelerate=false, enable_muted_state=false, enable_rtx_handling=false"
    );
    let mut changed = config.clone();
    changed.max_delay_ms = i32::MIN;
    changed.for_test_no_time_stretching = true;
    assert_eq!(changed.native_description(), config.native_description());
    changed.sample_rate_hz = i32::MIN;
    changed.max_packets_in_buffer = usize::MAX;
    changed.min_delay_ms = i32::MAX;
    changed.enable_fast_accelerate = true;
    changed.enable_muted_state = true;
    changed.enable_rtx_handling = true;
    assert_eq!(
        changed.native_description(),
        format!(
            "sample_rate_hz={}, max_packets_in_buffer={}, min_delay_ms={}, enable_fast_accelerate=true, enable_muted_state=true, enable_rtx_handling=true",
            i32::MIN,
            usize::MAX,
            i32::MAX
        )
    );
}

#[test]
fn native_tags_preserve_negative_results_and_unknown_values() {
    assert_eq!(NetEqReturnCode::OK.0, 0);
    assert_eq!(NetEqReturnCode::FAIL.0, -1);
    assert_eq!(NetEqOperation::UNDEFINED.0, 10);
    assert_eq!(NetEqMode::UNDEFINED.0, 14);
    assert_eq!(NetEqOperation(i32::MIN).0, i32::MIN);
    assert_eq!(NetEqMode(i32::MAX).0, i32::MAX);
    assert_eq!(NetEqReturnCode(-42).0, -42);
}
