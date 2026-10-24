//! Caller mDNS products bound to the manager's existing native network role.
use crate::{IpAddress, NetworkManagerFactory, NetworkManagerSignals, ffi};
use std::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    rc::Rc,
};

/// Implements the native responder contract, including name reference counts.
/// The binding does not generate names or implement a replacement mDNS protocol.
/// Methods and final provider destruction run on the manager's network role.
pub trait MdnsResponderProvider: 'static {
    fn create_name(&self, address: IpAddress, completion: MdnsNameCompletion);
    fn remove_name(&self, address: IpAddress, completion: MdnsRemovalCompletion);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MdnsOperationError {
    Retired,
    ProviderFailed,
}
impl std::fmt::Display for MdnsOperationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Retired => "mDNS manager scope is retired",
            Self::ProviderFailed => "mDNS provider failed",
        })
    }
}
impl std::error::Error for MdnsOperationError {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MdnsCompletionError {
    Inactive,
    AddressMismatch,
}
impl std::fmt::Display for MdnsCompletionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Inactive => "mDNS completion is already submitted or retired",
            Self::AddressMismatch => "mDNS completion address differs from its request",
        })
    }
}
impl std::error::Error for MdnsCompletionError {}

/// An owning sequence-local native responder. Aliases remain inspectable after
/// scope retirement, but cannot start work. Create via NetworkManagerContext.
#[derive(Clone)]
pub struct MdnsResponder {
    pub(crate) native: cxx::SharedPtr<ffi::NativeMdnsResponder>,
    sequence: PhantomData<Rc<()>>,
}
impl MdnsResponder {
    pub(crate) fn from_native(native: cxx::SharedPtr<ffi::NativeMdnsResponder>) -> Self {
        assert!(!native.is_null());
        Self {
            native,
            sequence: PhantomData,
        }
    }
    pub fn is_same_responder(&self, other: &Self) -> bool {
        ffi::mdns_responder_same(
            self.native.as_ref().unwrap(),
            other.native.as_ref().unwrap(),
        )
    }
    pub fn is_retired(&self) -> bool {
        ffi::mdns_responder_retired(self.native.as_ref().unwrap())
    }
    /// Aggregate provider, public callback and capture-destruction failure.
    /// A failed public callback does not disable a healthy provider.
    pub fn has_failed(&self) -> bool {
        ffi::mdns_responder_failed(self.native.as_ref().unwrap())
    }
    /// Ok means the native request was admitted, not that a name was created.
    /// Name bytes and the callback address are copied without UTF-8 conversion.
    pub fn create_name(
        &self,
        address: impl Into<IpAddress>,
        callback: impl FnOnce(IpAddress, Vec<u8>) + 'static,
    ) -> Result<(), MdnsOperationError> {
        self.request(address.into(), false, Callback::Created(Box::new(callback)))
    }
    pub fn remove_name(
        &self,
        address: impl Into<IpAddress>,
        callback: impl FnOnce(bool) + 'static,
    ) -> Result<(), MdnsOperationError> {
        self.request(address.into(), true, Callback::Removed(Box::new(callback)))
    }
    fn request(
        &self,
        address: IpAddress,
        remove: bool,
        callback: Callback,
    ) -> Result<(), MdnsOperationError> {
        let _turn = crate::execution::controlled_owner_turn();
        match ffi::mdns_responder_request(
            self.native.as_ref().unwrap(),
            &address.bytes(),
            remove,
            Box::new(RustMdnsCallback {
                callback: RefCell::new(Some(callback)),
                failure: RefCell::new(None),
            }),
        ) {
            0 => Ok(()),
            1 => Err(MdnsOperationError::Retired),
            _ => Err(MdnsOperationError::ProviderFailed),
        }
    }
}

/// Thread-safe, single-submission completion. Delivery and cancellation are
/// posted to the existing native role. Retained aliases cannot revive retired
/// work or retain the SDK callback beyond manager retirement.
#[derive(Clone)]
pub struct MdnsNameCompletion {
    native: cxx::SharedPtr<ffi::NativeMdnsCompletion>,
}
#[derive(Clone)]
pub struct MdnsRemovalCompletion {
    native: cxx::SharedPtr<ffi::NativeMdnsCompletion>,
}
// The native request mutex serializes posting with scope retirement; callback
// invocation/destruction stays on the network role. These contain no Rust loan.
unsafe impl Send for MdnsNameCompletion {}
unsafe impl Sync for MdnsNameCompletion {}
unsafe impl Send for MdnsRemovalCompletion {}
unsafe impl Sync for MdnsRemovalCompletion {}
impl MdnsNameCompletion {
    pub fn complete(
        &self,
        address: impl Into<IpAddress>,
        name: impl AsRef<[u8]>,
    ) -> Result<(), MdnsCompletionError> {
        match ffi::mdns_complete(
            self.native.as_ref().unwrap(),
            &address.into().bytes(),
            name.as_ref(),
            false,
        ) {
            0 => Ok(()),
            2 => Err(MdnsCompletionError::AddressMismatch),
            _ => Err(MdnsCompletionError::Inactive),
        }
    }
    pub fn cancel(&self) -> bool {
        ffi::mdns_cancel(self.native.as_ref().unwrap())
    }
}
impl MdnsRemovalCompletion {
    pub fn complete(&self, removed: bool) -> bool {
        ffi::mdns_complete(self.native.as_ref().unwrap(), &[], &[], removed) == 0
    }
    pub fn cancel(&self) -> bool {
        ffi::mdns_cancel(self.native.as_ref().unwrap())
    }
}

struct Failure {
    failed: Cell<bool>,
    callbacks_failed: Cell<bool>,
    product_failed: Rc<Cell<bool>>,
    manager: NetworkManagerFactory,
    signals: NetworkManagerSignals,
}
impl Failure {
    fn record(&self) {
        self.failed.set(true);
        self.product_failed.set(true);
        self.manager.record_failure();
        self.signals.notify_error();
    }
}
struct Provider {
    product: Option<Box<dyn MdnsResponderProvider>>,
    failure: Rc<Failure>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        let value = self.product.take();
        if crate::provider_panic::catch_provider(|| drop(value)).is_err() {
            self.failure.record();
        }
    }
}
pub(crate) struct RustMdnsResponder {
    provider: RefCell<Option<Rc<Provider>>>,
    failure: Rc<Failure>,
}
impl RustMdnsResponder {
    pub(crate) fn new(
        provider: impl MdnsResponderProvider,
        manager: NetworkManagerFactory,
        signals: NetworkManagerSignals,
        product_failed: Rc<Cell<bool>>,
    ) -> Self {
        let failure = Rc::new(Failure {
            failed: Cell::new(false),
            callbacks_failed: Cell::new(false),
            product_failed,
            manager,
            signals,
        });
        Self {
            provider: RefCell::new(Some(Rc::new(Provider {
                product: Some(Box::new(provider)),
                failure: failure.clone(),
            }))),
            failure,
        }
    }
    fn call(&self, operation: impl FnOnce(&dyn MdnsResponderProvider)) -> bool {
        if self.failure.failed.get() {
            return false;
        }
        let provider = self.provider.borrow().clone();
        let Some(provider) = provider else {
            return false;
        };
        // A reentrant retirement may release the registration slot, but this
        // frame keeps the actual provider alive until its last shared loan ends.
        if crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| {
            operation(provider.product.as_ref().unwrap().as_ref())
        }))
        .is_err()
        {
            self.failure.record();
            false
        } else {
            true
        }
    }
}
pub(crate) fn mdns_source_failed(source: &RustMdnsResponder) -> bool {
    source.failure.failed.get()
}
pub(crate) fn mdns_source_any_failed(source: &RustMdnsResponder) -> bool {
    source.failure.failed.get() || source.failure.callbacks_failed.get()
}
pub(crate) fn mdns_source_retire(source: &RustMdnsResponder) {
    let provider = source.provider.borrow_mut().take();
    drop(provider);
}
pub(crate) fn mdns_source_create(
    source: &RustMdnsResponder,
    ip: Vec<u8>,
    completion: cxx::SharedPtr<ffi::NativeMdnsCompletion>,
) -> bool {
    source.call(|provider| {
        provider.create_name(
            IpAddress::from_bytes(ip),
            MdnsNameCompletion { native: completion },
        )
    })
}
pub(crate) fn mdns_source_remove(
    source: &RustMdnsResponder,
    ip: Vec<u8>,
    completion: cxx::SharedPtr<ffi::NativeMdnsCompletion>,
) -> bool {
    source.call(|provider| {
        provider.remove_name(
            IpAddress::from_bytes(ip),
            MdnsRemovalCompletion { native: completion },
        )
    })
}

enum Callback {
    Created(Box<dyn FnOnce(IpAddress, Vec<u8>)>),
    Removed(Box<dyn FnOnce(bool)>),
}
pub(crate) struct RustMdnsCallback {
    callback: RefCell<Option<Callback>>,
    failure: RefCell<Option<Rc<Failure>>>,
}
impl RustMdnsCallback {
    fn fail(&self) {
        if let Some(failure) = self.failure.borrow().as_ref() {
            failure.callbacks_failed.set(true);
            failure.manager.record_failure();
        }
    }
}
pub(crate) fn mdns_callback_bind(callback: &RustMdnsCallback, source: &RustMdnsResponder) {
    *callback.failure.borrow_mut() = Some(source.failure.clone());
}
impl Drop for RustMdnsCallback {
    fn drop(&mut self) {
        let value = self.callback.get_mut().take();
        if crate::provider_panic::catch_provider(|| drop(value)).is_err() {
            self.fail();
        }
    }
}
pub(crate) fn mdns_callback_fire(
    source: &RustMdnsCallback,
    ip: Vec<u8>,
    name: Vec<u8>,
    removed: bool,
) {
    let callback = source.callback.borrow_mut().take();
    if crate::provider_panic::catch_provider(std::panic::AssertUnwindSafe(|| match callback {
        Some(Callback::Created(callback)) => callback(IpAddress::from_bytes(ip), name),
        Some(Callback::Removed(callback)) => callback(removed),
        None => {}
    }))
    .is_err()
    {
        source.fail();
    }
}
