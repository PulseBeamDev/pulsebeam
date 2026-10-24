use pulsebeam_webrtc_sys::{
    AudioCodecPairId, AudioEncoderFactory, AudioEncoderOptions, CodecError, Environment,
    OwnedAudioEncoderOptions, SdpAudioFormat,
};

#[test]
fn owning_sdk_options_reach_actual_encoder_creation_and_survive_source_drop() {
    let factory = AudioEncoderFactory::builtin().unwrap();
    let environment = Environment::builder().build().unwrap();
    let format = SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1);
    let id = AudioCodecPairId::create();
    // The actual G711 constructor stores the full signed field; it does not
    // impose the Opus payload assertion. No RTP sender is installed here.
    for payload_type in [i32::MIN, -1, 0, 127, 128, i32::MAX] {
        let source = AudioEncoderOptions {
            payload_type,
            codec_pair_id: Some(id.clone()),
        };
        let options = OwnedAudioEncoderOptions::from_values(&source);
        let copied = options.clone();
        drop(source);
        drop(options);
        let encoder = factory
            .create_with_options(&environment, &format, &copied)
            .unwrap()
            .unwrap();
        assert_eq!(copied.payload_type(), payload_type);
        assert_eq!(copied.codec_pair_id().unwrap(), id);
        drop(copied);
        assert!(encoder.encode(100, &[0; 80]).unwrap().bytes.is_empty());
        let packet = encoder.encode(180, &[0; 80]).unwrap();
        assert_eq!(packet.info.primary.payload_type, payload_type);
        assert_eq!(packet.info.primary.encoded_timestamp, 100);
        assert_eq!(packet.bytes.len(), 160);
    }
    let mut options = OwnedAudioEncoderOptions::default();
    options.set_payload_type(200);
    let encoder = factory
        .create_with_options(&environment, &format, &options)
        .unwrap()
        .unwrap();
    drop(factory);
    drop(environment);
    drop(options);
    drop(id);
    std::thread::spawn(move || {
        assert!(encoder.encode(200, &[0; 80]).unwrap().bytes.is_empty());
        let packet = encoder.encode(280, &[0; 80]).unwrap();
        assert_eq!(packet.info.primary.payload_type, 200);
        assert_eq!(packet.bytes.len(), 160);
    })
    .join()
    .unwrap();
}

#[test]
fn owning_sdk_options_preserve_backend_admission_and_unsupported_results() {
    let environment = Environment::builder().build().unwrap();
    let factory = AudioEncoderFactory::builtin().unwrap();
    let options = OwnedAudioEncoderOptions::default();
    assert_eq!(options.payload_type(), -1);
    let opus = SdpAudioFormat::new(b"opus".to_vec(), 48000, 2);
    assert!(matches!(
        factory.create_with_options(&environment, &opus, &options),
        Err(CodecError::InvalidConfiguration)
    ));
    let unknown = SdpAudioFormat::new(b"unknown".to_vec(), i32::MIN, usize::MAX);
    assert!(
        factory
            .create_with_options(&environment, &unknown, &options)
            .unwrap()
            .is_none()
    );
    // Carrier has its own native dispatch and does not acquire the stock Opus
    // payload assertion simply from the name of its advertised codec.
    let carrier = AudioEncoderFactory::with_opus_frames().unwrap();
    assert!(
        carrier
            .create_with_options(&environment, &opus, &options)
            .unwrap()
            .is_some()
    );
    assert_eq!(options.payload_type(), -1);
    assert!(options.codec_pair_id().is_none());
}
