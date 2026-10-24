use pulsebeam_webrtc_sys::{
    AudioEncoderFactory, AudioProcessingConfig, AudioProcessingOptions, GainControl,
    NoiseSuppression, PeerConnectionFactory, ProcessingChoice, ProcessingImplementation,
};

#[test]
fn software_audio_processing_is_opt_in_and_reports_live_module_status() {
    let plain = PeerConnectionFactory::builder().build().unwrap();
    let initial = plain.audio_processing_state();
    assert!(!initial.has_software_module);
    assert!(!initial.echo_cancellation.platform_available);

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
    track
        .set_audio_processing_options(AudioProcessingOptions {
            echo_cancellation: ProcessingChoice::Software,
            noise_suppression: ProcessingChoice::Automatic,
            gain_control: ProcessingChoice::Disabled,
        })
        .unwrap();
    let state = configured.audio_processing_state();
    assert!(state.has_software_module);
    assert!(!state.echo_cancellation.platform_available);
    // Before a capture send starts, the effective path can be unknown. Do not
    // mistake factory configuration for proof of active capture processing.
    assert_ne!(
        state.echo_cancellation.effective,
        ProcessingImplementation::Platform
    );
}

#[test]
fn encoded_opus_cannot_claim_pcm_processing() {
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
    assert!(
        track
            .set_audio_processing_options(AudioProcessingOptions::default())
            .is_err()
    );
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
            let factory = PeerConnectionFactory::builder()
                .audio_processing(AudioProcessingConfig {
                    echo_cancellation: true,
                    noise_suppression: suppression,
                    gain_control: gain,
                })
                .build()
                .unwrap();
            assert!(factory.audio_processing_state().has_software_module);
        }
    }
}
