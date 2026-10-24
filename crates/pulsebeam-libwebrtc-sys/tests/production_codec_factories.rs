use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Encoder(Arc<AtomicUsize>);
impl Drop for Encoder {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl VideoEncoderFactoryProvider for Encoder {
    fn supported_formats(&self) -> Vec<SdpVideoFormat> {
        let mut format = SdpVideoFormat::new("VP8");
        let mut fields = format.fields();
        fields
            .parameters
            .insert(b"x-actor-factory".to_vec(), vec![0xff]);
        format.set_fields(fields);
        vec![format]
    }
    fn query_support(
        &self,
        _: &SdpVideoFormat,
        _: Option<&[u8]>,
        _: Option<&Resolution>,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        Ok(CodecSupport {
            supported: true,
            power_efficient: false,
        })
    }
    fn create(&self, _: VideoCodecCreationContext) -> Result<Box<dyn VideoEncoder>, CodecError> {
        Err(CodecError::UnsupportedFormat)
    }
}
#[test]
fn explicitly_selected_actor_factories_keep_capabilities_and_registration_through_migration() {
    let drops = Arc::new(AtomicUsize::new(0));
    let encoder = VideoEncoderFactoryHandle::from_provider(Encoder(drops.clone())).unwrap();
    let decoder = VideoDecoderFactoryHandle::builtin_vp8().unwrap();
    let audio_encoder = AudioEncoderFactory::builtin().unwrap();
    let audio_decoder = AudioDecoderFactory::builtin().unwrap();
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .video_encoder_factory(&encoder)
        .video_decoder_factory(&decoder)
        .audio_encoder_factory(&audio_encoder)
        .audio_decoder_factory(&audio_decoder)
        .build()
        .unwrap();
    drop(encoder);
    drop(decoder);
    drop(audio_encoder);
    drop(audio_decoder);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    std::thread::spawn(move || {
        let capabilities = session
            .get_rtp_sender_capabilities(MediaType::Video)
            .unwrap();
        assert!(capabilities.codecs().iter().any(|codec| {
            codec
                .base_codec()
                .parameters()
                .iter()
                .any(|parameter| parameter.key == b"x-actor-factory" && parameter.value == [0xff])
        }));
        let receive = session
            .get_rtp_receiver_capabilities(MediaType::Video)
            .unwrap();
        assert!(
            receive
                .codecs()
                .iter()
                .any(|codec| codec.base_codec().name_bytes() == b"VP8")
        );
        // Explicit stock audio selection must not silently substitute the Opus
        // carrier required by this separate compressed-input convenience.
        assert_eq!(
            session.create_opus_source(1).unwrap_err().kind,
            PeerErrorKind::UnsupportedOperation
        );
        session.close_peer(peer).unwrap();
        session.shutdown().unwrap();
        drop(session);
        assert!(!capabilities.codecs().is_empty());
    })
    .join()
    .unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
#[test]
fn absent_audio_choice_selects_stock_pcm_and_conflicting_input_is_rejected() {
    let mut session = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    assert!(session.create_pcm_source().is_ok());
    assert!(session.create_opus_source(1).is_err());
    session.shutdown().unwrap();
    drop(session);
    let encoder = VideoEncoderFactoryHandle::builtin().unwrap();
    let result = ProductionSession::builder(ProductionSessionConfig {
        video_format: Some(VideoCodecFormat {
            name: "VP8".into(),
            parameters: vec![],
            scalability_modes: vec![],
        }),
    })
    .video_encoder_factory(&encoder)
    .build();
    assert!(matches!(
        result,
        Err(PeerError {
            kind: PeerErrorKind::InvalidParameter,
            ..
        })
    ));
}
#[test]
fn public_builder_provider_conveniences_install_the_selected_registration() {
    let drops = Arc::new(AtomicUsize::new(0));
    let decoder = VideoDecoderFactoryHandle::builtin().unwrap();
    let session = ProductionSession::builder(ProductionSessionConfig::default())
        .video_decoder_factory(&decoder)
        .video_encoder_provider(Encoder(drops.clone()))
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(session);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let factory = PeerConnectionFactory::builder()
        .video_decoder_factory(decoder)
        .video_encoder_provider(Encoder(drops.clone()))
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(factory);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}
