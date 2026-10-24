//! The native raw callback slot. The SDK ignores OnRawFrame's return code.
#[cfg(feature = "native")]
use crate::{CameraSettings, VideoRotation, provider_panic::catch_provider};
#[cfg(feature = "native")]
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[cfg(feature = "native")]
pub struct RawCameraFrame<'a> {
    pub bytes: &'a mut [u8],
    pub settings: CameraSettings,
    pub rotation: VideoRotation,
    pub capture_time: i64,
}
#[cfg(feature = "native")]
#[derive(Clone, Debug)]
pub struct RawCameraFrameSnapshot {
    pub bytes: Vec<u8>,
    pub settings: CameraSettings,
    pub rotation: VideoRotation,
    pub capture_time: i64,
}
#[cfg(feature = "native")]
impl RawCameraFrame<'_> {
    pub fn snapshot(&self) -> RawCameraFrameSnapshot {
        RawCameraFrameSnapshot {
            bytes: self.bytes.to_vec(),
            settings: self.settings.clone(),
            rotation: self.rotation,
            capture_time: self.capture_time,
        }
    }
}
/// Runs with the SDK capture mutex held. Do not wait for, or synchronously
/// control, this capture/session. Mutable bytes are borrowed only for this call.
/// The signed return is forwarded unchanged, but this pinned SDK ignores it.
#[cfg(feature = "native")]
pub trait RawCameraSink: Send + 'static {
    fn on_raw_frame(&mut self, frame: RawCameraFrame<'_>) -> i32;
}
#[cfg(feature = "native")]
#[derive(Clone)]
pub struct RawCameraSinkHandle(Arc<State>);
#[cfg(feature = "native")]
struct State {
    callback: Mutex<Option<Box<dyn RawCameraSink>>>,
    failed: AtomicBool,
}
#[cfg(feature = "native")]
impl RawCameraSinkHandle {
    pub fn new(sink: impl RawCameraSink) -> Self {
        Self(Arc::new(State {
            callback: Mutex::new(Some(Box::new(sink))),
            failed: AtomicBool::new(false),
        }))
    }
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
}
#[cfg(feature = "native")]
impl Drop for State {
    fn drop(&mut self) {
        let callback = self
            .callback
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let _ = catch_provider(|| drop(callback));
    }
}
pub(crate) struct RustRawCameraSink {
    #[cfg(feature = "native")]
    handle: Option<RawCameraSinkHandle>,
}
#[cfg(feature = "native")]
impl RustRawCameraSink {
    pub(crate) fn new(handle: Option<RawCameraSinkHandle>) -> Box<Self> {
        Box::new(Self { handle })
    }
}
pub(crate) fn camera_raw_sink_present(source: &RustRawCameraSink) -> bool {
    #[cfg(feature = "native")]
    {
        source.handle.is_some()
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = source;
        false
    }
}
pub(crate) fn camera_raw_frame(
    source: &RustRawCameraSink,
    bytes: &mut [u8],
    settings: crate::ffi::FfiCameraSettings,
    rotation: u16,
    capture_time: i64,
) -> i32 {
    #[cfg(feature = "native")]
    {
        let Some(handle) = &source.handle else {
            return -1;
        };
        if handle.has_failed() {
            return -1;
        }
        let mut callback = handle.0.callback.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sink) = callback.as_mut() else {
            return -1;
        };
        match catch_provider(|| {
            sink.on_raw_frame(RawCameraFrame {
                bytes,
                settings: CameraSettings::from_native(settings),
                rotation: VideoRotation::from_degrees(rotation),
                capture_time,
            })
        }) {
            Ok(code) => code,
            Err(()) => {
                handle.0.failed.store(true, Ordering::Release);
                let retired = callback.take();
                drop(callback);
                let _ = catch_provider(|| drop(retired));
                -1
            }
        }
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = (source, bytes, settings, rotation, capture_time);
        -1
    }
}
