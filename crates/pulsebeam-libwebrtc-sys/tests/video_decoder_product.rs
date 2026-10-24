use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Factory {
    contexts: Arc<Mutex<Vec<VideoCodecCreationContext>>>,
    drops: Arc<AtomicUsize>,
    releases: Arc<AtomicUsize>,
    reject: bool,
}
impl Drop for Factory {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl VideoDecoderFactoryProvider for Factory {
    fn supported_formats(&self) -> Vec<SdpVideoFormat> {
        vec![SdpVideoFormat::new("fixture")]
    }
    fn query_support(
        &self,
        _: &SdpVideoFormat,
        _: bool,
        _: Option<&Resolution>,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        Ok(CodecSupport {
            supported: !self.reject,
            power_efficient: false,
        })
    }
    fn create(
        &self,
        context: VideoCodecCreationContext,
    ) -> Result<Box<dyn VideoDecoder>, CodecError> {
        self.contexts.lock().unwrap().push(context);
        if self.reject {
            return Err(CodecError::ConstructionFailed);
        }
        Ok(Box::new(Decoder(self.releases.clone())))
    }
}
struct Decoder(Arc<AtomicUsize>);
impl VideoDecoder for Decoder {
    fn release_value(&mut self) -> Result<i32, CodecError> {
        self.release()?;
        Ok(29)
    }
    fn configure(&mut self, _: VideoDecoderSettings) -> Result<(), CodecError> {
        Ok(())
    }
    fn decode(&mut self, _: EncodedVideoFrame, _: DecodedImageCallback) -> Result<(), CodecError> {
        Err(CodecError::InvalidFrame)
    }
    fn release(&mut self) -> Result<(), CodecError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn info(&self) -> VideoDecoderInfo {
        VideoDecoderInfo {
            implementation_name: "provider\0info".into(),
            hardware_accelerated: false,
        }
    }
}
#[test]
fn standalone_decoder_owns_selected_registration_and_exact_creation_context() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let releases = Arc::new(AtomicUsize::new(0));
    let factory = VideoDecoderFactoryHandle::from_provider(Factory {
        contexts: contexts.clone(),
        drops: drops.clone(),
        releases: releases.clone(),
        reject: false,
    })
    .unwrap();
    let environment = Environment::builder().build().unwrap();
    let mut format = SdpVideoFormat::new([0xff, 0]);
    let mut fields = format.fields();
    fields.tx_mode = Some(vec![0xfe]);
    format.set_fields(fields.clone());
    let mut decoder = factory.create_decoder(&environment, &format).unwrap();
    drop(factory);
    drop(environment);
    drop(format);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(contexts.lock().unwrap()[0].format.fields(), fields);
    let info = decoder.info().unwrap();
    assert_eq!(info.fields().implementation_name, b"provider\0info");
    // The deprecated SDK default is a distinct method from GetDecoderInfo.
    assert_eq!(decoder.implementation_name_bytes().unwrap(), b"unknown");
    assert_eq!(decoder.release(), 29);
    assert_eq!(decoder.release(), 29); // Explicit calls are not cached success.
    assert_eq!(releases.load(Ordering::SeqCst), 2);
    assert!(decoder.configure(&OwnedVideoDecoderSettings::default()));
    assert_eq!(
        decoder.info().unwrap().fields().implementation_name,
        b"provider\0info"
    );
    assert_eq!(decoder.release(), 29);
    drop(decoder);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(releases.load(Ordering::SeqCst), 3);
    assert_eq!(info.fields().implementation_name, b"provider\0info");
    assert!(
        contexts.lock().unwrap()[0]
            .environment
            .timestamp()
            .microseconds()
            .is_ok()
    );
}
#[test]
fn caller_creation_rejection_returns_null_without_builtin_fallback() {
    let drops = Arc::new(AtomicUsize::new(0));
    let factory = VideoDecoderFactoryHandle::from_provider(Factory {
        contexts: Arc::new(Mutex::new(Vec::new())),
        drops: drops.clone(),
        releases: Arc::new(AtomicUsize::new(0)),
        reject: true,
    })
    .unwrap();
    let environment = Environment::builder().build().unwrap();
    assert_eq!(
        factory
            .create_decoder(&environment, &SdpVideoFormat::new("VP8"))
            .unwrap_err(),
        VideoDecoderCreationError::NativeNotCreated
    );
    assert_eq!(factory.failure_observer().unwrap().failure(), None);
    drop(factory);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
#[test]
fn observed_builtin_vp8_product_uses_real_sdk_creation_and_info() {
    let factory = VideoDecoderFactoryHandle::builtin_vp8().unwrap();
    let environment = Environment::builder().build().unwrap();
    let mut decoder = factory
        .create_decoder(&environment, &SdpVideoFormat::new("VP8"))
        .unwrap();
    assert_eq!(factory.decoder_statistics().unwrap().decoder_creations, 1);
    let info = decoder.info().unwrap();
    assert!(!info.fields().implementation_name.is_empty());
    assert!(!info.fields().is_hardware_accelerated);
    assert_eq!(decoder.release(), 0);
    drop(decoder);
    drop(factory);
    drop(environment);
    assert!(!info.fields().implementation_name.is_empty());
}
