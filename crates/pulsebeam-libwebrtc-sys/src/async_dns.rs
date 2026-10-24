//! Owned DNS factory/product interfaces with a live, callable result view.
use crate::{AddressFamily, SocketAddress, ffi, provider_panic::catch_provider as catch_unwind};
use std::{
    fmt,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

thread_local! { static DNS_LOANS: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) }; }
// Actual Rust provider-allocation loans, not SDK identity/owner lookups. Do not
// recursively lock the same non-Sync provider on this thread. Other threads
// still serialize on its mutex, preserving ordinary concurrent result queries.
struct DnsProviderLoan;
impl DnsProviderLoan {
    fn enter<T>(provider: &T) -> Option<Self> {
        let pointer = provider as *const T as usize;
        DNS_LOANS
            .try_with(|loans| {
                let mut loans = loans.borrow_mut();
                if loans.contains(&pointer) {
                    None
                } else {
                    loans.push(pointer);
                    Some(Self)
                }
            })
            .ok()
            .flatten()
    }
}
impl Drop for DnsProviderLoan {
    fn drop(&mut self) {
        let _ = DNS_LOANS.try_with(|loans| {
            loans.borrow_mut().pop();
        });
    }
}

/// Callable native DNS result. Preserve the source hostname and port in each
/// returned address, replacing only its resolved IP, and honor the requested
/// family. Errors are native signed resolver codes, not binding error enums.
/// Native result references retain this callable resource until product teardown.
pub trait AsyncDnsResolverResult: Send + 'static {
    fn resolved_address(&self, family: AddressFamily) -> Option<SocketAddress>;
    fn error(&self) -> i32;
}
/// Owned callable result resource. Clones retain the same live provider, never
/// a snapshot of its addresses or error. Mutable provider state may use interior
/// mutability; the bridge serializes calls without requiring provider Sync.
#[derive(Clone)]
pub struct DnsResolverResult(Arc<ResultInner>);
struct ResultInner {
    provider: Mutex<Box<dyn AsyncDnsResolverResult>>,
    failed: AtomicBool,
}
impl DnsResolverResult {
    pub fn from_provider(provider: impl AsyncDnsResolverResult) -> Self {
        Self(Arc::new(ResultInner {
            provider: Mutex::new(Box::new(provider)),
            failed: AtomicBool::new(false),
        }))
    }
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
    fn with<T>(&self, action: impl FnOnce(&dyn AsyncDnsResolverResult) -> T) -> Option<T> {
        if self.has_failed() {
            return None;
        }
        let Some(_loan) = DnsProviderLoan::enter(self.0.as_ref()) else {
            self.0.failed.store(true, Ordering::Release);
            return None;
        };
        let Ok(provider) = self.0.provider.lock() else {
            self.0.failed.store(true, Ordering::Release);
            return None;
        };
        if self.has_failed() {
            return None;
        }
        match catch_unwind(AssertUnwindSafe(|| action(provider.as_ref()))) {
            Ok(value) => Some(value),
            Err(_) => {
                self.0.failed.store(true, Ordering::Release);
                None
            }
        }
    }
    pub fn resolved_address(&self, family: AddressFamily) -> Option<SocketAddress> {
        self.with(|provider| provider.resolved_address(family))
            .flatten()
    }
    pub fn error(&self) -> i32 {
        self.with(|provider| provider.error()).unwrap_or(-1)
    }
}
/// Owned single-query resolver. Entry, result queries and native destruction
/// share the caller's native sequence. Retained tickets may cross Rust threads.
/// Follow the native single-query lifecycle; safe supersession in this adapter
/// does not establish repeat-Start support in stock native implementations.
pub trait AsyncDnsResolver: Send + 'static {
    fn start(&mut self, address: SocketAddress, completion: DnsResolutionCompletion) {
        let family = address.family();
        self.start_with_family(address, family, completion);
    }
    fn start_with_family(
        &mut self,
        address: SocketAddress,
        family: AddressFamily,
        completion: DnsResolutionCompletion,
    );
    fn result(&self) -> DnsResolverResult;
}
/// All three native factory operations are executable overrides. A factory
/// must return an owned product, including on failure; SDK consumers dereference
/// products without a null check. Immediate-start operations must start before
/// returning. Their defaults use create followed by the matching start overload.
pub trait AsyncDnsResolverFactoryProvider: Send + 'static {
    fn create(&self) -> Box<dyn AsyncDnsResolver>;
    fn create_and_resolve(
        &self,
        address: SocketAddress,
        completion: DnsResolutionCompletion,
    ) -> Box<dyn AsyncDnsResolver> {
        let mut resolver = self.create();
        resolver.start(address, completion);
        resolver
    }
    fn create_and_resolve_with_family(
        &self,
        address: SocketAddress,
        family: AddressFamily,
        completion: DnsResolutionCompletion,
    ) -> Box<dyn AsyncDnsResolver> {
        let mut resolver = self.create();
        resolver.start_with_family(address, family, completion);
        resolver
    }
}
#[derive(Clone)]
pub struct AsyncDnsResolverFactory(Arc<FactoryInner>);
struct FactoryInner {
    provider: Mutex<Box<dyn AsyncDnsResolverFactoryProvider>>,
    failed: Arc<AtomicBool>,
}
impl AsyncDnsResolverFactory {
    pub fn from_provider(provider: impl AsyncDnsResolverFactoryProvider) -> Self {
        Self(Arc::new(FactoryInner {
            provider: Mutex::new(Box::new(provider)),
            failed: Arc::new(AtomicBool::new(false)),
        }))
    }
    /// Shared fail-closed diagnostic for factory and product method panics.
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
    fn with<T>(&self, action: impl FnOnce(&dyn AsyncDnsResolverFactoryProvider) -> T) -> Option<T> {
        if self.has_failed() {
            return None;
        }
        let Some(_loan) = DnsProviderLoan::enter(self.0.as_ref()) else {
            self.0.failed.store(true, Ordering::Release);
            return None;
        };
        let Ok(provider) = self.0.provider.lock() else {
            self.0.failed.store(true, Ordering::Release);
            return None;
        };
        if self.has_failed() {
            return None;
        }
        match catch_unwind(AssertUnwindSafe(|| action(provider.as_ref()))) {
            Ok(value) => Some(value),
            Err(_) => {
                self.0.failed.store(true, Ordering::Release);
                None
            }
        }
    }
}

/// Move-only notification ticket. The provider updates its callable result
/// before completing. Delivery always posts to the exact requesting native
/// task queue, including non-Thread queues, and never runs inline. Product
/// destruction cancels pending notification, including already queued delivery.
/// Dropping an unfinished ticket discards native captures on that same queue.
pub struct DnsResolutionCompletion(Arc<CompletionInner>);
struct CompletionInner {
    native: cxx::UniquePtr<ffi::NativeDnsResolutionCompletion>,
}
// SAFETY: native admission/cancellation share a mutex. Product retirement nulls
// the borrowed queue while still on its sequence; no callbacks run in Rust.
unsafe impl Send for CompletionInner {}
unsafe impl Sync for CompletionInner {}
impl DnsResolutionCompletion {
    pub fn complete(self) -> Result<(), DnsResolutionCompletionError> {
        match self.0.complete() {
            0 => Ok(()),
            1 => Err(DnsResolutionCompletionError::Retired),
            2 => Err(DnsResolutionCompletionError::Superseded),
            3 => Err(DnsResolutionCompletionError::AlreadyCompleted),
            _ => Err(DnsResolutionCompletionError::NoDispatchSequence),
        }
    }
}
impl CompletionInner {
    fn complete(&self) -> u8 {
        ffi::dns_resolution_complete(self.native.as_ref().expect("owned DNS completion"))
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DnsResolutionCompletionError {
    Retired,
    Superseded,
    AlreadyCompleted,
    NoDispatchSequence,
}
impl fmt::Display for DnsResolutionCompletionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Retired => "the requesting DNS resolver has retired",
            Self::Superseded => "a newer DNS request superseded this completion",
            Self::AlreadyCompleted => "DNS resolution has already completed",
            Self::NoDispatchSequence => "the requesting sequence has no native dispatch queue",
        })
    }
}
impl std::error::Error for DnsResolutionCompletionError {}

struct FailedResolver;
impl AsyncDnsResolverResult for FailedResolver {
    fn resolved_address(&self, _: AddressFamily) -> Option<SocketAddress> {
        None
    }
    fn error(&self) -> i32 {
        -1
    }
}
impl AsyncDnsResolver for FailedResolver {
    fn start_with_family(
        &mut self,
        _: SocketAddress,
        _: AddressFamily,
        completion: DnsResolutionCompletion,
    ) {
        let _ = completion.complete();
    }
    fn result(&self) -> DnsResolverResult {
        DnsResolverResult::from_provider(FailedResolver)
    }
}
pub(crate) struct RustAsyncDnsFactory(pub(crate) Option<AsyncDnsResolverFactory>);
impl Drop for RustAsyncDnsFactory {
    fn drop(&mut self) {
        let factory = self.0.take();
        let _ = catch_unwind(AssertUnwindSafe(|| drop(factory)));
    }
}
pub(crate) struct RustAsyncDnsResolver {
    provider: Mutex<Option<Box<dyn AsyncDnsResolver>>>,
    failed: Arc<AtomicBool>,
    aggregate: Option<Arc<AtomicBool>>,
}
impl Drop for RustAsyncDnsResolver {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if catch_unwind(AssertUnwindSafe(|| drop(provider))).is_err() {
            self.fail();
        }
    }
}
impl RustAsyncDnsResolver {
    pub(crate) fn failure_latch(&self) -> Arc<AtomicBool> {
        self.failed.clone()
    }
    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        if let Some(aggregate) = &self.aggregate {
            aggregate.store(true, Ordering::Release);
        }
    }
    fn with<T>(&self, action: impl FnOnce(&mut dyn AsyncDnsResolver) -> T) -> Option<T> {
        if self.failed.load(Ordering::Acquire) {
            return None;
        }
        let Some(_loan) = DnsProviderLoan::enter(self) else {
            self.fail();
            return None;
        };
        let Ok(mut provider) = self.provider.lock() else {
            self.fail();
            return None;
        };
        if self.failed.load(Ordering::Acquire) {
            return None;
        }
        match catch_unwind(AssertUnwindSafe(|| {
            action(provider.as_deref_mut().expect("owned DNS product"))
        })) {
            Ok(value) => Some(value),
            Err(_) => {
                self.fail();
                None
            }
        }
    }
}
pub(crate) fn async_dns_source_failed(source: &RustAsyncDnsResolver) -> bool {
    source.failed.load(Ordering::Acquire)
}
pub(crate) fn async_dns_result_failed(source: &RustAsyncDnsResult) -> bool {
    source.failed.load(Ordering::Acquire)
        || source
            .result
            .as_ref()
            .is_none_or(DnsResolverResult::has_failed)
}
pub(crate) fn async_dns_factory_present(source: &RustAsyncDnsFactory) -> bool {
    source.0.is_some()
}
pub(crate) fn socket_factory_resolver(
    provider: Option<Box<dyn AsyncDnsResolver>>,
    aggregate: Arc<AtomicBool>,
) -> Box<RustAsyncDnsResolver> {
    // A factory diagnostic is not a per-product failure latch. A failed socket
    // or another installation must not poison this resolver's native results.
    Box::new(RustAsyncDnsResolver {
        provider: Mutex::new(Some(provider.unwrap_or_else(|| Box::new(FailedResolver)))),
        failed: Arc::new(AtomicBool::new(false)),
        aggregate: Some(aggregate),
    })
}
fn wrap(provider: Box<dyn AsyncDnsResolver>, failed: Arc<AtomicBool>) -> Box<RustAsyncDnsResolver> {
    Box::new(RustAsyncDnsResolver {
        provider: Mutex::new(Some(provider)),
        failed,
        aggregate: None,
    })
}
pub(crate) fn async_dns_factory_create(source: &RustAsyncDnsFactory) -> Box<RustAsyncDnsResolver> {
    let Some(factory) = &source.0 else {
        return wrap(Box::new(FailedResolver), Arc::new(AtomicBool::new(true)));
    };
    let result = factory.with(|provider| provider.create());
    let provider = result.unwrap_or_else(|| {
        factory.0.failed.store(true, Ordering::Release);
        Box::new(FailedResolver)
    });
    wrap(provider, factory.0.failed.clone())
}
pub(crate) fn async_dns_factory_create_and_resolve(
    source: &RustAsyncDnsFactory,
    address: cxx::UniquePtr<ffi::NativeSocketAddress>,
    has_family: bool,
    family: i32,
    native: cxx::UniquePtr<ffi::NativeDnsResolutionCompletion>,
) -> Box<RustAsyncDnsResolver> {
    let completion = Arc::new(CompletionInner { native });
    let Some(factory) = &source.0 else {
        completion.complete();
        return wrap(Box::new(FailedResolver), Arc::new(AtomicBool::new(true)));
    };
    let result = factory.with(|provider| {
        let address = SocketAddress::from_native(address);
        let ticket = DnsResolutionCompletion(completion.clone());
        if has_family {
            provider.create_and_resolve_with_family(
                address,
                AddressFamily::from_native_value(family),
                ticket,
            )
        } else {
            provider.create_and_resolve(address, ticket)
        }
    });
    let provider = result.unwrap_or_else(|| {
        factory.0.failed.store(true, Ordering::Release);
        completion.complete();
        Box::new(FailedResolver)
    });
    wrap(provider, factory.0.failed.clone())
}
pub(crate) fn async_dns_start(
    source: &RustAsyncDnsResolver,
    address: cxx::UniquePtr<ffi::NativeSocketAddress>,
    has_family: bool,
    family: i32,
    native: cxx::UniquePtr<ffi::NativeDnsResolutionCompletion>,
) {
    let completion = Arc::new(CompletionInner { native });
    if source
        .with(|provider| {
            let address = SocketAddress::from_native(address);
            let ticket = DnsResolutionCompletion(completion.clone());
            if has_family {
                provider.start_with_family(
                    address,
                    AddressFamily::from_native_value(family),
                    ticket,
                );
            } else {
                provider.start(address, ticket);
            }
        })
        .is_none()
    {
        completion.complete();
    }
}
pub(crate) struct RustAsyncDnsResult {
    result: Option<DnsResolverResult>,
    failed: Arc<AtomicBool>,
    aggregate: Option<Arc<AtomicBool>>,
}
impl RustAsyncDnsResult {
    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        if let Some(aggregate) = &self.aggregate {
            aggregate.store(true, Ordering::Release);
        }
    }
}
impl Drop for RustAsyncDnsResult {
    fn drop(&mut self) {
        let result = self.result.take();
        if catch_unwind(AssertUnwindSafe(|| drop(result))).is_err() {
            self.fail();
        }
    }
}
pub(crate) fn async_dns_result_view(source: &RustAsyncDnsResolver) -> Box<RustAsyncDnsResult> {
    let result = source
        .with(|provider| provider.result())
        .unwrap_or_else(|| DnsResolverResult::from_provider(FailedResolver));
    Box::new(RustAsyncDnsResult {
        result: Some(result),
        failed: source.failed.clone(),
        aggregate: source.aggregate.clone(),
    })
}
pub(crate) fn async_dns_result_identity(source: &RustAsyncDnsResult) -> usize {
    Arc::as_ptr(&source.result.as_ref().expect("owned result").0) as usize
}
pub(crate) fn async_dns_result_error(source: &RustAsyncDnsResult) -> i32 {
    if source.failed.load(Ordering::Acquire) {
        return -1;
    }
    let result = source.result.as_ref().expect("owned result");
    let error = result.error();
    if result.has_failed() {
        source.fail();
    }
    error
}
pub(crate) fn async_dns_result_address(
    source: &RustAsyncDnsResult,
    family: i32,
) -> cxx::UniquePtr<ffi::NativeSocketAddress> {
    if source.failed.load(Ordering::Acquire) {
        return cxx::UniquePtr::null();
    }
    let result = source.result.as_ref().expect("owned result");
    let address = result.resolved_address(AddressFamily::from_native_value(family));
    if result.has_failed() {
        source.fail();
    }
    match address {
        Some(address) => ffi::socket_address_clone(address.native()),
        None => cxx::UniquePtr::null(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, sync::atomic::AtomicI32};

    struct LiveResult(Arc<AtomicI32>);
    impl AsyncDnsResolverResult for LiveResult {
        fn resolved_address(&self, family: AddressFamily) -> Option<SocketAddress> {
            if family != AddressFamily::Ipv4 {
                return None;
            }
            let mut address = SocketAddress::from_host(b"live.example.invalid", 5301);
            address.set_resolved_ip(crate::IpAddress::from(std::net::Ipv4Addr::new(
                192, 0, 2, 1,
            )));
            Some(address)
        }
        fn error(&self) -> i32 {
            self.0.load(Ordering::SeqCst)
        }
    }
    struct Product {
        result: DnsResolverResult,
        calls: Cell<usize>,
    }
    impl AsyncDnsResolver for Product {
        fn start_with_family(
            &mut self,
            _: SocketAddress,
            _: AddressFamily,
            _: DnsResolutionCompletion,
        ) {
        }
        fn result(&self) -> DnsResolverResult {
            self.calls.set(self.calls.get() + 1);
            self.result.clone()
        }
    }

    #[test]
    fn result_views_share_identity_but_observe_live_native_values() {
        let error = Arc::new(AtomicI32::new(0));
        let source = wrap(
            Box::new(Product {
                result: DnsResolverResult::from_provider(LiveResult(error.clone())),
                calls: Cell::new(0),
            }),
            Arc::new(AtomicBool::new(false)),
        );
        let first = async_dns_result_view(&source);
        let second = async_dns_result_view(&source);
        assert_eq!(
            async_dns_result_identity(&first),
            async_dns_result_identity(&second)
        );
        assert_eq!(async_dns_result_error(&first), 0);
        for value in [-37, i32::MIN, i32::MAX] {
            error.store(value, Ordering::SeqCst);
            assert_eq!(async_dns_result_error(&first), value);
            assert_eq!(async_dns_result_error(&second), value);
        }
        let address = SocketAddress::from_native(async_dns_result_address(
            &first,
            AddressFamily::Ipv4.native_value(),
        ));
        assert_eq!(address.hostname(), b"live.example.invalid");
        assert_eq!(address.port(), 5301);
        assert_eq!(address.family(), AddressFamily::Ipv4);
        assert!(async_dns_result_address(&first, AddressFamily::Ipv6.native_value()).is_null());
        assert!(async_dns_result_address(&second, -97).is_null());
        drop(source);
        // The owned Rust callable resource remains valid even after its product
        // registration drops. Native borrowed references stay product-bound.
        error.store(-71, Ordering::SeqCst);
        assert_eq!(async_dns_result_error(&first), -71);
    }

    #[test]
    fn non_sync_factory_and_product_are_owned_and_factory_panics_fail_closed() {
        struct Factory(Cell<i32>);
        impl AsyncDnsResolverFactoryProvider for Factory {
            fn create(&self) -> Box<dyn AsyncDnsResolver> {
                self.0.set(self.0.get() + 1);
                Box::new(Product {
                    result: DnsResolverResult::from_provider(LiveResult(Arc::new(AtomicI32::new(
                        self.0.get(),
                    )))),
                    calls: Cell::new(0),
                })
            }
        }
        let factory = AsyncDnsResolverFactory::from_provider(Factory(Cell::new(0)));
        let source = RustAsyncDnsFactory(Some(factory.clone()));
        for expected in 1..=2 {
            let product = async_dns_factory_create(&source);
            assert_eq!(
                async_dns_result_error(&async_dns_result_view(&product)),
                expected
            );
        }
        assert!(!factory.has_failed());

        struct Panics;
        impl AsyncDnsResolverFactoryProvider for Panics {
            fn create(&self) -> Box<dyn AsyncDnsResolver> {
                panic!("intentional factory panic")
            }
        }
        let failed = AsyncDnsResolverFactory::from_provider(Panics);
        let source = RustAsyncDnsFactory(Some(failed.clone()));
        for _ in 0..2 {
            let product = async_dns_factory_create(&source);
            let view = async_dns_result_view(&product);
            assert_eq!(async_dns_result_error(&view), -1);
            assert!(async_dns_result_address(&view, AddressFamily::Ipv4.native_value()).is_null());
            assert!(failed.has_failed());
        }
    }

    #[test]
    fn native_result_view_drop_contains_provider_unwind() {
        struct PanicsOnDrop;
        impl AsyncDnsResolverResult for PanicsOnDrop {
            fn resolved_address(&self, _: AddressFamily) -> Option<SocketAddress> {
                None
            }
            fn error(&self) -> i32 {
                0
            }
        }
        impl Drop for PanicsOnDrop {
            fn drop(&mut self) {
                panic!("intentional result destruction panic")
            }
        }
        let failed = Arc::new(AtomicBool::new(false));
        let source = wrap(
            Box::new(Product {
                result: DnsResolverResult::from_provider(PanicsOnDrop),
                calls: Cell::new(0),
            }),
            failed.clone(),
        );
        let view = async_dns_result_view(&source);
        drop(source);
        assert!(catch_unwind(AssertUnwindSafe(|| drop(view))).is_ok());
        assert!(failed.load(Ordering::Acquire));
    }
}
