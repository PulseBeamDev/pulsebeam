use pulsebeam_webrtc_sys::*;

#[test]
fn native_option_values_preserve_all_fields_presence_and_binary_configuration() {
    let changes = AudioSourceOptions {
        echo_cancellation: Some(false),
        auto_gain_control: Some(true),
        noise_suppression: Some(false),
        highpass_filter: Some(true),
        stereo_swapping: Some(false),
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
    assert_eq!(format!("{actual:?}"), format!("{changes:?}"));
    assert_eq!(actual, changes);
    actual.set_all(&AudioSourceOptions::default());
    assert_eq!(format!("{actual:?}"), format!("{changes:?}"));
    let description = actual.native_description();
    assert!(description.contains("aec: 0"));
    assert!(description.contains("agc: 1"));
    assert!(description.contains("ns: 0"));
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
fn source_volume_uses_upstream_void_setter_without_fabricated_readback() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let source = factory.create_audio_source().unwrap();
    let track = factory.create_audio_track("controls", &source).unwrap();
    let sink = track.attach_sink().unwrap();
    for volume in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1, 10.1] {
        assert_eq!(
            source.set_volume(volume),
            Err(AudioSourceControlError::InvalidVolume)
        );
    }
    source.set_volume(0.0).unwrap();
    source
        .push_frame(&AudioPcmFrame::i16_interleaved(8000, 1, 0, vec![17; 80]).unwrap())
        .unwrap();
    assert_eq!(sink.try_next_frame().unwrap().samples, vec![17; 80]);
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
    assert_eq!(source.options(), AudioSourceOptions::default());
    assert_eq!(track.kind(), "audio");
    assert_eq!(track.state(), AudioTrackState::Live);
    assert_eq!(track.signal_level(), None);
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
    peer.close().unwrap();
    drop((peer, transceiver, sender, track, source, factory));
    assert_eq!(before.state, AudioTrackState::Live);
    assert!(before.enabled);
}
