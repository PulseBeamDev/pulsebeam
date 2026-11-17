use pulsebeam_webrtc_sys::{
    AudioEncoderFactory, AudioProcessingConfig, GainControl, NoiseSuppression,
    PeerConnectionFactory,
};

#[test]
fn software_audio_processing_is_configured_at_factory_creation() {
    let configured = PeerConnectionFactory::builder()
        .audio_processing(AudioProcessingConfig {
            echo_cancellation: true,
            noise_suppression: NoiseSuppression::High,
            gain_control: GainControl::AdaptiveDigital,
        })
        .build()
        .unwrap();
    let source = configured.create_audio_source().unwrap();
    let track = configured.create_audio_track("processed", &source).unwrap();
    assert_eq!(track.kind(), "audio");
}

#[test]
fn encoded_opus_factory_remains_separate_from_pcm_processing() {
    assert!(
        PeerConnectionFactory::builder()
            .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
            .audio_processing(AudioProcessingConfig::default())
            .build()
            .is_err()
    );
    let factory = PeerConnectionFactory::builder()
        .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
        .build()
        .unwrap();
    let source = factory.create_encoded_audio_source(1).unwrap();
    let track = factory
        .create_encoded_audio_track("encoded", &source)
        .unwrap();
    assert_eq!(track.kind(), "audio");
}

#[test]
fn upstream_audio_processing_choices_can_be_constructed_headlessly() {
    for suppression in [
        NoiseSuppression::Off,
        NoiseSuppression::Low,
        NoiseSuppression::Moderate,
        NoiseSuppression::High,
        NoiseSuppression::VeryHigh,
    ] {
        for gain in [
            GainControl::Off,
            GainControl::AdaptiveAnalog,
            GainControl::AdaptiveDigital,
            GainControl::FixedDigital,
            GainControl::AdaptiveDigitalV2,
        ] {
            let _factory = PeerConnectionFactory::builder()
                .audio_processing(AudioProcessingConfig {
                    echo_cancellation: true,
                    noise_suppression: suppression,
                    gain_control: gain,
                })
                .build()
                .unwrap();
        }
    }
}
