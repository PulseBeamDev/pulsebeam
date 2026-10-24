//! Owning default-local-address providers used by native Network objects.
use crate::{AddressFamily, IpAddress, ffi};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub trait DefaultLocalAddressProvider: Send + Sync + 'static {
    fn default_local_address(&self, family: AddressFamily) -> Option<IpAddress>;
}
impl<T: DefaultLocalAddressProvider + ?Sized> DefaultLocalAddressProvider for Arc<T> {
    fn default_local_address(&self, family: AddressFamily) -> Option<IpAddress> {
        (**self).default_local_address(family)
    }
}

#[derive(Clone)]
pub struct DefaultLocalAddressProviderHandle {
    pub(crate) native: cxx::SharedPtr<ffi::NativeDefaultLocalAddressProvider>,
}
// SAFETY: native ownership is shared and immutable; all calls dispatch to a
// Send + Sync provider. The only adapter mutation is an atomic failure flag.
unsafe impl Send for DefaultLocalAddressProviderHandle {}
unsafe impl Sync for DefaultLocalAddressProviderHandle {}
impl DefaultLocalAddressProviderHandle {
    pub fn new(provider: impl DefaultLocalAddressProvider) -> Self {
        Self {
            native: ffi::default_address_provider_new(Box::new(RustDefaultLocalAddressProvider {
                provider: Some(Arc::new(provider)),
                failed: AtomicBool::new(false),
            })),
        }
    }
    pub fn default_local_address(&self, family: AddressFamily) -> Option<IpAddress> {
        let result = ffi::default_address_provider_get(
            self.native
                .as_ref()
                .expect("owning default address provider"),
            family.native_value(),
        );
        result.present.then(|| IpAddress::from_bytes(result.ip))
    }
    pub fn has_failed(&self) -> bool {
        ffi::default_address_provider_failed(
            self.native
                .as_ref()
                .expect("owning default address provider"),
        )
    }
}

pub(crate) struct RustDefaultLocalAddressProvider {
    provider: Option<Arc<dyn DefaultLocalAddressProvider>>,
    failed: AtomicBool,
}
impl Drop for RustDefaultLocalAddressProvider {
    fn drop(&mut self) {
        let provider = self.provider.take();
        let _ = crate::provider_panic::catch_provider(|| drop(provider));
    }
}
pub(crate) fn default_address_get(
    source: &RustDefaultLocalAddressProvider,
    family: i32,
) -> ffi::FfiDefaultLocalAddress {
    let value = if source.failed.load(Ordering::Acquire) {
        None
    } else {
        match crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
            source
                .provider
                .as_ref()
                .expect("retained provider")
                .default_local_address(AddressFamily::from_native_value(family))
        })) {
            Ok(value) => value,
            Err(_) => {
                source.failed.store(true, Ordering::Release);
                None
            }
        }
    };
    ffi::FfiDefaultLocalAddress {
        present: value.is_some(),
        ip: value.unwrap_or_default().bytes(),
    }
}
pub(crate) fn default_address_failed(source: &RustDefaultLocalAddressProvider) -> bool {
    source.failed.load(Ordering::Acquire)
}
