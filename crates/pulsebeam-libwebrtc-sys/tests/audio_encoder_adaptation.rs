use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

struct Time(Arc<AtomicI64>);
impl ClockProvider for Time {
    fn current_time(&self) -> FiniteTimestamp {
        FiniteTimestamp::from_microseconds(self.0.load(Ordering::SeqCst)).unwrap()
    }
    fn convert_timestamp_to_ntp_time(&self, _: Timestamp) -> NtpTime {
        NtpTime::from_raw(0)
    }
}
fn encoder(name: &[u8], rate: i32, channels: usize) -> (AudioEncoder, Arc<AtomicI64>) {
    let time = Arc::new(AtomicI64::new(0));
    let bounds = ClockTimeBounds::new(
        FiniteTimestamp::from_microseconds(0).unwrap(),
        FiniteTimestamp::from_microseconds(60_000_000).unwrap(),
    )
    .unwrap();
    let clock = Clock::from_monotonic_bounded_provider(Time(time.clone()), bounds);
    let env = Environment::builder().clock_handle(&clock).build().unwrap();
    let encoder = AudioEncoderFactory::builtin()
        .unwrap()
        .create(
            &env,
            &SdpAudioFormat::new(name.to_vec(), rate, channels),
            &AudioEncoderOptions {
                payload_type: 111,
                codec_pair_id: None,
            },
        )
        .unwrap()
        .unwrap();
    (encoder, time)
}
fn controller(tag: u16, body: &[u8]) -> Vec<u8> {
    assert!(body.len() < 125);
    let key = (tag << 3) | 2;
    let mut result = vec![
        0x0a,
        (body.len() + 3) as u8,
        (key as u8 & 0x7f) | 0x80,
        (key >> 7) as u8,
        body.len() as u8,
    ];
    result.extend_from_slice(body);
    result
}
fn enable(encoder: &AudioEncoder, config: &[u8]) -> bool {
    match encoder.enable_audio_network_adaptor(config) {
        Ok(true) => true,
        Err(AudioEncoderError::UnsupportedControl) => false,
        other => panic!("unexpected native ANA enable result: {other:?}"),
    }
}
fn encode(encoder: &AudioEncoder, timestamp: u32) {
    let pcm = vec![100; encoder.sample_rate_hz() as usize / 100 * encoder.channels()];
    encoder.encode(timestamp, &pcm).unwrap();
}

#[test]
fn actual_dtx_controller_changes_codec_and_preserves_teardown_state() {
    let (encoder, _) = encoder(b"opus", 48000, 2);
    // Native DTX enabling/disabling bandwidths: 10000 and 20000 bps.
    let config = controller(24, &[8, 0x90, 0x4e, 16, 0xa0, 0x9c, 1]);
    if !enable(&encoder, &config) {
        return;
    }
    encoder.on_received_uplink_bandwidth(6000, None).unwrap();
    encode(&encoder, 0); // Native periodic update supplies the actual smoothed BW.
    encoder.on_received_rtt(10).unwrap();
    assert!(encoder.dtx());
    assert!(
        encoder
            .network_adaptation_statistics()
            .dtx_action_counter
            .unwrap()
            > 0
    );
    assert_eq!(
        encoder.enable_audio_network_adaptor(&[0xff]),
        Err(AudioEncoderError::InvalidControl)
    );
    assert!(encoder.dtx()); // Rejected replacement did not touch the old codec.
    encoder.disable_audio_network_adaptor();
    assert!(encoder.dtx()); // Native teardown does not reset codec controls.
    assert!(encoder.set_dtx(false));
    assert!(!encoder.dtx());
    encode(&encoder, 480);
}

#[test]
fn rejected_bitrate_decision_publishes_neither_partial_stats_nor_codec_change() {
    let (encoder, _) = encoder(b"opus", 48000, 2);
    if !enable(&encoder, &controller(25, &[])) {
        return;
    }
    encoder
        .on_received_uplink_bandwidth(i32::MIN, None)
        .unwrap();
    let bitrate = encoder.target_bitrate_bps();
    let statistics = encoder.network_adaptation_statistics();
    // With a 20ms frame, native target - 400 would overflow signed int.
    assert_eq!(
        encoder.on_received_overhead(1),
        Err(AudioEncoderError::InvalidControl)
    );
    assert_eq!(encoder.target_bitrate_bps(), bitrate);
    assert_eq!(encoder.network_adaptation_statistics(), statistics);
    // The valid sparse update uses the actual retained overhead, not a reset.
    encoder.on_received_uplink_bandwidth(32000, None).unwrap();
    assert_eq!(encoder.target_bitrate_bps(), 31600);
}

#[test]
fn duplicate_v2_writers_preserve_inactive_prefixes_and_native_overwrites() {
    let (encoder, _) = encoder(b"opus", 48000, 2);
    let v2 = controller(27, &[]);
    if !enable(&encoder, &[v2.as_slice(), v2.as_slice()].concat()) {
        return;
    }
    // Missing BW/target makes the native early return ignore this multiplication.
    encoder.on_received_overhead(268436).unwrap();
    encoder.on_received_uplink_bandwidth(60000, None).unwrap();
    encode(&encoder, 0);
    let statistics = encoder.network_adaptation_statistics();
    assert_eq!(
        encoder.on_received_rtt(1),
        Err(AudioEncoderError::InvalidControl)
    );
    assert_eq!(encoder.network_adaptation_statistics(), statistics);
    encoder.on_received_overhead(1).unwrap();
    assert_eq!(encoder.frames_in_next_packet(), 2);
    encode(&encoder, 480);
}

#[test]
fn active_period_admission_does_not_restrict_disabled_or_ignored_native_periods() {
    let (encoder, time) = encoder(b"opus", 48000, 2);
    encoder
        .on_received_uplink_bandwidth(16000, Some(i64::MAX))
        .unwrap();
    if !enable(&encoder, &[]) {
        return;
    }
    assert_eq!(
        encoder.on_received_uplink_bandwidth(16000, Some(i64::MAX)),
        Err(AudioEncoderError::InvalidControl)
    );
    encoder
        .on_received_uplink_bandwidth(16000, Some(-1))
        .unwrap();
    encode(&encoder, 0);
    time.store(6_000_000, Ordering::SeqCst);
    encode(&encoder, 480);
    encoder.reset();
    encoder.disable_audio_network_adaptor();
    assert!(enable(&encoder, &[]));
    encoder.on_received_rtt(0).unwrap();
    encode(&encoder, 960);
}

#[test]
fn inherited_unsupported_enable_is_real_native_false() {
    let (encoder, _) = encoder(b"PCMU", 8000, 1);
    assert_eq!(encoder.enable_audio_network_adaptor(&[]), Ok(false));
    encode(&encoder, 0);
    encode(&encoder, 80);
}
