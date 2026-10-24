//! Sequence-local ownership of actual SDK legacy DNS products and result views.
use crate::{AddressFamily, PacketSocketEventError, SocketAddress, ffi};
use std::{
    cell::Cell,
    fmt,
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DnsResolverOperationError {
    Retired,
    WrongSequence,
    InProgress,
    ProviderFailed,
    AlreadyStarted,
}
impl fmt::Display for DnsResolverOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Retired => "DNS resolver has retired",
            Self::WrongSequence => "DNS operation requires its actual creation role",
            Self::InProgress => "DNS product or result is already being called",
            Self::ProviderFailed => "DNS provider has failed",
            Self::AlreadyStarted => "DNS resolver encapsulates one name query",
        })
    }
}
impl std::error::Error for DnsResolverOperationError {}
fn result(status: u8) -> Result<(), DnsResolverOperationError> {
    match status {
        0 => Ok(()),
        1 => Err(DnsResolverOperationError::Retired),
        3 => Err(DnsResolverOperationError::WrongSequence),
        4 => Err(DnsResolverOperationError::InProgress),
        13 => Err(DnsResolverOperationError::AlreadyStarted),
        _ => Err(DnsResolverOperationError::ProviderFailed),
    }
}
/// Owns one native resolver and actual environment/driver or production runtime.
/// Drop cancels pending completion and retires its role-local provider. Returned
/// callable result owners remain independent; neither contexts nor query metadata
/// own this resolver or revive it after retirement.
pub struct OwnedDnsResolver {
    native: Option<cxx::UniquePtr<ffi::NativeOwnedDnsResolver>>,
    context: DnsResolverContext,
}
impl Drop for OwnedDnsResolver {
    fn drop(&mut self) {
        let _turn = crate::execution::controlled_owner_turn();
        drop(self.native.take());
    }
}
impl std::ops::Deref for OwnedDnsResolver {
    type Target = DnsResolverContext;
    fn deref(&self) -> &Self::Target {
        &self.context
    }
}
impl OwnedDnsResolver {
    pub(crate) fn new(
        factory: crate::PacketSocketFactory,
        role: crate::owned_packet_socket::RustSocketRole,
        source: Box<crate::RustAsyncDnsResolver>,
    ) -> Result<Self, PacketSocketEventError> {
        let failed = source.failure_latch();
        let native = ffi::owned_dns_new(source, Box::new(role));
        if native.is_null() {
            return Err(PacketSocketEventError::WrongSequence);
        }
        let context = DnsResolverContext {
            native: ffi::owned_dns_context(native.as_ref().unwrap()),
            factory,
            failed,
            sequence: PhantomData,
        };
        Ok(Self {
            native: Some(native),
            context,
        })
    }
    pub fn context(&self) -> &DnsResolverContext {
        &self.context
    }
}
#[derive(Clone)]
pub struct DnsResolverContext {
    native: cxx::SharedPtr<ffi::NativeDnsResolverContext>,
    factory: crate::PacketSocketFactory,
    failed: Arc<AtomicBool>,
    sequence: PhantomData<Rc<()>>,
}
impl DnsResolverContext {
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    /// Both Start variants are dispatched to the actual SDK virtual. Retain the
    /// returned owning callback registration until notification or cancellation.
    /// Start may be called once; it never pumps or delivers completion inline.
    pub fn start(
        &self,
        address: &SocketAddress,
        callback: impl FnOnce() + 'static,
    ) -> Result<DnsResolverQuery, DnsResolverOperationError> {
        self.start_impl(address, None, callback)
    }
    pub fn start_with_family(
        &self,
        address: &SocketAddress,
        family: AddressFamily,
        callback: impl FnOnce() + 'static,
    ) -> Result<DnsResolverQuery, DnsResolverOperationError> {
        self.start_impl(address, Some(family), callback)
    }
    fn start_impl(
        &self,
        address: &SocketAddress,
        family: Option<AddressFamily>,
        callback: impl FnOnce() + 'static,
    ) -> Result<DnsResolverQuery, DnsResolverOperationError> {
        let _turn = crate::execution::controlled_owner_turn();
        let state = Rc::new(QueryState {
            active: Cell::new(true),
            delivered: Cell::new(false),
            failed: Cell::new(false),
        });
        let outcome = ffi::owned_dns_start(
            self.native.as_ref().unwrap(),
            address.native(),
            family.is_some(),
            family.map_or(0, AddressFamily::native_value),
            Box::new(RustDnsCallback {
                callback: Some(Box::new(callback)),
                state: state.clone(),
            }),
        );
        result(outcome.status)?;
        Ok(DnsResolverQuery {
            native: Some(outcome.query),
            state,
            sequence: PhantomData,
        })
    }
    pub fn result(&self) -> Result<OwnedDnsResolverResult, DnsResolverOperationError> {
        let _turn = crate::execution::controlled_owner_turn();
        let outcome = ffi::owned_dns_result(self.native.as_ref().unwrap());
        result(outcome.status)?;
        Ok(OwnedDnsResolverResult {
            native: Some(outcome.result),
            _factory: self.factory.clone(),
            sequence: PhantomData,
        })
    }
}
/// A callable owner of the actual native result object, not copied addresses or
/// a borrowed reference. It retains the actual role after resolver destruction.
pub struct OwnedDnsResolverResult {
    native: Option<cxx::UniquePtr<ffi::NativeOwnedDnsResult>>,
    _factory: crate::PacketSocketFactory,
    sequence: PhantomData<Rc<()>>,
}
impl Drop for OwnedDnsResolverResult {
    fn drop(&mut self) {
        let _turn = crate::execution::controlled_owner_turn();
        drop(self.native.take());
    }
}
impl OwnedDnsResolverResult {
    pub fn resolved_address(
        &self,
        family: AddressFamily,
    ) -> Result<Option<SocketAddress>, DnsResolverOperationError> {
        let _turn = crate::execution::controlled_owner_turn();
        let output = ffi::owned_dns_result_call(
            self.native.as_ref().unwrap().as_ref().unwrap(),
            true,
            family.native_value(),
        );
        result(output.status)?;
        Ok((!output.address.is_null()).then(|| SocketAddress::from_native(output.address)))
    }
    pub fn error(&self) -> Result<i32, DnsResolverOperationError> {
        let _turn = crate::execution::controlled_owner_turn();
        let output =
            ffi::owned_dns_result_call(self.native.as_ref().unwrap().as_ref().unwrap(), false, 0);
        result(output.status)?;
        Ok(output.code)
    }
}
struct QueryState {
    active: Cell<bool>,
    delivered: Cell<bool>,
    failed: Cell<bool>,
}
/// Cancel/Drop removes only this query's pending notification. It does not
/// manufacture a DNS failure/success or stop provider-owned I/O. Completion and
/// callback-capture failures are separately observable on this role-local guard.
pub struct DnsResolverQuery {
    native: Option<cxx::UniquePtr<ffi::NativeDnsQuery>>,
    state: Rc<QueryState>,
    sequence: PhantomData<Rc<()>>,
}
impl DnsResolverQuery {
    pub fn cancel(&self) -> bool {
        let _turn = crate::execution::controlled_owner_turn();
        ffi::owned_dns_query_cancel(self.native.as_ref().unwrap().as_ref().unwrap())
    }
    pub fn is_pending(&self) -> bool {
        self.state.active.get()
    }
    pub fn was_delivered(&self) -> bool {
        self.state.delivered.get()
    }
    pub fn has_failed(&self) -> bool {
        self.state.failed.get()
    }
}
impl Drop for DnsResolverQuery {
    fn drop(&mut self) {
        let _turn = crate::execution::controlled_owner_turn();
        drop(self.native.take());
    }
}
pub(crate) struct RustDnsCallback {
    callback: Option<Box<dyn FnOnce()>>,
    state: Rc<QueryState>,
}
impl Drop for RustDnsCallback {
    fn drop(&mut self) {
        self.state.active.set(false);
        let capture = self.callback.take();
        if crate::provider_panic::catch_provider(|| drop(capture)).is_err() {
            self.state.failed.set(true);
        }
    }
}
#[allow(
    clippy::boxed_local,
    reason = "CXX transfers ownership through this fixed Box ABI."
)]
pub(crate) fn owned_dns_callback_fire(mut callback: Box<RustDnsCallback>) {
    let _turn = crate::execution::controlled_owner_turn();
    callback.state.active.set(false);
    callback.state.delivered.set(true);
    let body = callback.callback.take().unwrap();
    if crate::provider_panic::catch_provider(body).is_err() {
        callback.state.failed.set(true);
    }
}
