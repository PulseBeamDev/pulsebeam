use pulsebeam_webrtc_sys::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
#[path = "common/panic_payload.rs"]
mod panic_payload;

#[derive(Default)]
struct Observed {
    created: AtomicUsize,
    initialized: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
    queries: AtomicUsize,
    drops: AtomicUsize,
    threads: Mutex<Vec<std::thread::ThreadId>>,
    signals: Mutex<Option<NetworkManagerSignals>>,
}
impl Observed {
    fn role(&self) {
        self.threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
    }
}
struct Factory {
    observed: Arc<Observed>,
    reject: bool,
    panic_initialize: bool,
    panic_drop: bool,
}
struct Manager {
    observed: Arc<Observed>,
    panic_initialize: bool,
    panic_drop: bool,
}
impl NetworkManagerFactoryProvider for Factory {
    fn create(&self, context: NetworkManagerContext) -> Option<Box<dyn NetworkManager>> {
        self.observed.role();
        self.observed.created.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            context.field_trials().lookup("Manager-Provider"),
            "EnabledOwned"
        );
        assert_eq!(
            context
                .environment()
                .field_trials()
                .lookup("Manager-Provider"),
            "EnabledOwned"
        );
        let signals = context.signals();
        assert!(!signals.notify_networks_changed());
        assert!(!signals.notify_error());
        *self.observed.signals.lock().unwrap() = Some(signals);
        (!self.reject).then(|| {
            Box::new(Manager {
                observed: self.observed.clone(),
                panic_initialize: self.panic_initialize,
                panic_drop: self.panic_drop,
            }) as Box<dyn NetworkManager>
        })
    }
}
impl NetworkManager for Manager {
    fn initialize(&self) {
        self.observed.role();
        self.observed.initialized.fetch_add(1, Ordering::SeqCst);
        if self.panic_initialize {
            panic_payload::raise();
        }
    }
    fn start_updating(&self) {
        self.observed.role();
        assert_eq!(self.observed.initialized.load(Ordering::SeqCst), 1);
        self.observed.starts.fetch_add(1, Ordering::SeqCst);
        assert!(
            self.observed
                .signals
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .notify_networks_changed()
        );
    }
    fn stop_updating(&self) {
        self.observed.role();
        self.observed.stops.fetch_add(1, Ordering::SeqCst);
    }
    fn networks(&self) -> Vec<Network> {
        self.observed.role();
        self.observed.queries.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }
    fn any_address_networks(&self) -> Vec<Network> {
        self.networks()
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        self.observed.role();
        self.observed.drops.fetch_add(1, Ordering::SeqCst);
        if self.panic_drop {
            panic_payload::raise();
        }
    }
}
fn registration(
    observed: &Arc<Observed>,
    reject: bool,
    panic_initialize: bool,
    panic_drop: bool,
) -> NetworkManagerFactory {
    NetworkManagerFactory::from_provider(Factory {
        observed: observed.clone(),
        reject,
        panic_initialize,
        panic_drop,
    })
}
fn trials() -> FieldTrials {
    FieldTrials::parse("Manager-Provider/EnabledOwned/").unwrap()
}
fn finish(
    session: &mut ProductionSession,
    peer: SessionPeerId,
    operation: OperationId,
) -> Option<SessionDescription> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        while let Some(event) = session.try_peer_event(peer).unwrap() {
            if let SessionEvent::OperationComplete(done) = event {
                if done.operation_id == operation {
                    return done.result.unwrap();
                }
            }
        }
        assert!(Instant::now() < deadline, "operation did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn local_manager_is_initialized_without_start_and_retires_on_its_network_role() {
    let observed = Arc::new(Observed::default());
    let registration = registration(&observed, false, false, true);
    let factory = PeerConnectionFactory::builder()
        .environment(
            Environment::builder()
                .field_trials(trials())
                .build()
                .unwrap(),
        )
        .network_manager_factory(&registration)
        .build()
        .unwrap();
    assert_eq!(observed.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(observed.starts.load(Ordering::SeqCst), 0);
    drop(factory);
    assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
    assert!(
        registration.has_failed(),
        "final product panic is contained and observed"
    );
    let threads = observed.threads.lock().unwrap();
    assert!(threads.iter().all(|thread| *thread == threads[0]));
    assert_ne!(threads[0], std::thread::current().id());
    assert!(
        !observed
            .signals
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .notify_networks_changed()
    );
}
#[test]
fn actor_installs_manager_and_cancels_retained_cross_thread_sources_on_shutdown() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<NetworkManagerFactory>();
    send_sync::<NetworkManagerSignals>();
    let observed = Arc::new(Observed::default());
    let registration = registration(&observed, false, false, false);
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .field_trials(trials())
        .network_manager_factory(&registration)
        .build()
        .unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    let operation = session.create_offer(peer, false).unwrap();
    let offer = finish(&mut session, peer, operation).unwrap();
    let operation = session.set_local_description(peer, offer).unwrap();
    finish(&mut session, peer, operation);
    let deadline = Instant::now() + Duration::from_secs(15);
    while observed.queries.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "SDK did not enumerate custom manager"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let signals = observed.signals.lock().unwrap().as_ref().unwrap().clone();
    assert!(
        std::thread::spawn({
            let signals = signals.clone();
            move || signals.notify_networks_changed()
        })
        .join()
        .unwrap()
    );
    std::thread::spawn(move || {
        session.shutdown().unwrap();
    })
    .join()
    .unwrap();
    assert_eq!(
        observed.starts.load(Ordering::SeqCst),
        observed.stops.load(Ordering::SeqCst)
    );
    assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
    assert!(!signals.notify_networks_changed());
    assert!(!signals.notify_error());
    let threads = observed.threads.lock().unwrap();
    assert!(threads.iter().all(|thread| *thread == threads[0]));
    assert!(!registration.has_failed());
}
#[test]
fn rejection_and_initialize_panic_fail_build_without_stock_fallback() {
    for panic_initialize in [false, true] {
        let observed = Arc::new(Observed::default());
        let registration = registration(&observed, !panic_initialize, panic_initialize, false);
        assert!(
            ProductionSession::builder(ProductionSessionConfig::default())
                .field_trials(trials())
                .network_manager_factory(&registration)
                .build()
                .is_err()
        );
        assert_eq!(observed.created.load(Ordering::SeqCst), 1);
        assert_eq!(
            observed.drops.load(Ordering::SeqCst),
            usize::from(panic_initialize)
        );
        assert_eq!(registration.has_failed(), panic_initialize);
        assert_eq!(observed.starts.load(Ordering::SeqCst), 0);
        assert_eq!(observed.queries.load(Ordering::SeqCst), 0);
        assert!(
            !observed
                .signals
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .notify_networks_changed()
        );
    }
}
