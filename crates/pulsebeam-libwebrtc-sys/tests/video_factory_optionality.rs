use pulsebeam_webrtc_sys::*;
use std::sync::{Arc, Mutex};

struct Factory(Arc<Mutex<Vec<Option<String>>>>);
impl VideoEncoderFactory for Factory {
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        vec![VideoCodecFormat::new("VP8")]
    }
    fn implementations(&self) -> Vec<VideoCodecFormat> {
        vec![VideoCodecFormat::new("VP9")]
    }
    fn query_support(
        &self,
        _: &VideoCodecFormat,
        mode: Option<&str>,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        self.0.lock().unwrap().push(mode.map(str::to_owned));
        CodecSupport {
            supported: mode.is_none(),
            power_efficient: mode == Some(""),
        }
    }
    fn create(&self, _: &VideoCodecFormat) -> Result<Box<dyn VideoEncoder>, CodecError> {
        Err(CodecError::ConstructionFailed)
    }
}

struct PanickingPayload;
impl Drop for PanickingPayload {
    fn drop(&mut self) {
        panic!("panic-payload destructor");
    }
}
struct PanickingFactory(Arc<std::sync::atomic::AtomicUsize>);
impl VideoEncoderFactory for PanickingFactory {
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        std::panic::panic_any(PanickingPayload)
    }
    fn query_support(
        &self,
        _: &VideoCodecFormat,
        _: Option<&str>,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        panic!("poisoned factory must not be queried");
    }
    fn create(&self, _: &VideoCodecFormat) -> Result<Box<dyn VideoEncoder>, CodecError> {
        panic!("poisoned factory must not create");
    }
}
impl Drop for PanickingFactory {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        panic!("factory destructor");
    }
}

#[test]
fn factory_callback_and_destructor_panics_do_not_cross_cxx() {
    let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory = VideoEncoderFactoryHandle::new(PanickingFactory(dropped.clone())).unwrap();
    assert!(factory.try_supported_format_values().is_err());
    // Failure stays observable rather than becoming a fabricated empty list.
    assert!(factory.try_supported_format_values().is_err());
    assert!(factory.try_implementation_values().is_err());
    assert!(
        !factory
            .query_support(&VideoCodecFormat::new("VP8"), None, None)
            .supported
    );
    drop(factory);
    assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 1);
}

struct PanickingDecoderFactory(Arc<std::sync::atomic::AtomicUsize>);
impl VideoDecoderFactory for PanickingDecoderFactory {
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        std::panic::panic_any(PanickingPayload)
    }
    fn query_support(
        &self,
        _: &VideoCodecFormat,
        _: bool,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        panic!("poisoned decoder factory must not be queried");
    }
    fn create(&self, _: &VideoCodecFormat) -> Result<Box<dyn VideoDecoder>, CodecError> {
        panic!("poisoned decoder factory must not create");
    }
}
impl Drop for PanickingDecoderFactory {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        panic!("decoder factory destructor");
    }
}
#[test]
fn decoder_factory_panics_do_not_cross_cxx() {
    let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory = VideoDecoderFactoryHandle::new(PanickingDecoderFactory(dropped.clone())).unwrap();
    assert!(factory.try_supported_format_values().is_err());
    assert!(factory.try_supported_format_values().is_err());
    assert!(
        !factory
            .query_support(&VideoCodecFormat::new("VP8"), false, None)
            .supported
    );
    drop(factory);
    assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn actual_factory_query_preserves_absent_present_empty_and_present_mode() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let factory = VideoEncoderFactoryHandle::new(Factory(calls.clone())).unwrap();
    let format = VideoCodecFormat::new("VP8");
    assert_eq!(factory.supported_format_values()[0].fields().name, b"VP8");
    assert_eq!(factory.implementation_values()[0].fields().name, b"VP9");
    let absent = factory.query_support(&format, None, None);
    assert!(absent.supported);
    assert!(!absent.power_efficient);
    let empty = factory.query_support(&format, Some(""), None);
    assert!(!empty.supported);
    assert!(empty.power_efficient); // Native bits are not normalized together.
    let specified = factory.query_support(&format, Some("L1T1"), None);
    assert!(!specified.supported);
    assert!(!specified.power_efficient);
    assert_eq!(
        *calls.lock().unwrap(),
        vec![None, Some("".into()), Some("L1T1".into())]
    );
}
