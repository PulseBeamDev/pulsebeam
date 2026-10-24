//! Owned asynchronous local-network permission providers.
use crate::{SocketAddress, ffi, provider_panic::catch_provider as catch_unwind};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalNetworkPermissionStatus {
    Granted,
    Denied,
}
/// All methods execute on the native permission sequence. Do not wait for or
/// synchronously reenter the owning application actor from these callbacks.
pub trait LocalNetworkPermission: Send + Sync + 'static {
    fn should_request_permission(&self, address: &SocketAddress) -> bool {
        address.is_private_ip() || address.is_loopback_ip()
    }
    fn request_permission(
        &self,
        address: SocketAddress,
        completion: LocalNetworkPermissionCompletion,
    );
}
impl<T: LocalNetworkPermission + ?Sized> LocalNetworkPermission for Arc<T> {
    fn should_request_permission(&self, address: &SocketAddress) -> bool {
        (**self).should_request_permission(address)
    }
    fn request_permission(
        &self,
        address: SocketAddress,
        completion: LocalNetworkPermissionCompletion,
    ) {
        (**self).request_permission(address, completion);
    }
}
pub trait LocalNetworkPermissionFactory: Send + Sync + 'static {
    fn create(&self) -> Arc<dyn LocalNetworkPermission>;
}
impl<T: LocalNetworkPermissionFactory + ?Sized> LocalNetworkPermissionFactory for Arc<T> {
    fn create(&self) -> Arc<dyn LocalNetworkPermission> {
        (**self).create()
    }
}
/// Clones share a single completion. First submission wins and is dispatched
/// asynchronously on the native originating sequence. Product destruction
/// cancels queued delivery and rejects all later submissions. A true result
/// means accepted for delivery, not that delivery has already occurred.
pub struct LocalNetworkPermissionCompletion {
    inner: cxx::UniquePtr<ffi::NativeLocalNetworkPermissionCompletion>,
}
// Only an independently reference-counted state/cancellation mutex is accessed
// cross-thread. No raw thread pointer escapes the native cancellation protocol.
unsafe impl Send for LocalNetworkPermissionCompletion {}
unsafe impl Sync for LocalNetworkPermissionCompletion {}
impl LocalNetworkPermissionCompletion {
    pub fn complete(&self, status: LocalNetworkPermissionStatus) -> bool {
        ffi::local_permission_complete(
            self.inner.as_ref().unwrap(),
            status == LocalNetworkPermissionStatus::Granted,
        )
    }
    pub fn grant(&self) -> bool {
        self.complete(LocalNetworkPermissionStatus::Granted)
    }
    pub fn deny(&self) -> bool {
        self.complete(LocalNetworkPermissionStatus::Denied)
    }
}
impl Clone for LocalNetworkPermissionCompletion {
    fn clone(&self) -> Self {
        Self {
            inner: ffi::local_permission_completion_clone(self.inner.as_ref().unwrap()),
        }
    }
}
pub(crate) struct RustLocalNetworkPermissionFactory {
    provider: Option<Arc<dyn LocalNetworkPermissionFactory>>,
    failed: AtomicBool,
}
impl Drop for RustLocalNetworkPermissionFactory {
    fn drop(&mut self) {
        let provider = self.provider.take();
        let _ = catch_unwind(|| drop(provider));
    }
}
impl RustLocalNetworkPermissionFactory {
    pub(crate) fn new(provider: Option<Arc<dyn LocalNetworkPermissionFactory>>) -> Box<Self> {
        Box::new(Self {
            provider,
            failed: AtomicBool::new(false),
        })
    }
}
pub(crate) struct RustLocalNetworkPermission {
    provider: Option<Arc<dyn LocalNetworkPermission>>,
    failed: AtomicBool,
}
impl Drop for RustLocalNetworkPermission {
    fn drop(&mut self) {
        let provider = self.provider.take();
        let _ = catch_unwind(|| drop(provider));
    }
}
pub(crate) fn local_permission_factory_present(
    factory: &RustLocalNetworkPermissionFactory,
) -> bool {
    factory.provider.is_some()
}
pub(crate) fn local_permission_create(
    factory: &RustLocalNetworkPermissionFactory,
) -> Box<RustLocalNetworkPermission> {
    let provider = if factory.failed.load(Ordering::Acquire) {
        None
    } else {
        factory.provider.as_ref().and_then(|provider| {
            match catch_unwind(AssertUnwindSafe(|| provider.create())) {
                Ok(product) => Some(product),
                Err(_) => {
                    factory.failed.store(true, Ordering::Release);
                    None
                }
            }
        })
    };
    Box::new(RustLocalNetworkPermission {
        provider,
        failed: AtomicBool::new(false),
    })
}
pub(crate) fn local_permission_should_request(
    permission: &RustLocalNetworkPermission,
    address: cxx::UniquePtr<ffi::NativeSocketAddress>,
) -> bool {
    if permission.failed.load(Ordering::Acquire) {
        return true;
    }
    let Some(provider) = &permission.provider else {
        return true;
    };
    let address = SocketAddress::from_native(address);
    match catch_unwind(AssertUnwindSafe(|| {
        provider.should_request_permission(&address)
    })) {
        Ok(request) => request,
        Err(_) => {
            permission.failed.store(true, Ordering::Release);
            true
        }
    }
}
pub(crate) fn local_permission_request(
    permission: &RustLocalNetworkPermission,
    address: cxx::UniquePtr<ffi::NativeSocketAddress>,
    completion: cxx::UniquePtr<ffi::NativeLocalNetworkPermissionCompletion>,
) {
    let completion = LocalNetworkPermissionCompletion { inner: completion };
    let fallback = completion.clone();
    let result = if permission.failed.load(Ordering::Acquire) {
        None
    } else {
        permission.provider.as_ref().map(|provider| {
            catch_unwind(AssertUnwindSafe(|| {
                provider.request_permission(SocketAddress::from_native(address), completion)
            }))
        })
    };
    match result {
        Some(Ok(())) => (),
        Some(Err(_)) => {
            permission.failed.store(true, Ordering::Release);
            fallback.deny();
        }
        None => {
            fallback.deny();
        }
    }
}
