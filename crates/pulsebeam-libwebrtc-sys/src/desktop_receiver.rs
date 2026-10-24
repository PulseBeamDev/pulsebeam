//! Actual DesktopCapturer and delegated-selection callback registrations.
use crate::ffi;
#[cfg(feature = "native")]
use crate::{CapturedDesktopFrame, provider_panic::catch_provider};
#[cfg(feature = "native")]
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
#[cfg(feature = "native")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopCaptureResult {
    Success,
    TemporaryError,
    PermanentError,
    Unknown(i32),
}
#[cfg(feature = "native")]
pub trait DesktopCaptureReceiver: Send + 'static {
    fn on_frame_capture_start(&mut self) {}
    /// Frame ownership is native, but shared SDK pixels are read-only. CPU SDK
    /// copies or owning snapshots are available; texture frames remain truthful.
    /// This mode bypasses publication into the binding's ordinary video source.
    fn on_capture_result(
        &mut self,
        result: DesktopCaptureResult,
        frame: Option<CapturedDesktopFrame>,
    );
}
#[cfg(feature = "native")]
pub trait DesktopSelectionObserver: Send + 'static {
    fn on_selection(&mut self);
    fn on_cancelled(&mut self);
    fn on_error(&mut self);
}
#[cfg(feature = "native")]
macro_rules! registration {
    ($handle:ident, $state:ident, $trait:ident) => {
        struct $state {
            body: Mutex<Option<Box<dyn $trait>>>,
            failed: AtomicBool,
        }
        impl Drop for $state {
            fn drop(&mut self) {
                let retired = self
                    .body
                    .get_mut()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                let _ = catch_provider(|| drop(retired));
            }
        }
        #[derive(Clone)]
        pub struct $handle(Arc<$state>);
        impl $handle {
            pub fn new(body: impl $trait) -> Self {
                Self(Arc::new($state {
                    body: Mutex::new(Some(Box::new(body))),
                    failed: AtomicBool::new(false),
                }))
            }
            pub fn has_failed(&self) -> bool {
                self.0.failed.load(Ordering::Acquire)
            }
            fn invoke(&self, call: impl FnOnce(&mut dyn $trait)) {
                if self.has_failed() {
                    return;
                }
                let mut body = self.0.body.lock().unwrap_or_else(|e| e.into_inner());
                if self.has_failed() {
                    return;
                }
                let Some(callback) = body.as_mut() else {
                    return;
                };
                if catch_provider(|| call(callback.as_mut())).is_err() {
                    self.0.failed.store(true, Ordering::Release);
                    let retired = body.take();
                    drop(body);
                    let _ = catch_provider(|| drop(retired));
                }
            }
        }
    };
}
#[cfg(feature = "native")]
registration!(
    DesktopCaptureReceiverHandle,
    ReceiverState,
    DesktopCaptureReceiver
);
#[cfg(feature = "native")]
registration!(
    DesktopSelectionObserverHandle,
    SelectionState,
    DesktopSelectionObserver
);
pub(crate) struct RustDesktopCaptureReceiver {
    #[cfg(feature = "native")]
    handle: Option<DesktopCaptureReceiverHandle>,
}
pub(crate) struct RustDesktopSelectionObserver {
    #[cfg(feature = "native")]
    handle: Option<DesktopSelectionObserverHandle>,
}
#[cfg(feature = "native")]
impl RustDesktopCaptureReceiver {
    pub(crate) fn new(handle: Option<&DesktopCaptureReceiverHandle>) -> Box<Self> {
        Box::new(Self {
            handle: handle.cloned(),
        })
    }
}
#[cfg(feature = "native")]
impl RustDesktopSelectionObserver {
    pub(crate) fn new(handle: Option<&DesktopSelectionObserverHandle>) -> Box<Self> {
        Box::new(Self {
            handle: handle.cloned(),
        })
    }
}
pub(crate) fn desktop_receiver_present(source: &RustDesktopCaptureReceiver) -> bool {
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
pub(crate) fn desktop_selection_present(source: &RustDesktopSelectionObserver) -> bool {
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
pub(crate) fn desktop_receiver_start(source: &RustDesktopCaptureReceiver) {
    #[cfg(feature = "native")]
    {
        if let Some(handle) = &source.handle {
            handle.invoke(|body| body.on_frame_capture_start());
        }
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = source;
    }
}
pub(crate) fn desktop_receiver_result(
    source: &RustDesktopCaptureReceiver,
    result: i32,
    frame: cxx::UniquePtr<ffi::NativeDesktopFrame>,
) {
    #[cfg(feature = "native")]
    {
        let frame = (!frame.is_null()).then(|| CapturedDesktopFrame::new(frame));
        let result = match result {
            0 => DesktopCaptureResult::Success,
            1 => DesktopCaptureResult::TemporaryError,
            2 => DesktopCaptureResult::PermanentError,
            other => DesktopCaptureResult::Unknown(other),
        };
        if let Some(handle) = &source.handle {
            handle.invoke(|body| body.on_capture_result(result, frame));
        }
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = (source, result, frame);
    }
}
pub(crate) fn desktop_selection_notify(source: &RustDesktopSelectionObserver, event: u8) {
    #[cfg(feature = "native")]
    {
        if let Some(handle) = &source.handle {
            handle.invoke(|body| match event {
                0 => body.on_selection(),
                1 => body.on_cancelled(),
                _ => body.on_error(),
            });
        }
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = (source, event);
    }
}
