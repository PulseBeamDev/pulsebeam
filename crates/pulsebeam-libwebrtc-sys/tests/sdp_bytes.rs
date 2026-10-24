use pulsebeam_webrtc_sys::*;
use std::time::{Duration, Instant};

struct RawFormats;
impl VideoEncoderFactoryProvider for RawFormats {
    fn supported_formats(&self) -> Vec<SdpVideoFormat> {
        let mut format = SdpVideoFormat::new("VP8");
        let mut fields = format.fields();
        fields
            .parameters
            .insert(b"x-native-bytes".to_vec(), vec![0xff, 0xfe]);
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
struct RawDecoderFormats;
impl VideoDecoderFactoryProvider for RawDecoderFormats {
    fn supported_formats(&self) -> Vec<SdpVideoFormat> {
        RawFormats.supported_formats()
    }
    fn query_support(
        &self,
        _: &SdpVideoFormat,
        _: bool,
        _: Option<&Resolution>,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        Ok(CodecSupport {
            supported: true,
            power_efficient: false,
        })
    }
    fn create(&self, _: VideoCodecCreationContext) -> Result<Box<dyn VideoDecoder>, CodecError> {
        Err(CodecError::UnsupportedFormat)
    }
}
#[test]
fn native_offer_completion_keeps_non_utf8_protocol_bytes_without_an_extra_terminal() {
    let encoder = VideoEncoderFactoryHandle::from_provider(RawFormats).unwrap();
    let factory = PeerConnectionFactory::builder()
        .video_encoder_factory(encoder)
        // Both native capability sides advertise the parameter. The SDK may
        // drop encoder-only parameters during its common-codec negotiation.
        .video_decoder_factory(VideoDecoderFactoryHandle::from_provider(RawDecoderFormats).unwrap())
        .build()
        .unwrap();
    let mut peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    // Full native capability values remain readable; the legacy text projection
    // reports a binding error instead of constructing a fatal CXX rust::String.
    assert!(
        factory
            .get_rtp_sender_capabilities(MediaType::Video)
            .codecs()
            .iter()
            .any(|codec| codec
                .base_codec()
                .parameters()
                .iter()
                .any(|parameter| parameter.key == b"x-native-bytes"
                    && parameter.value == [0xff, 0xfe]))
    );
    assert_eq!(
        peer.video_sender_capabilities().unwrap_err().kind,
        PeerErrorKind::UnrepresentableCodecCapability
    );
    let source = factory.create_video_source().unwrap();
    let track = factory.create_video_track("raw-sdp", &source).unwrap();
    let transceiver = peer
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    // SDP generation may discard codec-specific unknown capability parameters.
    // Stream IDs are serialized by the SDK and accept these protocol bytes.
    transceiver
        .sender()
        .set_stream_id_bytes(&[b"raw-stream-\xff\xfe".to_vec()]);
    let operation = peer.create_offer().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let description = loop {
        if let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::OperationCompleteBytes {
                    operation_id,
                    description,
                } if operation_id == operation => break description,
                PeerConnectionEvent::OperationComplete(done) if done.operation_id == operation => {
                    panic!("native byte offer was not retained: {done:?}")
                }
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "native offer callback timeout");
        std::thread::yield_now();
    };
    assert!(
        description
            .sdp
            .windows(2)
            .any(|bytes| bytes == [0xff, 0xfe])
    );
    assert!(String::from_utf8(description.sdp.clone()).is_err());
    assert_eq!(peer.event_observation().admitted_operations, 0);
    while let Some(event) = peer.try_next_event() {
        assert!(
            !matches!(event, PeerConnectionEvent::OperationCompleteBytes { operation_id, .. } if operation_id == operation)
        );
        assert!(
            !matches!(event, PeerConnectionEvent::OperationComplete(done) if done.operation_id == operation)
        );
    }
    let apply = peer
        .set_local_description_bytes(description.clone())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::OperationComplete(done) if done.operation_id == apply => {
                    assert!(done.result.unwrap().is_none());
                    break;
                }
                PeerConnectionEvent::OperationFailedBytes {
                    operation_id,
                    error,
                } if operation_id == apply => panic!("native byte apply failed: {error:?}"),
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "native byte SDP apply timeout");
        std::thread::yield_now();
    }
    let snapshot = peer.descriptions_bytes().unwrap().pending_local.unwrap();
    assert!(snapshot.sdp.windows(2).any(|bytes| bytes == [0xff, 0xfe]));
    assert_eq!(
        peer.descriptions().unwrap_err().kind,
        PeerErrorKind::DescriptionNotUtf8
    );
    peer.close().unwrap();
    drop(peer);
    drop(factory);
    assert!(
        description
            .sdp
            .windows(2)
            .any(|bytes| bytes == [0xff, 0xfe])
    );
}

#[test]
fn textual_description_projection_is_checked_and_byte_values_are_independent() {
    let value = SessionDescriptionBytes {
        kind: SessionDescriptionType::Offer,
        sdp: vec![0xff, 0],
    };
    let clone = value.clone();
    assert_eq!(
        SessionDescription::try_from(value)
            .unwrap_err()
            .into_bytes(),
        clone.sdp
    );
    let text = SessionDescription {
        kind: SessionDescriptionType::Answer,
        sdp: "v=0\r\n".into(),
    };
    assert_eq!(
        SessionDescription::try_from(SessionDescriptionBytes::from(text.clone())).unwrap(),
        text
    );
}
