//! Actual SDK responder callbacks and manager-scope retirement, not a fake
//! mDNS protocol or proof of candidate obfuscation on a physical interface.
use pulsebeam_webrtc_sys::*;
use std::{
    cell::RefCell,
    net::Ipv4Addr,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
#[path = "common/panic_payload.rs"]
mod panic_payload;

static TEST_LOCK: Mutex<()> = Mutex::new(());
thread_local! {
    // Test-only same-role handoff of the actual product created by the native
    // manager callback. No ID lookup, fabricated SDK identity or cross-role loan.
    static CREATED: RefCell<Option<MdnsResponder>> = const { RefCell::new(None) };
}
#[derive(Default)]
struct Observed {
    names: Mutex<Vec<(IpAddress, MdnsNameCompletion)>>,
    removals: Mutex<Vec<(IpAddress, MdnsRemovalCompletion)>>,
    drops: AtomicUsize,
}
struct Responder(Arc<Observed>);
impl MdnsResponderProvider for Responder {
    fn create_name(&self, address: IpAddress, completion: MdnsNameCompletion) {
        self.0.names.lock().unwrap().push((address, completion));
    }
    fn remove_name(&self, address: IpAddress, completion: MdnsRemovalCompletion) {
        self.0.removals.lock().unwrap().push((address, completion));
    }
}
impl Drop for Responder {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Managers(Arc<Observed>);
struct Manager(MdnsResponder);
impl NetworkManagerFactoryProvider for Managers {
    fn create(&self, context: NetworkManagerContext) -> Option<Box<dyn NetworkManager>> {
        let responder = context.mdns_responder(Responder(self.0.clone())).unwrap();
        CREATED.with(|slot| assert!(slot.borrow_mut().replace(responder.clone()).is_none()));
        Some(Box::new(Manager(responder)))
    }
    fn is_controlled(&self) -> bool {
        true
    }
}
impl NetworkManager for Manager {
    fn start_updating(&self) {}
    fn stop_updating(&self) {}
    fn networks(&self) -> Vec<Network> {
        Vec::new()
    }
    fn any_address_networks(&self) -> Vec<Network> {
        Vec::new()
    }
    fn mdns_responder(&self) -> Option<MdnsResponder> {
        Some(self.0.clone())
    }
}
struct Fixture {
    factory: Option<PeerConnectionFactory>,
    responder: MdnsResponder,
    observed: Arc<Observed>,
    registration: NetworkManagerFactory,
    _endpoint: NetworkEndpoint,
    _network: ControlledSimulatedNetwork,
    world: ControlledWorld,
}
impl Fixture {
    fn new() -> Self {
        let world = ControlledWorld::acquire(0x6d, Duration::from_secs(10)).unwrap();
        let network = world.create_network().unwrap();
        let endpoint = network
            .register_endpoint(Ipv4Addr::new(10, 85, 0, 1).into())
            .unwrap();
        let observed = Arc::new(Observed::default());
        let registration = NetworkManagerFactory::from_provider(Managers(observed.clone()));
        let factory = world
            .peer_factory_builder()
            .unwrap()
            .network_manager_factory(&registration)
            .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
            .build()
            .unwrap();
        let responder = CREATED.with(|slot| {
            slot.borrow_mut()
                .take()
                .expect("SDK did not construct the selected manager")
        });
        Self {
            factory: Some(factory),
            responder,
            observed,
            registration,
            _endpoint: endpoint,
            _network: network,
            world,
        }
    }
    fn pump(&self) {
        assert!(!self.world.pump(256).blocked);
    }
    fn name(&self) -> (IpAddress, MdnsNameCompletion) {
        self.observed.names.lock().unwrap().pop().unwrap()
    }
    fn remove(&self) -> (IpAddress, MdnsRemovalCompletion) {
        self.observed.removals.lock().unwrap().pop().unwrap()
    }
}
fn address() -> IpAddress {
    Ipv4Addr::new(10, 85, 0, 1).into()
}

#[test]
fn native_callbacks_preserve_binary_names_and_retry_mismatched_completion() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::new();
    let responder = fixture.responder.clone();
    assert!(fixture.responder.is_same_responder(&responder));
    let names = Rc::new(RefCell::new(Vec::new()));
    let names_callback = names.clone();
    fixture
        .responder
        .create_name(address(), move |ip, bytes| {
            names_callback.borrow_mut().push((ip, bytes))
        })
        .unwrap();
    assert!(names.borrow().is_empty());
    let (requested, completion) = fixture.name();
    assert_eq!(requested, address());
    assert_eq!(
        completion.complete(Ipv4Addr::new(10, 85, 0, 2), b"wrong.local"),
        Err(MdnsCompletionError::AddressMismatch)
    );
    let alias = completion.clone();
    let bytes = vec![0, 255, 17, 0, b'.', b'l'];
    let submitted = bytes.clone();
    std::thread::spawn(move || completion.complete(address(), submitted))
        .join()
        .unwrap()
        .unwrap();
    assert_eq!(
        alias.complete(address(), b"second.local"),
        Err(MdnsCompletionError::Inactive)
    );
    assert!(!alias.cancel());
    assert!(
        names.borrow().is_empty(),
        "submission must not invoke role-local callbacks on a foreign thread"
    );
    fixture.pump();
    assert_eq!(*names.borrow(), vec![(address(), bytes)]);
    assert!(!fixture.responder.has_failed());

    for removed in [false, true] {
        let results = Rc::new(RefCell::new(Vec::new()));
        let callback_results = results.clone();
        responder
            .remove_name(address(), move |value| {
                callback_results.borrow_mut().push(value)
            })
            .unwrap();
        let (requested, completion) = fixture.remove();
        assert_eq!(requested, address());
        let alias = completion.clone();
        assert!(
            std::thread::spawn(move || completion.complete(removed))
                .join()
                .unwrap()
        );
        assert!(!alias.complete(!removed));
        assert!(results.borrow().is_empty());
        fixture.pump();
        assert_eq!(*results.borrow(), vec![removed]);
    }
    assert!(!fixture.registration.has_failed());
}

struct Capture(Arc<AtomicUsize>, bool);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        if self.1 {
            panic_payload::raise();
        }
    }
}

#[test]
fn completion_cancel_and_queued_delivery_retirement_release_actual_captures() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for retire in [false, true] {
        let mut fixture = Fixture::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = Capture(drops.clone(), false);
        fixture
            .responder
            .create_name(address(), move |_, _| {
                let _capture = capture;
                panic!("canceled or retired mDNS callback delivered");
            })
            .unwrap();
        let (_, completion) = fixture.name();
        if retire {
            completion.complete(address(), b"queued.local").unwrap();
            drop(fixture.factory.take());
            assert!(fixture.responder.is_retired());
            assert_eq!(fixture.observed.drops.load(Ordering::SeqCst), 1);
            assert_eq!(
                fixture.responder.create_name(address(), |_, _| {}),
                Err(MdnsOperationError::Retired)
            );
            assert_eq!(
                fixture.responder.remove_name(address(), |_| {}),
                Err(MdnsOperationError::Retired)
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        } else {
            assert!(completion.cancel());
            assert!(!completion.cancel());
            assert_eq!(
                drops.load(Ordering::SeqCst),
                0,
                "foreign-safe cancellation only posts role-local cleanup"
            );
        }
        assert_eq!(
            completion.complete(address(), b"late.local"),
            Err(MdnsCompletionError::Inactive)
        );
        fixture.pump();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(!fixture.responder.has_failed());
        assert!(!fixture.registration.has_failed());
    }
}

#[test]
fn callback_reentry_and_hostile_capture_drop_do_not_disable_healthy_provider() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let fixture = Fixture::new();
    let responder = fixture.responder.clone();
    let removed = Rc::new(RefCell::new(Vec::new()));
    let removed_callback = removed.clone();
    fixture
        .responder
        .create_name(address(), move |ip, _| {
            responder
                .remove_name(ip, move |value| removed_callback.borrow_mut().push(value))
                .unwrap();
        })
        .unwrap();
    let (_, created) = fixture.name();
    created.complete(address(), b"reentrant.local").unwrap();
    fixture.pump();
    let (_, removal) = fixture.remove();
    assert!(removal.complete(true));
    fixture.pump();
    assert_eq!(*removed.borrow(), vec![true]);

    for cancel in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = Capture(drops.clone(), true);
        fixture
            .responder
            .create_name(address(), move |_, _| drop(capture))
            .unwrap();
        let (_, completion) = fixture.name();
        if cancel {
            assert!(completion.cancel());
        } else {
            completion.complete(address(), b"hostile.local").unwrap();
        }
        fixture.pump();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            fixture.responder.has_failed(),
            "callback/capture failure remains observable"
        );
        // Aggregate diagnostics must not turn a callback failure into provider
        // quarantine. The next real native provider operation still runs.
        fixture.responder.create_name(address(), |_, _| {}).unwrap();
        let (_, healthy) = fixture.name();
        healthy.complete(address(), b"healthy.local").unwrap();
        fixture.pump();
    }
    assert!(fixture.registration.has_failed());
}
