use pulsebeam_webrtc_sys::*;

#[test]
fn native_option_values_preserve_all_fields_presence_and_binary_configuration() {
    let changes = AudioSourceOptions {
        echo_cancellation: Some(false),
        echo_cancellation_mode: Some(AudioProcessingMode::Automatic),
        auto_gain_control: Some(true),
        auto_gain_control_mode: Some(AudioProcessingMode::Platform),
        noise_suppression: Some(false),
        noise_suppression_mode: Some(AudioProcessingMode::Software),
        highpass_filter: Some(true),
        highpass_filter_mode: Some(AudioProcessingMode::Unknown(-73)),
        stereo_swapping: Some(false),
        // Owning values preserve the full integer domain. Applying them to
        // an engine is a separate operation with its own consumption rules.
        audio_jitter_buffer_max_packets: Some(i32::MAX),
        audio_jitter_buffer_fast_accelerate: Some(false),
        audio_jitter_buffer_min_delay_ms: Some(i32::MIN),
        audio_network_adaptor: Some(false),
        audio_network_adaptor_config: Some(vec![0, 0xff, b'x', 0, 0x80]),
        init_recording_on_send: Some(false),
    };
    let mut actual = AudioSourceOptions::default();
    assert_ne!(actual, changes);
    actual.set_all(&changes);
    // Compare the reified Rust fields too, so dropping a field on both sides
    // of native operator== cannot hide an incomplete bridge conversion.
    assert_eq!(format!("{actual:?}"), format!("{changes:?}"));
    assert_eq!(actual, changes);
    actual.set_all(&AudioSourceOptions::default());
    assert_eq!(format!("{actual:?}"), format!("{changes:?}"));
    let description = actual.native_description();
    assert!(description.contains("aec: 0"));
    assert!(description.contains("agc_mode: platform"));
    assert!(description.contains("ns_mode: software"));
    assert!(!description.contains("adaptor_config"));

    let mut empty_config = AudioSourceOptions::default();
    empty_config.audio_network_adaptor_config = Some(vec![]);
    assert_ne!(empty_config, AudioSourceOptions::default());
    let mut copied = AudioSourceOptions::default();
    copied.set_all(&empty_config);
    assert_eq!(copied.audio_network_adaptor_config, Some(vec![]));
    let mut explicit = AudioSourceOptions::default();
    explicit.audio_jitter_buffer_min_delay_ms = Some(0);
    explicit.echo_cancellation = Some(false);
    copied.set_all(&explicit);
    assert_eq!(copied.audio_jitter_buffer_min_delay_ms, Some(0));
    assert_eq!(copied.echo_cancellation, Some(false));
    assert_eq!(copied.audio_network_adaptor_config, Some(vec![]));
}

#[test]
fn source_mutation_admits_whole_values_and_preserves_native_volume_noop() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let source = factory.create_audio_source().unwrap();
    let track = factory.create_audio_track("controls", &source).unwrap();
    let alias = track.clone();
    let sink = track.attach_sink().unwrap();
    let accepted = AudioSourceOptions {
        echo_cancellation: Some(false),
        echo_cancellation_mode: Some(AudioProcessingMode::Automatic),
        auto_gain_control: Some(false),
        auto_gain_control_mode: Some(AudioProcessingMode::Platform),
        noise_suppression: Some(false),
        noise_suppression_mode: Some(AudioProcessingMode::Software),
        highpass_filter: Some(false),
        highpass_filter_mode: Some(AudioProcessingMode::Software),
        stereo_swapping: Some(false),
        audio_jitter_buffer_max_packets: Some(i32::MAX),
        audio_jitter_buffer_fast_accelerate: Some(false),
        audio_jitter_buffer_min_delay_ms: Some(i32::MIN),
        audio_network_adaptor: Some(false),
        audio_network_adaptor_config: Some(vec![0, 0xff, 0]),
        init_recording_on_send: Some(false),
    };
    source.set_options(&accepted).unwrap();
    assert_eq!(source.options(), accepted);
    assert_eq!(format!("{:?}", source.options()), format!("{accepted:?}"));
    let mutations: [fn(&mut AudioSourceOptions); 4] = [
        |o| o.echo_cancellation_mode = Some(AudioProcessingMode::Unknown(17)),
        |o| o.auto_gain_control_mode = Some(AudioProcessingMode::Unknown(-1)),
        |o| o.noise_suppression_mode = Some(AudioProcessingMode::Unknown(i32::MAX)),
        |o| o.highpass_filter_mode = Some(AudioProcessingMode::Unknown(i32::MIN)),
    ];
    for mutation in mutations {
        let mut rejected = accepted.clone();
        rejected.stereo_swapping = Some(true);
        mutation(&mut rejected);
        assert_eq!(
            source.set_options(&rejected),
            Err(AudioSourceControlError::UnknownProcessingMode)
        );
        assert_eq!(source.options(), accepted);
    }
    let mut lower = AudioSourceOptions::default();
    lower.audio_jitter_buffer_max_packets = Some(i32::MIN);
    source.set_options(&lower).unwrap();
    assert_eq!(
        source.options().audio_jitter_buffer_max_packets,
        Some(i32::MIN)
    );
    source.set_options(&AudioSourceOptions::default()).unwrap();
    assert_eq!(source.options(), AudioSourceOptions::default());
    for volume in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1, 10.1] {
        assert_eq!(
            source.set_volume(volume),
            Err(AudioSourceControlError::InvalidVolume)
        );
        assert_eq!(
            track.set_volume(volume),
            Err(AudioSourceControlError::InvalidVolume)
        );
    }
    for volume in [-0.0, 0.0, 0.5, 10.0] {
        source.set_volume(volume).unwrap();
        alias.set_volume(volume).unwrap();
        assert_eq!(source.volume(), 1.0);
        assert_eq!(alias.volume(), 1.0);
    }
    // Source SetVolume is a real native no-op, not a request to synthesize gain.
    source
        .push_frame(&AudioPcmFrame::i16_interleaved(8000, 1, 0, vec![17; 80]).unwrap())
        .unwrap();
    assert_eq!(sink.try_next_frame().unwrap().samples, vec![17; 80]);
    drop((source, track, factory));
    alias.set_volume(0.0).unwrap();
    assert_eq!(alias.volume(), 1.0);
}

#[test]
fn source_and_installed_track_queries_follow_native_state_and_detachment() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut source = factory.create_audio_source().unwrap();
    let track = factory
        .create_audio_track("options-track", &source)
        .unwrap();
    assert_eq!(source.state(), AudioSourceState::Live);
    assert!(!source.is_remote());
    assert_eq!(source.volume(), 1.0);
    assert_eq!(source.options(), AudioSourceOptions::default());
    assert_eq!(track.kind(), "audio");
    assert_eq!(track.state(), AudioTrackState::Live);
    assert_eq!(track.signal_level(), None); // Pinned native default, not zero.
    assert_eq!(track.volume(), 1.0);
    track
        .set_audio_processing_options(AudioProcessingOptions {
            echo_cancellation: ProcessingChoice::Software,
            noise_suppression: ProcessingChoice::Disabled,
            gain_control: ProcessingChoice::Automatic,
        })
        .unwrap();
    let options = source.options();
    assert_eq!(options.echo_cancellation, Some(true));
    assert_eq!(
        options.echo_cancellation_mode,
        Some(AudioProcessingMode::Software)
    );
    assert_eq!(options.noise_suppression, Some(false));
    assert_eq!(options.noise_suppression_mode, None);
    assert_eq!(options.auto_gain_control, Some(true));
    assert_eq!(
        options.auto_gain_control_mode,
        Some(AudioProcessingMode::Automatic)
    );
    assert_eq!(options.highpass_filter, None);
    let mut peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let transceiver = peer
        .add_audio_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let sender = transceiver.sender();
    let before = sender.audio_track_status().unwrap();
    assert_eq!(before, track.status());
    assert_eq!(before.id, "options-track");
    assert_eq!(before.kind, "audio");
    assert!(before.enabled);
    assert!(track.set_enabled(false));
    assert!(!sender.audio_track_status().unwrap().enabled);
    sender.set_audio_track_volume(0.0).unwrap();
    assert_eq!(sender.audio_track_status().unwrap().volume, 1.0);
    sender.set_audio_track(None).unwrap();
    assert_eq!(sender.audio_track_status(), None);
    assert_eq!(
        sender.set_audio_track_volume(1.0),
        Err(AudioSourceControlError::MissingAudioTrack)
    );
    sender.set_audio_track(Some(&track)).unwrap();
    assert_eq!(sender.audio_track_status().unwrap().id, before.id);
    source.close().unwrap();
    assert_eq!(source.state(), AudioSourceState::Ended);
    assert_eq!(track.state(), AudioTrackState::Ended);
    assert_eq!(
        sender.audio_track_status().unwrap().state,
        AudioTrackState::Ended
    );
    assert_eq!(source.options(), options);
    peer.close().unwrap();
    drop((peer, transceiver, sender, track, source, factory));
    assert_eq!(before.state, AudioTrackState::Live);
    assert!(before.enabled);
    assert_eq!(options.echo_cancellation, Some(true));
}
