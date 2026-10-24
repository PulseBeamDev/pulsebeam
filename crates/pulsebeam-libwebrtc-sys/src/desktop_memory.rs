//! Owning Linux SDK SharedMemory products and the actual factory virtual call.
use crate::ffi;
#[cfg(feature = "native")]
use crate::provider_panic::catch_provider;
#[cfg(all(feature = "native", target_os = "linux"))]
use std::os::fd::IntoRawFd;
#[cfg(all(feature = "native", unix))]
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(feature = "native")]
use std::{
    cell::Cell,
    marker::PhantomData,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(feature = "native")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopMemoryInfo {
    pub size: usize,
    pub id: i32,
    pub has_handle: bool,
}
/// Owns an SDK SharedMemory subclass and its buffer. Consuming it into an SDK
/// frame/factory transfers storage and dependency leases, not borrowed pointers.
/// Send, not Sync: this library's products have only owned buffers/FDs and Send
/// dependency tokens, with no role-affine capturer or arbitrary SDK subclass.
#[cfg(feature = "native")]
pub struct DesktopMemory {
    pub(crate) native: cxx::UniquePtr<ffi::NativeDesktopMemory>,
    _exclusive: PhantomData<Cell<()>>,
}
#[cfg(feature = "native")]
unsafe impl Send for DesktopMemory {}
#[cfg(feature = "native")]
impl DesktopMemory {
    fn create(
        native: cxx::UniquePtr<ffi::NativeDesktopMemory>,
        error: i32,
    ) -> std::io::Result<Self> {
        if native.is_null() {
            return Err(std::io::Error::from_raw_os_error(error));
        }
        Ok(Self {
            native,
            _exclusive: PhantomData,
        })
    }
    /// A shrink-sealed Linux memfd, zero initialized by the kernel. Failure is
    /// returned unchanged; no heap or stock-factory substitution is attempted.
    pub fn allocate(size: usize, id: i32) -> std::io::Result<Self> {
        let mut error = 0;
        let native = ffi::desktop_memory_new(size, id, false, &mut error);
        Self::create(native, error)
    }
    /// Explicit CPU-only storage with SDK kInvalidHandle, not a mapped buffer.
    pub fn heap(size: usize, id: i32) -> std::io::Result<Self> {
        let mut error = 0;
        let native = ffi::desktop_memory_new(size, id, true, &mut error);
        Self::create(native, error)
    }
    /// Consume a writable FD with F_SEAL_SHRINK and sufficient length. Seals are
    /// checked before size, avoiding truncation races. External content writers
    /// must follow the application's shared-buffer synchronization protocol.
    #[cfg(target_os = "linux")]
    pub fn from_fd(fd: OwnedFd, size: usize, id: i32) -> std::io::Result<Self> {
        let mut error = 0;
        let native = ffi::desktop_memory_map(fd.into_raw_fd(), size, id, &mut error);
        Self::create(native, error)
    }
    fn value(&self) -> &ffi::NativeDesktopMemory {
        self.native.as_ref().expect("owned SDK shared memory")
    }
    pub fn info(&self) -> DesktopMemoryInfo {
        let v = ffi::desktop_memory_info(self.value());
        DesktopMemoryInfo {
            size: v.size,
            id: v.id,
            has_handle: v.has_handle,
        }
    }
    pub fn read(&self, offset: usize, size: usize) -> std::io::Result<Vec<u8>> {
        let out = ffi::desktop_memory_read(self.value(), offset, size);
        if out.status == 0 {
            Ok(out.bytes)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "SDK shared memory range unavailable",
            ))
        }
    }
    pub fn write(&mut self, offset: usize, bytes: &[u8]) -> std::io::Result<()> {
        if ffi::desktop_memory_write(self.native.pin_mut(), offset, bytes) {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "SDK shared memory range unavailable",
            ))
        }
    }
    #[cfg(unix)]
    pub fn try_clone_fd(&self) -> std::io::Result<Option<OwnedFd>> {
        owned_fd(ffi::desktop_memory_fd(self.value()))
    }
    /// Retained through the native product's final destruction, even after its
    /// creating factory registration has been replaced or destroyed.
    pub fn retain(&mut self, dependency: impl Send + 'static) {
        ffi::desktop_memory_retain(
            self.native.pin_mut(),
            Box::new(RustDesktopMemoryLease {
                value: Some(Box::new(dependency)),
            }),
        );
    }
}
#[cfg(all(feature = "native", unix))]
pub(crate) fn owned_fd(out: ffi::FfiDesktopFd) -> std::io::Result<Option<OwnedFd>> {
    if out.error != 0 {
        return Err(std::io::Error::from_raw_os_error(out.error));
    }
    // Only successful F_DUPFD_CLOEXEC outputs enter this branch. Ownership is
    // transferred exactly once from C++, including descriptor zero.
    Ok((out.fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(out.fd) }))
}
#[cfg(feature = "native")]
pub trait DesktopMemoryFactory: Send + 'static {
    /// Actual native request. None is native allocation rejection, not fallback.
    fn create_shared_memory(&mut self, size: usize) -> Option<DesktopMemory>;
}
#[cfg(feature = "native")]
struct FactoryState {
    body: Mutex<Option<Box<dyn DesktopMemoryFactory>>>,
    active_thread: Mutex<Option<std::thread::ThreadId>>,
    failed: AtomicBool,
}
#[cfg(feature = "native")]
struct ActiveCall<'a>(&'a Mutex<Option<std::thread::ThreadId>>);
#[cfg(feature = "native")]
impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}
#[cfg(feature = "native")]
impl Drop for FactoryState {
    fn drop(&mut self) {
        let retired = self
            .body
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let _ = catch_provider(|| drop(retired));
    }
}
#[cfg(feature = "native")]
#[derive(Clone)]
pub struct DesktopMemoryFactoryHandle(Arc<FactoryState>);
#[cfg(feature = "native")]
impl DesktopMemoryFactoryHandle {
    pub fn new(body: impl DesktopMemoryFactory) -> Self {
        Self(Arc::new(FactoryState {
            body: Mutex::new(Some(Box::new(body))),
            active_thread: Mutex::new(None),
            failed: AtomicBool::new(false),
        }))
    }
    /// Panic or undersized-product quarantine, separate from ordinary None.
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
    fn create(&self, size: usize) -> cxx::UniquePtr<ffi::NativeDesktopMemory> {
        if self.has_failed() {
            return cxx::UniquePtr::null();
        }
        let current = std::thread::current().id();
        // Reject recursive mutable use of this body, without rejecting nested
        // independent factories or serializing unrelated provider instances.
        if self
            .0
            .active_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            == Some(&current)
        {
            return cxx::UniquePtr::null();
        }
        let mut body = self.0.body.lock().unwrap_or_else(|e| e.into_inner());
        if self.has_failed() {
            return cxx::UniquePtr::null();
        }
        let Some(callback) = body.as_mut() else {
            return cxx::UniquePtr::null();
        };
        *self
            .0
            .active_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(current);
        let _active = ActiveCall(&self.0.active_thread);
        let result = catch_provider(|| callback.create_shared_memory(size));
        match result {
            Ok(Some(memory)) if memory.info().size >= size => memory.native,
            Ok(None) => cxx::UniquePtr::null(),
            invalid => {
                self.0.failed.store(true, Ordering::Release);
                let retired = body.take();
                drop(body);
                // An undersized product can itself retain caller destructors.
                let _ = catch_provider(|| drop((retired, invalid)));
                cxx::UniquePtr::null()
            }
        }
    }
}
pub(crate) struct RustDesktopMemoryFactory {
    #[cfg(feature = "native")]
    handle: Option<DesktopMemoryFactoryHandle>,
}
#[cfg(feature = "native")]
impl RustDesktopMemoryFactory {
    pub(crate) fn new(handle: Option<&DesktopMemoryFactoryHandle>) -> Box<Self> {
        Box::new(Self {
            handle: handle.cloned(),
        })
    }
}
pub(crate) struct RustDesktopMemoryLease {
    #[cfg(feature = "native")]
    value: Option<Box<dyn Send>>,
}
impl Drop for RustDesktopMemoryLease {
    fn drop(&mut self) {
        #[cfg(feature = "native")]
        {
            let retired = self.value.take();
            let _ = catch_provider(|| drop(retired));
        }
    }
}
pub(crate) fn desktop_memory_factory_present(source: &RustDesktopMemoryFactory) -> bool {
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
pub(crate) fn desktop_memory_factory_create(
    source: &RustDesktopMemoryFactory,
    size: usize,
) -> cxx::UniquePtr<ffi::NativeDesktopMemory> {
    #[cfg(feature = "native")]
    {
        source
            .handle
            .as_ref()
            .map_or_else(cxx::UniquePtr::null, |handle| handle.create(size))
    }
    #[cfg(not(feature = "native"))]
    {
        let _ = (source, size);
        cxx::UniquePtr::null()
    }
}
