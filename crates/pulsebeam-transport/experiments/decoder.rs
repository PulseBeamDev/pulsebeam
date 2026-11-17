//! Shared actual H264 decoder for corpus observations and tiny codec controls.
use super::*;
use std::sync::mpsc::{Receiver, Sender, channel};
struct DecodedOutput(Sender<VideoFrame>);
impl DecodedOutput {
    fn capture(&self, frame: &RawVideoFrame) {
        self.0.send(frame.to_legacy().unwrap()).unwrap();
    }
}
impl DecodedImageCallbackProvider for DecodedOutput {
    fn decoded(&self, frame: &mut RawVideoFrame) -> i32 {
        self.capture(frame);
        0
    }
    fn decoded_with_time(&self, frame: &mut RawVideoFrame, _: i64) -> i32 {
        self.capture(frame);
        0
    }
    fn decoded_with_metadata(&self, frame: &mut RawVideoFrame, _: Option<i32>, _: Option<u8>) {
        self.capture(frame);
    }
}
pub(super) struct Decoder {
    product: OwnedVideoDecoder,
    pub(super) output: Receiver<VideoFrame>,
    _sink: DecodedImageCallbackSink,
    _environment: Environment,
    #[cfg(test)]
    pub(super) last_input: Option<usize>,
    pub(super) rejected: u64,
    #[cfg(test)]
    pub(super) waiting_for_key: bool,
}
impl Decoder {
    pub(super) fn new() -> Self {
        let environment = Environment::builder().build().unwrap();
        let factory = VideoDecoderFactoryHandle::builtin().unwrap();
        let mut product = factory
            .create_decoder(&environment, &SdpVideoFormat::new("H264"))
            .unwrap();
        let mut settings = OwnedVideoDecoderSettings::default();
        let mut fields = settings.fields();
        fields.codec_type = 4; // Exact SDK H264 ordinal used by owned decoder settings.
        fields.number_of_cores = 1;
        fields.max_render_width = 1280;
        fields.max_render_height = 720;
        settings.set_fields(fields).unwrap();
        assert!(product.configure(&settings));
        let (sent, output) = channel();
        let sink = DecodedImageCallbackSink::new(DecodedOutput(sent));
        assert_eq!(product.register_callback(Some(&sink)), 0);
        Self {
            product,
            output,
            _sink: sink,
            _environment: environment,
            #[cfg(test)]
            last_input: None,
            rejected: 0,
            #[cfg(test)]
            waiting_for_key: false,
        }
    }
    pub(super) fn decode_image(&mut self, image: &EncodedImage, missing: bool, at_ms: i64) -> bool {
        if self
            .product
            .decode_with_missing_frames(image, missing, at_ms)
            .unwrap()
            < 0
        {
            self.rejected += 1;
            #[cfg(test)]
            {
                self.waiting_for_key = true;
            }
            false
        } else {
            true
        }
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        assert_eq!(self.product.release(), 0);
    }
}
