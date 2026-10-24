//! Actual SDK resolver/result dispatch, independent of ICE hostname policy.
use pulsebeam_webrtc_sys::*;
use std::{
    cell::Cell,
    net::Ipv4Addr,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI32, AtomicUsize, Ordering},
    },
    time::Duration,
};
#[path = "common/panic_payload.rs"]
mod panic_payload;

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct Observed {
    starts: Mutex<Vec<(bool, SocketAddress, AddressFamily)>>,
    pending: Mutex<Option<DnsResolutionCompletion>>,
    address: Mutex<Option<SocketAddress>>,
    error: AtomicI32,
    resolver_drops: AtomicUsize,
    result_drops: AtomicUsize,
}
struct Answer(Arc<Observed>);
impl AsyncDnsResolverResult for Answer {
    fn resolved_address(&self, family: AddressFamily) -> Option<SocketAddress> {
        if family != AddressFamily::Ipv4 {
            return None;
        }
        let mut address = self.0.address.lock().unwrap().clone()?;
        address.set_resolved_ip(Ipv4Addr::new(10, 83, 0, 2));
        Some(address)
    }
    fn error(&self) -> i32 {
        self.0.error.load(Ordering::SeqCst)
    }
}
impl Drop for Answer {
    fn drop(&mut self) {
        self.0.result_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Resolver {
    observed: Arc<Observed>,
    result: DnsResolverResult,
}
impl Resolver {
    fn begin(
        &self,
        explicit: bool,
        address: SocketAddress,
        family: AddressFamily,
        completion: DnsResolutionCompletion,
    ) {
        self.observed
            .starts
            .lock()
            .unwrap()
            .push((explicit, address.clone(), family));
        *self.observed.address.lock().unwrap() = Some(address);
        assert!(
            self.observed
                .pending
                .lock()
                .unwrap()
                .replace(completion)
                .is_none()
        );
    }
}
impl AsyncDnsResolver for Resolver {
    fn start(&mut self, address: SocketAddress, completion: DnsResolutionCompletion) {
        let family = address.family();
        self.begin(false, address, family, completion);
    }
    fn start_with_family(
        &mut self,
        address: SocketAddress,
        family: AddressFamily,
        completion: DnsResolutionCompletion,
    ) {
        self.begin(true, address, family, completion);
    }
    fn result(&self) -> DnsResolverResult {
        self.result.clone()
    }
}
impl Drop for Resolver {
    fn drop(&mut self) {
        self.observed.resolver_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Factory(Arc<Observed>);
impl PacketSocketFactoryImplementation for Factory {
    fn create_udp_socket(
        &self,
        _: PacketSocketContext,
        _: SocketAddress,
        _: u16,
        _: u16,
    ) -> Option<Box<dyn PacketSocket>> {
        None
    }
    fn create_server_tcp_socket(
        &self,
        _: ListenSocketContext,
        _: SocketAddress,
        _: u16,
        _: u16,
        _: PacketSocketFlags,
    ) -> Option<Box<dyn ListenSocket>> {
        None
    }
    fn create_client_tcp_socket(
        &self,
        _: PacketSocketContext,
        _: SocketAddress,
        _: SocketAddress,
        _: PacketSocketTcpOptions,
    ) -> Option<Box<dyn PacketSocket>> {
        None
    }
    fn create_client_udp_socket(
        &self,
        _: PacketSocketContext,
        _: SocketAddress,
        _: SocketAddress,
        _: u16,
        _: u16,
        _: PacketSocketTcpOptions,
    ) -> Option<Box<dyn PacketSocket>> {
        None
    }
    fn create_async_dns_resolver(&self, environment: Environment) -> Box<dyn AsyncDnsResolver> {
        assert_eq!(environment.field_trials().lookup("Owned-DNS"), "Selected");
        Box::new(Resolver {
            observed: self.0.clone(),
            result: DnsResolverResult::from_provider(Answer(self.0.clone())),
        })
    }
    fn is_controlled(&self) -> bool {
        true
    }
}
struct Fixture {
    driver: ControlledPeerDriver,
    environment: Environment,
    factory: PacketSocketFactory,
    observed: Arc<Observed>,
}
impl Fixture {
    fn new() -> Self {
        let clock = ManualClock::new(Duration::from_secs(10)).unwrap();
        let driver = ControlledPeerDriver::new(&clock).unwrap();
        let queues = TaskQueueFactory::cooperative(&clock).unwrap();
        let environment = Environment::builder()
            .task_queue_factory(&queues)
            .field_trials(FieldTrials::parse("Owned-DNS/Selected/").unwrap())
            .build()
            .unwrap();
        let observed = Arc::new(Observed::default());
        let factory = PacketSocketFactory::from_provider(Factory(observed.clone()));
        Self {
            driver,
            environment,
            factory,
            observed,
        }
    }
    fn resolver(&self) -> OwnedDnsResolver {
        self.factory
            .create_async_dns_resolver_controlled(&self.driver, &self.environment)
            .unwrap()
    }
    fn complete(&self) -> Result<(), DnsResolutionCompletionError> {
        let completion = self.observed.pending.lock().unwrap().take().unwrap();
        std::thread::spawn(move || completion.complete())
            .join()
            .unwrap()
    }
    fn pump(&self) {
        assert!(!self.driver.pump(128).blocked);
    }
}

#[test]
fn both_native_start_overloads_post_to_origin_and_results_outlive_resolver() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for family in [None, Some(AddressFamily::Other(-83))] {
        let fixture = Fixture::new();
        let resolver = fixture.resolver();
        let context = resolver.context().clone();
        let address = SocketAddress::from_host(b"retained.example.invalid", 3478);
        let calls = Rc::new(Cell::new(0));
        let callback_calls = calls.clone();
        let callback_context = context.clone();
        let callback = move || {
            assert_eq!(callback_context.result().unwrap().error().unwrap(), -37);
            callback_calls.set(callback_calls.get() + 1);
        };
        let query = match family {
            None => resolver.start(&address, callback),
            Some(family) => resolver.start_with_family(&address, family, callback),
        }
        .unwrap();
        assert!(query.is_pending());
        assert_eq!(calls.get(), 0);
        assert!(matches!(
            resolver.start(&address, || {}),
            Err(DnsResolverOperationError::AlreadyStarted)
        ));
        assert_eq!(
            *fixture.observed.starts.lock().unwrap(),
            vec![(
                family.is_some(),
                address.clone(),
                family.unwrap_or(address.family())
            )]
        );
        fixture.observed.error.store(-37, Ordering::SeqCst);
        fixture.complete().unwrap();
        assert_eq!(
            calls.get(),
            0,
            "completion must not run on the submitting thread"
        );
        fixture.pump();
        assert_eq!(calls.get(), 1);
        assert!(!query.is_pending());
        assert!(query.was_delivered());
        assert!(!query.has_failed());
        assert!(!query.cancel());

        let result = resolver.result().unwrap();
        drop(resolver);
        assert_eq!(fixture.observed.resolver_drops.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.observed.result_drops.load(Ordering::SeqCst), 0);
        assert!(matches!(
            context.result(),
            Err(DnsResolverOperationError::Retired)
        ));
        assert!(matches!(
            context.start(&address, || {}),
            Err(DnsResolverOperationError::Retired)
        ));
        for code in [i32::MIN, -1, 0, i32::MAX] {
            fixture.observed.error.store(code, Ordering::SeqCst);
            assert_eq!(
                result.error().unwrap(),
                code,
                "native result owner must remain callable, not a snapshot"
            );
        }
        assert!(
            result
                .resolved_address(AddressFamily::Ipv6)
                .unwrap()
                .is_none()
        );
        assert!(
            result
                .resolved_address(AddressFamily::Other(-83))
                .unwrap()
                .is_none()
        );
        let resolved = result
            .resolved_address(AddressFamily::Ipv4)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.hostname(), address.hostname());
        assert_eq!(resolved.port(), address.port());
        assert_eq!(
            resolved.ip_address(),
            IpAddress::from(Ipv4Addr::new(10, 83, 0, 2))
        );
        drop(result);
        assert_eq!(fixture.observed.result_drops.load(Ordering::SeqCst), 1);
        assert!(!context.has_failed());
        assert!(!fixture.factory.has_failed());
    }
}

#[test]
fn cancellation_and_retirement_suppress_even_already_queued_completion() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for retire in [false, true] {
        let fixture = Fixture::new();
        let resolver = fixture.resolver();
        let context = resolver.context().clone();
        let calls = Rc::new(Cell::new(0));
        let callback_calls = calls.clone();
        let query = resolver
            .start(&SocketAddress::from_host(b"cancel.invalid", 9), move || {
                callback_calls.set(callback_calls.get() + 1);
            })
            .unwrap();
        fixture.complete().unwrap();
        if retire {
            drop(resolver);
        } else {
            assert!(query.cancel());
            assert!(!query.cancel());
        }
        assert!(!query.is_pending());
        fixture.pump();
        assert_eq!(calls.get(), 0);
        assert!(!query.was_delivered());
        assert!(!query.has_failed());
        assert!(!context.has_failed());
        assert!(!fixture.factory.has_failed());
    }
}

#[test]
fn late_cross_thread_completion_observes_actual_retirement() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::new();
    let resolver = fixture.resolver();
    let query = resolver
        .start(&SocketAddress::from_host(b"late.invalid", 53), || {
            panic!("retired query delivered")
        })
        .unwrap();
    drop(resolver);
    assert_eq!(
        fixture.complete(),
        Err(DnsResolutionCompletionError::Retired)
    );
    fixture.pump();
    assert!(!query.is_pending());
    assert!(!query.was_delivered());
    assert!(!query.cancel());
}

#[test]
fn callback_and_capture_panics_latch_on_query_without_disabling_recipe() {
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic_payload::raise();
        }
    }
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for cancel in [false, true] {
        let fixture = Fixture::new();
        let resolver = fixture.resolver();
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = Capture(drops.clone());
        let query = resolver
            .start(&SocketAddress::from_host(b"panic.invalid", 53), move || {
                // Capture Drop alone raises one hostile payload; do not double panic
                // by raising a second payload while unwinding this closure.
                drop(capture);
            })
            .unwrap();
        if cancel {
            assert!(query.cancel());
            assert_eq!(
                fixture.complete(),
                Err(DnsResolutionCompletionError::AlreadyCompleted)
            );
        } else {
            fixture.complete().unwrap();
        }
        fixture.pump();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(query.has_failed());
        assert_eq!(query.was_delivered(), !cancel);
        assert!(!resolver.has_failed());
        assert!(!fixture.factory.has_failed());
        drop(fixture.resolver());
        assert_eq!(fixture.observed.resolver_drops.load(Ordering::SeqCst), 1);
    }
}
