//! Owning SDK camera options. Unsafe pinned PipeWire session initialization is
//! explicitly rejected, not replaced with a V4L2 camera.
#[cfg(feature = "native")]
use crate::{
    ffi,
    peer::{FactoryInner, PeerError, PeerErrorKind},
    provider_panic::catch_provider,
};
#[cfg(feature = "native")]
use std::{
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};

#[cfg(all(feature = "native", unix))]
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};

#[cfg(feature = "native")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CameraInitializationStatus {
    Success,
    Uninitialized,
    Unavailable,
    Denied,
    Error,
    Unknown(i32),
}
#[cfg(feature = "native")]
impl CameraInitializationStatus {
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::Success,
            1 => Self::Uninitialized,
            2 => Self::Unavailable,
            3 => Self::Denied,
            4 => Self::Error,
            other => Self::Unknown(other),
        }
    }
}
#[cfg(feature = "native")]
pub trait CameraInitializationObserver: Send + 'static {
    fn on_initialized(&mut self, status: CameraInitializationStatus);
}
#[cfg(feature = "native")]
struct ObserverState {
    callback: Mutex<Option<Box<dyn CameraInitializationObserver>>>,
    failed: AtomicBool,
    status: AtomicI32,
}
#[cfg(feature = "native")]
impl Drop for ObserverState {
    fn drop(&mut self) {
        let retired = self
            .callback
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let _ = catch_provider(|| drop(retired));
    }
}
#[cfg(feature = "native")]
#[derive(Clone)]
pub struct CameraInitializationObserverHandle(Arc<ObserverState>);
#[cfg(feature = "native")]
impl CameraInitializationObserverHandle {
    pub fn new(observer: impl CameraInitializationObserver) -> Self {
        Self(Arc::new(ObserverState {
            callback: Mutex::new(Some(Box::new(observer))),
            failed: AtomicBool::new(false),
            status: AtomicI32::new(-1),
        }))
    }
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
    /// Last actual SDK notification, not a configuration echo or SDK query.
    pub fn last_notification(&self) -> Option<CameraInitializationStatus> {
        let status = self.0.status.load(Ordering::Acquire);
        (status != -1).then(|| CameraInitializationStatus::from_native(status))
    }
}
pub(crate) struct RustCameraInitializationObserver {
    #[cfg(feature = "native")]
    observer: Option<CameraInitializationObserverHandle>,
}
pub(crate) fn camera_options_initialized(source: &RustCameraInitializationObserver, status: i32) {
    #[cfg(feature = "native")]
    {
        let Some(handle) = &source.observer else {
            return;
        };
        handle.0.status.store(status, Ordering::Release);
        if handle.has_failed() {
            return;
        }
        let mut callback = handle.0.callback.lock().unwrap_or_else(|e| e.into_inner());
        let Some(body) = callback.as_mut() else {
            return;
        };
        if catch_provider(|| body.on_initialized(CameraInitializationStatus::from_native(status)))
            .is_err()
        {
            handle.0.failed.store(true, Ordering::Release);
            let retired = callback.take();
            drop(callback);
            let _ = catch_provider(|| drop(retired));
        }
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = (source, status);
    }
}
#[cfg(feature = "native")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CameraOptionSettings {
    pub allow_v4l2: bool,
    /// None on artifacts without the native PipeWire feature.
    pub allow_pipewire: Option<bool>,
}
#[cfg(feature = "native")]
pub struct CameraCaptureOptions {
    pub(crate) native: cxx::UniquePtr<ffi::NativeCameraOptions>,
    pub(crate) factory: Rc<FactoryInner>,
    // SDK set_pipewire_fd borrows an integer. This duplicate owns that input;
    // no unsafe SDK PipeWire initialization is entered or consumes it.
    #[cfg(unix)]
    fd: Option<Arc<OwnedFd>>,
}
#[cfg(feature = "native")]
impl CameraCaptureOptions {
    pub(crate) fn new(
        native: cxx::UniquePtr<ffi::NativeCameraOptions>,
        factory: Rc<FactoryInner>,
    ) -> Self {
        Self {
            native,
            factory,
            #[cfg(unix)]
            fd: None,
        }
    }
    pub(crate) fn value(&self) -> &ffi::NativeCameraOptions {
        self.native.as_ref().expect("owned SDK camera options")
    }
    pub(crate) fn compatible(&self, factory: &Rc<FactoryInner>) -> Result<(), PeerError> {
        self.factory.check_control_call()?;
        if !Rc::ptr_eq(&self.factory, factory) {
            return Err(error(
                PeerErrorKind::InvalidState,
                "camera options belong to another factory",
            ));
        }
        Ok(())
    }
    pub(crate) fn consumable(&self, factory: &Rc<FactoryInner>) -> Result<(), PeerError> {
        self.compatible(factory)?;
        if self.settings()?.allow_pipewire == Some(true) {
            return Err(error(
                PeerErrorKind::UnsupportedOperation,
                "pinned PipeWire camera session lacks safe shutdown ordering",
            ));
        }
        Ok(())
    }
    pub fn try_clone(&self) -> Result<Self, PeerError> {
        self.factory.check_control_call()?;
        let native = ffi::camera_options_clone(self.value());
        if native.is_null() {
            return Err(error(
                PeerErrorKind::UnsupportedOperation,
                "SDK camera options unavailable",
            ));
        }
        Ok(Self {
            native,
            factory: self.factory.clone(),
            #[cfg(unix)]
            fd: self.fd.clone(),
        })
    }
    pub fn settings(&self) -> Result<CameraOptionSettings, PeerError> {
        self.factory.check_control_call()?;
        let flags = ffi::camera_options_flags(self.value());
        Ok(CameraOptionSettings {
            allow_v4l2: flags & 1 != 0,
            allow_pipewire: (flags & 2 != 0).then_some(flags & 4 != 0),
        })
    }
    pub fn set_settings(&mut self, settings: CameraOptionSettings) -> Result<(), PeerError> {
        let current = self.settings()?;
        if current.allow_pipewire.is_none() && settings.allow_pipewire.is_some() {
            return Err(error(
                PeerErrorKind::UnsupportedOperation,
                "artifact lacks selected PipeWire option",
            ));
        }
        if !ffi::camera_options_write(
            self.value(),
            settings.allow_v4l2,
            settings
                .allow_pipewire
                .or(current.allow_pipewire)
                .unwrap_or(false),
        ) {
            return Err(error(
                PeerErrorKind::UnsupportedOperation,
                "artifact lacks selected PipeWire option",
            ));
        }
        Ok(())
    }
    #[cfg(unix)]
    pub fn set_pipewire_fd(&mut self, fd: BorrowedFd<'_>) -> Result<(), PeerError> {
        self.factory.check_control_call()?;
        let owned = Arc::new(
            fd.try_clone_to_owned()
                .map_err(|e| error(PeerErrorKind::Operation, &e.to_string()))?,
        );
        if !ffi::camera_options_set_fd(self.value(), owned.as_raw_fd()) {
            return Err(error(
                PeerErrorKind::UnsupportedOperation,
                "artifact lacks PipeWire descriptor input",
            ));
        }
        self.fd = Some(owned);
        Ok(())
    }
    #[cfg(unix)]
    pub fn clear_pipewire_fd(&mut self) -> Result<(), PeerError> {
        self.factory.check_control_call()?;
        if !ffi::camera_options_set_fd(self.value(), -1) {
            return Err(error(
                PeerErrorKind::UnsupportedOperation,
                "artifact lacks PipeWire descriptor input",
            ));
        }
        self.fd = None;
        Ok(())
    }
    /// Calls SDK Init and returns its actual synchronous V4L2 notification.
    /// Selected PipeWire Init is rejected before native entry: pinned session
    /// cleanup can join a loop blocked on its own callback lock. No fallback.
    pub fn initialize(
        &mut self,
        observer: Option<&CameraInitializationObserverHandle>,
    ) -> Result<CameraInitializationStatus, PeerError> {
        self.factory.check_control_call()?;
        let source = RustCameraInitializationObserver {
            observer: observer.cloned(),
        };
        match ffi::camera_options_initialize(self.value(), &source) {
            -2 => Err(error(
                PeerErrorKind::UnsupportedOperation,
                "pinned PipeWire camera session lacks safe shutdown ordering",
            )),
            -1 => Err(error(
                PeerErrorKind::UnsupportedOperation,
                "SDK camera options unavailable",
            )),
            status => Ok(CameraInitializationStatus::from_native(status)),
        }
    }
}
#[cfg(feature = "native")]
fn error(kind: PeerErrorKind, message: &str) -> PeerError {
    PeerError {
        kind,
        message: message.into(),
    }
}
