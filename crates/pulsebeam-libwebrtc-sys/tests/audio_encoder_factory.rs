use pulsebeam_webrtc_sys::{
    AudioEncoderCodecType, AudioEncoderError, AudioEncoderFactory, AudioEncoderOptions,
    AudioEncoderOutput, AudioNetworkAdaptationStatistics, CodecError, Environment, SdpAudioFormat,
};

#[test]
fn selected_factories_expose_native_prioritized_metadata() {
    let builtin = AudioEncoderFactory::builtin().unwrap().supported_encoders();
    assert!(!builtin.is_empty());
    assert!(
        builtin
            .iter()
            .any(|spec| spec.format.name.eq_ignore_ascii_case(b"opus"))
    );
    let carrier = AudioEncoderFactory::with_opus_frames()
        .unwrap()
        .supported_encoders();
    assert_eq!(carrier.len(), 1);
    let spec = &carrier[0];
    assert_eq!(spec.format.name, b"opus");
    assert_eq!(spec.format.clockrate_hz, 48000);
    assert_eq!(spec.format.num_channels, 2);
    // The private stereo carrier is independent of the encoded packet layout.
    assert_eq!(spec.info.num_channels, 2);
    assert_eq!(
        spec.format.parameters.get(b"stereo".as_slice()).unwrap(),
        b"0"
    );
    assert!(!spec.to_native_description().is_empty());
}

#[test]
fn selected_encoder_query_and_creation_preserve_native_dispatch() {
    let factory = AudioEncoderFactory::builtin().unwrap();
    for spec in factory.supported_encoders() {
        assert_eq!(
            factory.query_encoder(&spec.format).unwrap(),
            Some(spec.info)
        );
    }
    let unknown = SdpAudioFormat::new(b"unknown".to_vec(), i32::MIN, usize::MAX);
    assert!(factory.query_encoder(&unknown).unwrap().is_none());
    let opus = SdpAudioFormat::new(b"OpUs".to_vec(), 48000, 2);
    let environment = Environment::builder().build().unwrap();
    let invalid_payload = AudioEncoderOptions {
        payload_type: 128,
        codec_pair_id: None,
    };
    assert!(matches!(
        factory.create(&environment, &opus, &invalid_payload),
        Err(CodecError::InvalidConfiguration)
    ));
    // A payload range restriction must not turn an unsupported format into an error.
    assert!(
        factory
            .create(&environment, &unknown, &invalid_payload)
            .unwrap()
            .is_none()
    );
    let carrier = AudioEncoderFactory::with_opus_frames().unwrap();
    let canonical = SdpAudioFormat::new(b"opus".to_vec(), 48000, 2);
    assert!(
        carrier
            .create(&environment, &canonical, &invalid_payload)
            .unwrap()
            .is_some()
    );
    let carrier_info = carrier.query_encoder(&canonical).unwrap().unwrap();
    // Dispatch follows native Opus matching, including case-insensitive names.
    assert_eq!(carrier.query_encoder(&opus).unwrap(), Some(carrier_info));

    let mut multi = SdpAudioFormat::new(b"multiopus".to_vec(), 48000, 2);
    multi
        .parameters
        .insert(b"num_streams".to_vec(), b"0".to_vec());
    multi
        .parameters
        .insert(b"coupled_streams".to_vec(), b"0".to_vec());
    multi
        .parameters
        .insert(b"channel_mapping".to_vec(), b"255,255".to_vec());
    assert!(factory.query_encoder(&multi).unwrap().is_some());
    let options = AudioEncoderOptions {
        payload_type: 111,
        codec_pair_id: None,
    };
    assert!(matches!(
        factory.create(&environment, &multi, &options),
        Err(CodecError::InvalidConfiguration)
    ));
    multi
        .parameters
        .insert(b"num_streams".to_vec(), b"255".to_vec());
    assert!(factory.query_encoder(&multi).unwrap().is_none());
    multi
        .parameters
        .insert(b"num_streams".to_vec(), i32::MAX.to_string().into_bytes());
    multi.parameters.insert(
        b"coupled_streams".to_vec(),
        i32::MAX.to_string().into_bytes(),
    );
    assert!(matches!(
        factory.query_encoder(&multi),
        Err(CodecError::InvalidConfiguration)
    ));
    multi.num_channels = usize::MAX;
    assert!(matches!(
        factory.query_encoder(&multi),
        Err(CodecError::InvalidConfiguration)
    ));
}

#[test]
fn stock_fallback_channel_narrowing_preserves_unsupported_results() {
    let factory = AudioEncoderFactory::builtin().unwrap();
    let environment = Environment::builder().build().unwrap();
    let options = AudioEncoderOptions::default();
    for name in [b"L16".as_slice(), b"unknown"] {
        for channels in [i32::MAX as usize + 1, usize::MAX] {
            let format = SdpAudioFormat::new(name.to_vec(), 48000, channels);
            assert!(factory.query_encoder(&format).unwrap().is_none());
            assert!(
                factory
                    .create(&environment, &format, &options)
                    .unwrap()
                    .is_none()
            );
        }
    }
    let valid = SdpAudioFormat::new(b"L16".to_vec(), 48000, 2);
    assert!(factory.query_encoder(&valid).unwrap().is_some());
    assert!(
        factory
            .create(&environment, &valid, &options)
            .unwrap()
            .is_some()
    );
}

#[test]
fn owning_pcm_encoder_preserves_packet_state_metadata_and_environment() {
    let factory = AudioEncoderFactory::builtin().unwrap();
    let mut environment = Environment::builder().build().unwrap();
    let format = SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1);
    // G711 has no Opus payload assertion; standalone metadata retains the full int.
    let options = AudioEncoderOptions {
        payload_type: 200,
        codec_pair_id: None,
    };
    let encoder = factory
        .create(&environment, &format, &options)
        .unwrap()
        .unwrap();
    let clone = encoder.clone();
    drop(factory);
    environment.close();
    drop(environment);
    assert_eq!(encoder.sample_rate_hz(), 8000);
    assert_eq!(encoder.channels(), 1);
    assert_eq!(encoder.frames_in_next_packet(), 2);
    assert!(
        encoder
            .network_adaptation_statistics()
            .bitrate_action_counter
            .is_none()
    );
    let first = encoder.encode(100, &[0; 80]).unwrap();
    assert!(first.bytes.is_empty());
    assert_eq!(
        clone.encode(180, &[0; 79]),
        Err(AudioEncoderError::InvalidSamples)
    );
    let packet = clone.encode(180, &[0; 80]).unwrap();
    assert_eq!(packet.bytes.len(), 160);
    assert_eq!(packet.info.primary.encoded_bytes, 160);
    assert_eq!(packet.info.primary.encoded_timestamp, 100);
    assert_eq!(packet.info.primary.payload_type, 200);
    assert_eq!(
        packet.info.primary.encoder_type,
        AudioEncoderCodecType::PCMU
    );
    assert!(packet.info.redundant.is_empty());
    assert!(encoder.encode(260, &[0; 80]).unwrap().bytes.is_empty());
    clone.reset();
    assert!(encoder.encode(340, &[0; 80]).unwrap().bytes.is_empty());
    assert_eq!(
        clone
            .encode(420, &[0; 80])
            .unwrap()
            .info
            .primary
            .encoded_timestamp,
        340
    );
}

#[test]
fn opus_bitrate_trial_and_unsigned_overhead_guards_are_operation_specific() {
    let trials = pulsebeam_webrtc_sys::FieldTrials::parse(
        "WebRTC-Audio-OpusBitrateMultipliers/Enabled-1-1e35/",
    )
    .unwrap();
    let environment = Environment::builder().field_trials(trials).build().unwrap();
    let factory = AudioEncoderFactory::builtin().unwrap();
    let format = SdpAudioFormat::new(b"opus".to_vec(), 48000, 2);
    let options = AudioEncoderOptions {
        payload_type: 111,
        codec_pair_id: None,
    };
    let encoder = factory
        .create(&environment, &format, &options)
        .unwrap()
        .unwrap();
    let original = encoder.target_bitrate_bps();
    assert_eq!(
        encoder.set_target_bitrate(6000),
        Err(AudioEncoderError::InvalidControl)
    );
    assert_eq!(
        encoder.on_received_target_audio_bitrate(6000),
        Err(AudioEncoderError::InvalidControl)
    );
    assert_eq!(encoder.target_bitrate_bps(), original);
    // With no overhead and ANA disabled, native uplink bandwidth does not set bitrate.
    encoder
        .on_received_uplink_bandwidth(6000, Some(i64::MIN))
        .unwrap();
    assert_eq!(encoder.target_bitrate_bps(), original);
    encoder.on_received_overhead(0).unwrap();
    assert_eq!(
        encoder.on_received_uplink_bandwidth(6000, None),
        Err(AudioEncoderError::InvalidControl)
    );
    encoder.on_received_overhead(usize::MAX).unwrap();
    assert_eq!(
        encoder.on_received_uplink_bandwidth(i32::MAX, None),
        Err(AudioEncoderError::InvalidControl)
    );
    assert_eq!(encoder.target_bitrate_bps(), original);
}

#[test]
fn native_provider_copies_memoize_the_first_field_trial_group() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[derive(Clone)]
    struct ChangingTrials(Arc<AtomicUsize>);
    impl pulsebeam_webrtc_sys::FieldTrialsView for ChangingTrials {
        fn lookup(&self, key: &str) -> String {
            if key != "WebRTC-Audio-OpusBitrateMultipliers" {
                return String::new();
            }
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                "Enabled-1-1".into()
            } else {
                "Enabled-1-1e35".into()
            }
        }
        fn create_copy(&self) -> Arc<dyn pulsebeam_webrtc_sys::FieldTrialsView> {
            Arc::new(self.clone())
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let environment = Environment::builder()
        .field_trials(ChangingTrials(calls.clone()))
        .build()
        .unwrap();
    let factory = AudioEncoderFactory::builtin().unwrap();
    let format = SdpAudioFormat::new(b"opus".to_vec(), 48000, 2);
    let options = AudioEncoderOptions {
        payload_type: 111,
        codec_pair_id: None,
    };
    let encoder = factory
        .create(&environment, &format, &options)
        .unwrap()
        .unwrap();
    let copy = environment.field_trials().clone();
    assert_eq!(
        copy.lookup("WebRTC-Audio-OpusBitrateMultipliers"),
        "Enabled-1-1"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    encoder.set_target_bitrate(6000).unwrap();
    assert_eq!(encoder.target_bitrate_bps(), 6000);
}

#[test]
fn opus_loss_requires_a_durable_native_clock_contract() {
    use pulsebeam_webrtc_sys::{
        Clock, ClockProvider, ClockTimeBounds, FiniteTimestamp, ManualClock, NtpTime, Timestamp,
    };
    let factory = AudioEncoderFactory::builtin().unwrap();
    let format = SdpAudioFormat::new(b"opus".to_vec(), 48000, 2);
    let options = AudioEncoderOptions {
        payload_type: 111,
        codec_pair_id: None,
    };
    let default = Environment::builder().build().unwrap();
    let encoder = factory
        .create(&default, &format, &options)
        .unwrap()
        .unwrap();
    assert_eq!(
        encoder.on_received_uplink_packet_loss_fraction(0.1),
        Err(AudioEncoderError::UnsafeClock)
    );
    assert_eq!(
        encoder.on_received_uplink_packet_loss_fraction(f32::NAN),
        Err(AudioEncoderError::InvalidControl)
    );
    let clock = ManualClock::new(std::time::Duration::ZERO).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let encoder = factory
        .create(&environment, &format, &options)
        .unwrap()
        .unwrap();
    encoder
        .on_received_uplink_packet_loss_fraction(0.1)
        .unwrap();
    clock.advance(std::time::Duration::from_millis(10)).unwrap();
    encoder
        .on_received_uplink_packet_loss_fraction(1.0)
        .unwrap();
    assert_eq!(
        encoder.on_received_uplink_packet_loss_fraction(-0.1),
        Err(AudioEncoderError::InvalidControl)
    );
    assert_eq!(
        encoder.on_received_uplink_recoverable_packet_loss_fraction(0.1),
        Err(AudioEncoderError::UnsupportedControl)
    );
    struct Constant;
    impl ClockProvider for Constant {
        fn current_time(&self) -> FiniteTimestamp {
            FiniteTimestamp::from_microseconds(0).unwrap()
        }
        fn convert_timestamp_to_ntp_time(&self, _: Timestamp) -> NtpTime {
            NtpTime::from_raw(0)
        }
    }
    let unbounded = Clock::from_monotonic_bounded_provider(Constant, ClockTimeBounds::all_finite());
    let environment = Environment::builder()
        .clock_handle(&unbounded)
        .build()
        .unwrap();
    let encoder = factory
        .create(&environment, &format, &options)
        .unwrap()
        .unwrap();
    // A current sample cannot substitute for the clock's complete retaining range.
    assert_eq!(
        encoder.on_received_uplink_packet_loss_fraction(0.1),
        Err(AudioEncoderError::UnsafeClock)
    );
}

#[test]
fn allocation_retains_unused_fields_and_checks_consumed_units_only() {
    use pulsebeam_webrtc_sys::{BitrateAllocationUpdate, DataRate, TimeDelta};
    let environment = Environment::builder().build().unwrap();
    let factory = AudioEncoderFactory::builtin().unwrap();
    let format = SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1);
    let options = AudioEncoderOptions {
        payload_type: 111,
        codec_pair_id: None,
    };
    let encoder = factory
        .create(&environment, &format, &options)
        .unwrap()
        .unwrap();
    encoder.disable_audio_network_adaptor();
    encoder.disable_audio_network_adaptor();
    let mut allocation = BitrateAllocationUpdate::default();
    assert_eq!(
        encoder.on_received_uplink_allocation(allocation),
        Err(AudioEncoderError::InvalidControl)
    );
    allocation.bwe_period = TimeDelta::zero();
    allocation.round_trip_time = TimeDelta::minus_infinity();
    allocation.packet_loss_ratio = f64::NAN;
    allocation.cwnd_reduce_ratio = f64::INFINITY;
    encoder.on_received_uplink_allocation(allocation).unwrap();
    allocation.target_bitrate = DataRate::BitsPerSecond(-1);
    assert_eq!(
        encoder.on_received_uplink_allocation(allocation),
        Err(AudioEncoderError::InvalidControl)
    );
    allocation.target_bitrate = DataRate::PositiveInfinity;
    assert_eq!(
        encoder.on_received_uplink_allocation(allocation),
        Err(AudioEncoderError::InvalidControl)
    );
}

#[test]
fn encoder_values_are_public_owned_and_preserve_absence() {
    let options = AudioEncoderOptions::default();
    assert_eq!(options.payload_type, -1);
    assert!(options.codec_pair_id.is_none());
    let output = AudioEncoderOutput::default();
    assert!(output.bytes.is_empty());
    assert!(output.info.redundant.is_empty());
    assert!(output.info.primary.speech);
    let statistics = AudioNetworkAdaptationStatistics::default();
    assert!(statistics.bitrate_action_counter.is_none());
    assert!(statistics.uplink_packet_loss_fraction.is_none());
}
