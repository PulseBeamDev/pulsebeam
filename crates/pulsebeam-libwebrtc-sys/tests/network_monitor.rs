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
    panic_drop: bool,
    factory_drops: AtomicUsize,
    factory_drop_threads: Mutex<Vec<std::thread::ThreadId>>,
    monitor_drop_threads: Mutex<Vec<std::thread::ThreadId>>,
    created: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
    info: AtomicUsize,
    drops: AtomicUsize,
    changes: Mutex<Option<NetworkChangeNotifier>>,
    threads: Mutex<Vec<std::thread::ThreadId>>,
}
struct Factory(Arc<Observed>);
struct Monitor(Arc<Observed>);
impl Drop for Factory {
    fn drop(&mut self) {
        self.0.factory_drops.fetch_add(1, Ordering::SeqCst);
        self.0
            .factory_drop_threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        if self.0.panic_drop {
            panic_payload::raise();
        }
    }
}
impl NetworkMonitorFactory for Factory {
    fn create(
        &self,
        trials: FieldTrialsSnapshot,
        changes: NetworkChangeNotifier,
    ) -> Option<Box<dyn NetworkMonitor>> {
        assert_eq!(trials.lookup("Monitor-Provider"), "EnabledOwned");
        assert!(
            !changes.notify(),
            "notification before Start must be rejected"
        );
        self.0.created.fetch_add(1, Ordering::SeqCst);
        self.0
            .threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        *self.0.changes.lock().unwrap() = Some(changes);
        Some(Box::new(Monitor(self.0.clone())))
    }
}
impl NetworkMonitor for Monitor {
    fn start(&self) {
        self.0.starts.fetch_add(1, Ordering::SeqCst);
        self.0
            .threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        assert!(self.0.changes.lock().unwrap().as_ref().unwrap().notify());
    }
    fn stop(&self) {
        self.0.stops.fetch_add(1, Ordering::SeqCst);
        self.0
            .threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        assert!(!self.0.changes.lock().unwrap().as_ref().unwrap().notify());
        if self.0.panic_drop {
            panic_payload::raise();
        }
    }
    fn interface_info(&self, _name: &[u8]) -> NetworkInterfaceInfo {
        self.0.info.fetch_add(1, Ordering::SeqCst);
        self.0
            .threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        NetworkInterfaceInfo {
            adapter_type: NetworkAdapterType::Ethernet,
            underlying_type_for_vpn: NetworkAdapterType::Unknown,
            preference: NetworkPreference::NotPreferred,
            available: true,
            slice: NetworkSlice::UnifiedCommunications,
        }
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.0
            .monitor_drop_threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        self.0.drops.fetch_add(1, Ordering::SeqCst);
        if self.0.panic_drop {
            panic_payload::raise();
        }
    }
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
fn actor_monitor_owns_provider_and_retires_cross_thread_notifications() {
    run_monitor(false, false);
}
#[test]
fn actor_monitor_stop_and_factory_product_destruction_contain_hostile_payloads() {
    run_monitor(true, false);
}
#[test]
fn pinned_linux_sdk_monitor_option_does_not_disable_the_installed_monitor() {
    // SetOptions stores this field, but the pinned Linux C++ path never reads
    // it. Exercise real BasicNetworkManager callbacks rather than an option echo.
    run_monitor(false, true);
}
fn run_monitor(panic_drop: bool, disable_network_monitor: bool) {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<NetworkChangeNotifier>();
    let observed = Arc::new(Observed {
        panic_drop,
        ..Observed::default()
    });
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .field_trials(FieldTrials::parse("Monitor-Provider/EnabledOwned/").unwrap())
        .network_monitor_factory(Factory(observed.clone()))
        .build()
        .unwrap();
    session
        .set_factory_options(FactoryOptions {
            disable_network_monitor,
            ..Default::default()
        })
        .unwrap();
    let observed_actor = observed.clone();
    let session = std::thread::spawn(move || {
        let mut session = session;
        let peer = session.create_peer(PeerConfiguration::default()).unwrap();
        let operation = session.create_offer(peer, false).unwrap();
        let offer = finish(&mut session, peer, operation).unwrap();
        let operation = session.set_local_description(peer, offer).unwrap();
        finish(&mut session, peer, operation);
        let deadline = Instant::now() + Duration::from_secs(15);
        while observed_actor.starts.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "native network monitor was not started"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        session
    })
    .join()
    .unwrap();
    assert_eq!(observed.created.load(Ordering::SeqCst), 1);
    let changes = observed.changes.lock().unwrap().as_ref().unwrap().clone();
    assert!(
        std::thread::spawn({
            let changes = changes.clone();
            move || changes.notify()
        })
        .join()
        .unwrap()
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while observed.info.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "interface metadata not requested"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let close_thread = std::thread::spawn(move || {
        let mut session = session;
        session.shutdown().unwrap();
        drop(session);
        std::thread::current().id()
    })
    .join()
    .unwrap();
    assert_eq!(observed.stops.load(Ordering::SeqCst), 1);
    assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
    assert_eq!(observed.factory_drops.load(Ordering::SeqCst), 1);
    assert!(!changes.notify());
    std::thread::spawn(move || assert!(!changes.notify()))
        .join()
        .unwrap();
    let threads = observed.threads.lock().unwrap();
    assert!(
        threads.iter().all(|thread| *thread == threads[0]),
        "creation/start/metadata/stop must share the native network sequence"
    );
    let monitor_drop_threads = observed.monitor_drop_threads.lock().unwrap();
    let factory_drop_threads = observed.factory_drop_threads.lock().unwrap();
    assert_eq!(
        monitor_drop_threads.as_slice(),
        &[threads[0]],
        "the monitor adapter retires its product on its network creation sequence"
    );
    assert_ne!(
        monitor_drop_threads[0], close_thread,
        "migration must not move native-owned product destruction to the actor caller"
    );
    assert_ne!(
        factory_drop_threads[0], close_thread,
        "the native manager retains the factory registration through its own teardown"
    );
}
