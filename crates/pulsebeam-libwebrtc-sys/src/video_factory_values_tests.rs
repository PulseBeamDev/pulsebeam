use crate::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct EncoderFactory {
    contexts: Arc<Mutex<Vec<VideoCodecCreationContext>>>,
    drops: Arc<AtomicUsize>,
}
impl VideoEncoderFactoryProvider for EncoderFactory {
    fn supported_formats(&self) -> Vec<SdpVideoFormat> {
        vec![SdpVideoFormat::new("H264")]
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
    fn create(
        &self,
        context: VideoCodecCreationContext,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        self.contexts.lock().unwrap().push(context);
        Err(CodecError::ConstructionFailed)
    }
}
impl Drop for EncoderFactory {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct DecoderFactory(Arc<Mutex<Vec<VideoCodecCreationContext>>>);
impl VideoDecoderFactoryProvider for DecoderFactory {
    fn supported_formats(&self) -> Vec<SdpVideoFormat> {
        vec![SdpVideoFormat::new("H264")]
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
    fn create(
        &self,
        context: VideoCodecCreationContext,
    ) -> Result<Box<dyn VideoDecoder>, CodecError> {
        self.0.lock().unwrap().push(context);
        Ok(Box::new(Decoder))
    }
}
struct Decoder;
impl VideoDecoder for Decoder {
    fn configure(&mut self, _: VideoDecoderSettings) -> Result<(), CodecError> {
        Ok(())
    }
    fn decode(&mut self, _: EncodedVideoFrame, _: DecodedImageCallback) -> Result<(), CodecError> {
        Err(CodecError::UnsupportedFormat)
    }
    fn release(&mut self) -> Result<(), CodecError> {
        Ok(())
    }
    fn info(&self) -> VideoDecoderInfo {
        VideoDecoderInfo {
            implementation_name: "context fixture".into(),
            hardware_accelerated: false,
        }
    }
}
#[test]
fn actual_native_create_and_deferred_encoder_create_deliver_independent_contexts() {
    let enc_contexts = Arc::new(Mutex::new(Vec::new()));
    let dec_contexts = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let encoder = VideoEncoderFactoryHandle::from_provider(EncoderFactory {
        contexts: enc_contexts.clone(),
        drops: drops.clone(),
    })
    .unwrap();
    let decoder =
        VideoDecoderFactoryHandle::from_provider(DecoderFactory(dec_contexts.clone())).unwrap();
    // The SDK defers provider Create until the first Encode, not InitEncode.
    // A zero-frame probe never enters that callback and cannot prove rejection.
    let result = ffi::test_codec_roundtrip(encoder.native(), decoder.native(), 1);
    assert_ne!(result.status, 0);
    assert!(!enc_contexts.lock().unwrap().is_empty());
    assert_eq!(dec_contexts.lock().unwrap().len(), 1);
    drop(encoder);
    drop(decoder);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    for context in enc_contexts
        .lock()
        .unwrap()
        .iter()
        .chain(dec_contexts.lock().unwrap().iter())
    {
        let fields = context.format.fields();
        assert_eq!(fields.name, b"H264");
        assert_eq!(
            fields
                .parameters
                .get(b"profile-level-id".as_slice())
                .unwrap(),
            b"42e01f"
        );
        // Actual native copied clock remains callable after source owners drop.
        assert!(context.environment.timestamp().microseconds().unwrap() > 0);
        let _independent_trials = context.environment.field_trials();
    }
}
