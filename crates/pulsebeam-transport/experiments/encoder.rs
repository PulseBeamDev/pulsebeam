//! The same incremental H264 encoder for both controller-following sources.
use pulsebeam_webrtc_sys::*;
use std::sync::mpsc::{Receiver, Sender, channel};

struct Image {
    timestamp: u32,
    bytes: Vec<u8>,
    key: bool,
}
struct Output(Sender<Option<Image>>);
impl EncodedImageCallbackProvider for Output {
    fn encoded(
        &self,
        image: EncodedImage,
        _: Option<OwnedCodecSpecificInfo>,
    ) -> EncodedImageCallbackResult {
        let metadata = image.metadata();
        self.0
            .send(Some(Image {
                timestamp: metadata.rtp_timestamp,
                bytes: image.to_vec(),
                key: metadata.frame_type == EncodedImageFrameType::Key,
            }))
            .unwrap();
        EncodedImageCallbackResult {
            error: EncodedImageCallbackError::Ok,
            frame_id: metadata.rtp_timestamp,
            drop_next_frame: false,
        }
    }
    fn frame_dropped(&self, _: u32, _: i32, _: bool) {
        self.0.send(None).unwrap();
    }
}

pub(super) struct SourceEncoder {
    product: OwnedVideoEncoder,
    output: Receiver<Option<Image>>,
    _sink: EncodedImageCallbackSink,
    _environment: Environment,
}
impl SourceEncoder {
    #[cfg(test)]
    pub(super) fn new(start_bps: u32, max_bps: u32, frame_drop: bool) -> Self {
        Self::with_format(320, 180, 25, start_bps, max_bps, frame_drop)
    }

    pub(super) fn with_format(
        width: u32,
        height: u32,
        fps: u32,
        start_bps: u32,
        max_bps: u32,
        frame_drop: bool,
    ) -> Self {
        let environment = Environment::builder().build().unwrap();
        let factory = VideoEncoderFactoryHandle::builtin().unwrap();
        let mut product = factory
            .create_encoder(&environment, &SdpVideoFormat::new("H264"))
            .unwrap();
        let (sent, output) = channel();
        let sink = EncodedImageCallbackSink::new(Output(sent));
        assert_eq!(product.register_callback(Some(&sink)), 0);
        let mut codec = OwnedVideoCodec::default();
        let mut fields = codec.fields();
        fields.codec_type = VideoCodecType::H264;
        fields.width = u16::try_from(width).unwrap();
        fields.height = u16::try_from(height).unwrap();
        fields.max_framerate = fps;
        fields.start_bitrate_kbps = start_bps.min(max_bps) / 1000;
        fields.min_bitrate_kbps = 50;
        fields.max_bitrate_kbps = max_bps / 1000;
        fields.active = true;
        fields.qp_max = 51;
        fields.frame_drop_enabled = frame_drop;
        fields.codec_specific = Some(VideoCodecSpecificConfiguration::H264 {
            key_frame_interval: 60,
            temporal_layers: 1,
        });
        codec.set_fields(&fields).unwrap();
        assert_eq!(
            product.initialize(
                &codec,
                VideoEncoderSettingsValue {
                    loss_notification: false,
                    number_of_cores: 1,
                    max_payload_size: 1100,
                    encoder_thread_limit: Some(1),
                }
            ),
            0
        );
        Self {
            product,
            output,
            _sink: sink,
            _environment: environment,
        }
    }
    pub(super) fn encode(
        &mut self,
        raw: &VideoFrame,
        target_bps: u32,
        fps: f64,
        key: bool,
    ) -> Option<(Vec<u8>, bool)> {
        assert!(target_bps > 0 && fps > 0.0);
        let mut allocation = OwnedVideoBitrateAllocation::default();
        allocation.set_bitrate(0, 0, target_bps).unwrap();
        self.product
            .set_rates(&OwnedVideoRateControl::new(allocation.fields(), fps, None).unwrap());
        let kind = if key {
            EncodedImageFrameType::Key
        } else {
            EncodedImageFrameType::Delta
        };
        assert_eq!(
            self.product
                .encode(&RawVideoFrame::from_legacy(raw).unwrap(), Some(&[kind])),
            0
        );
        // This built-in single-layer H264 product is synchronous; absence is a
        // source omission, not a fabricated access unit or transport loss.
        let image = self.output.try_recv().ok().flatten();
        assert!(self.output.try_recv().is_err());
        image.map(|image| {
            assert_eq!(image.timestamp, raw.rtp_timestamp);
            assert!(!image.bytes.is_empty());
            (image.bytes, image.key)
        })
    }
}
impl Drop for SourceEncoder {
    fn drop(&mut self) {
        assert_eq!(self.product.release(), 0);
    }
}
