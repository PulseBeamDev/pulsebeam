#[path = "support/isolated.rs"]
mod isolated;

use pulsebeam_webrtc_sys::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

static SERIAL: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct Observed {
    requests: Mutex<Vec<(Vec<u8>, QueuePriority)>>,
    tasks: Mutex<Vec<TaskQueueTask>>,
    deleted: AtomicUsize,
    dropped: AtomicUsize,
    fail: AtomicBool,
}
struct Provider(Arc<Observed>);
impl TaskQueueFactoryProvider for Provider {
    fn create_task_queue(
        &self,
        name: &[u8],
        priority: QueuePriority,
    ) -> Box<dyn TaskQueueProvider> {
        assert!(
            !self.0.fail.load(Ordering::SeqCst),
            "selected factory failed"
        );
        self.0
            .requests
            .lock()
            .unwrap()
            .push((name.to_vec(), priority));
        Box::new(Queue(self.0.clone(), name.to_vec()))
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Queue(Arc<Observed>, Vec<u8>);
impl TaskQueueProvider for Queue {
    fn queue_name(&self) -> Vec<u8> {
        self.1.clone()
    }
    fn post_task(&self, task: TaskQueueTask, _: TaskQueueLocation) {
        self.0.tasks.lock().unwrap().push(task);
    }
    fn post_delayed_task(
        &self,
        task: TaskQueueTask,
        _: TimeDelta,
        _: TaskDelayPrecision,
        location: TaskQueueLocation,
    ) {
        self.post_task(task, location);
    }
    fn delete(&self) {
        self.0.deleted.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn utility_and_products_retain_native_provider_and_rust_only_resources() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let lease = RandomnessLease::acquire(0x7400).unwrap();
    let observed = Arc::new(Observed::default());
    let source = TaskQueueFactory::from_provider(Provider(observed.clone())).unwrap();
    let mut environment = Environment::builder()
        .task_queue_factory(&source)
        .randomness(&lease)
        .build()
        .unwrap();
    let utility = environment.task_queue_factory().unwrap();
    assert!(utility.is_provider_backed());
    assert!(!utility.is_cooperative());
    drop(source);
    drop(lease);
    environment.close();
    assert!(matches!(
        environment.task_queue_factory(),
        Err(BuildEnvironmentError::Closed)
    ));
    let mut queue = utility
        .create_queue_bytes(b"owned\0\xff", QueuePriority::Audio)
        .unwrap();
    assert_eq!(queue.name_bytes(), b"owned\0\xff");
    assert_eq!(
        *observed.requests.lock().unwrap(),
        vec![(b"owned\0\xff".to_vec(), QueuePriority::Audio)]
    );
    drop(utility);
    let completed = Arc::new(AtomicBool::new(false));
    let result = completed.clone();
    assert!(queue.post(move || result.store(true, Ordering::SeqCst)));
    assert!(!completed.load(Ordering::SeqCst));
    let task = observed.tasks.lock().unwrap().pop().unwrap();
    assert!(task.execute());
    assert!(completed.load(Ordering::SeqCst));
    assert_eq!(observed.dropped.load(Ordering::SeqCst), 0);
    assert!(matches!(
        RandomnessLease::acquire(0x7401),
        Err(RandomnessLeaseError::AlreadyAcquired)
    ));
    queue.close();
    assert_eq!(observed.deleted.load(Ordering::SeqCst), 1);
    drop(queue);
    assert_eq!(observed.dropped.load(Ordering::SeqCst), 1);
    drop(RandomnessLease::acquire(0x7401).unwrap());
}

#[test]
fn provider_utility_reinstalls_exact_names_priorities_and_failure_diagnostics() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let observed = Arc::new(Observed::default());
    let source = TaskQueueFactory::from_provider(Provider(observed.clone())).unwrap();
    let mut environment = Environment::builder()
        .task_queue_factory(&source)
        .build()
        .unwrap();
    let utility = environment.task_queue_factory().unwrap();
    let mut rebound = Environment::builder()
        .task_queue_factory(&utility)
        .build()
        .unwrap();
    let second = rebound.task_queue_factory().unwrap();
    environment.close();
    rebound.close();
    drop(utility);
    drop(source);
    for priority in [
        QueuePriority::Normal,
        QueuePriority::High,
        QueuePriority::Video,
        QueuePriority::Audio,
        QueuePriority::Low,
    ] {
        for name in [&[][..], &b"native\0\xff"[..]] {
            let mut queue = second.create_queue_bytes(name, priority).unwrap();
            assert_eq!(queue.name_bytes(), name);
            assert_eq!(
                observed.requests.lock().unwrap().last(),
                Some(&(name.to_vec(), priority))
            );
            queue.close();
        }
    }
    assert!(!second.has_failed());
    observed.fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        second.create_queue("failure", QueuePriority::Normal),
        Err(BuildEnvironmentError::NativeConstructionFailed)
    ));
    assert!(second.has_failed());
    drop(second);
    assert_eq!(observed.dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn cooperative_utility_keeps_original_creator_scheduler_deadlines_and_identity() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let manual = ManualClock::new(Duration::from_secs(1)).unwrap();
    let source = TaskQueueFactory::cooperative(&manual).unwrap();
    let mut environment = Environment::builder()
        .task_queue_factory(&source)
        .build()
        .unwrap();
    let moved = environment.clone();
    let utility = std::thread::spawn(move || {
        let utility = moved.task_queue_factory().unwrap();
        assert!(utility.is_cooperative());
        assert_eq!(utility.pump(1), Err(BuildEnvironmentError::WrongThread));
        assert!(matches!(
            utility.create_queue("foreign", QueuePriority::Normal),
            Err(BuildEnvironmentError::WrongThread)
        ));
        utility
    })
    .join()
    .unwrap();
    let mut rebound = Environment::builder()
        .task_queue_factory(&utility)
        .build()
        .unwrap();
    let second = rebound.task_queue_factory().unwrap();
    let queue = second
        .create_queue_bytes(b"cooperative\0\xff", QueuePriority::High)
        .unwrap();
    let identity = queue.identity().unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let result = observed.clone();
    assert!(
        queue
            .post_delayed(Duration::from_millis(10), move || {
                let current = TaskQueueContext::current().unwrap();
                assert_eq!(current.identity(), identity);
                assert!(current.is_current());
                assert_eq!(current.name_bytes(), b"cooperative\0\xff");
                result.lock().unwrap().push(1);
            })
            .unwrap()
    );
    assert_eq!(source.next_deadline(), Some(Duration::from_millis(1010)));
    assert_eq!(utility.next_deadline(), source.next_deadline());
    assert_eq!(second.pump(1).unwrap().dispatched, 0);
    assert_eq!(manual.now(), Duration::from_secs(1));
    environment.close();
    rebound.close();
    manual.advance(Duration::from_millis(10)).unwrap();
    assert_eq!(source.pump(1).unwrap().dispatched, 1);
    assert_eq!(*observed.lock().unwrap(), vec![1]);
    assert_eq!(second.next_deadline(), None);
    assert_eq!(manual.now(), Duration::from_millis(1010));
}

#[test]
fn public_cooperative_queue_is_not_run_by_native_lifecycle_yields() {
    if isolated::run_in_child("public_cooperative_queue_is_not_run_by_native_lifecycle_yields") {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let manual = ManualClock::new(Duration::from_secs(1)).unwrap();
    let source = TaskQueueFactory::cooperative(&manual).unwrap();
    let environment = Environment::builder()
        .task_queue_factory(&source)
        .build()
        .unwrap();
    let utility = environment.task_queue_factory().unwrap();
    let queue = utility
        .create_queue("caller-owned", QueuePriority::Normal)
        .unwrap();
    let completed = Arc::new(AtomicBool::new(false));
    let result = completed.clone();
    assert!(queue.post(move || result.store(true, Ordering::SeqCst)));
    let rebound = Environment::builder()
        .task_queue_factory(&utility)
        .build()
        .unwrap();
    let driver = ControlledPeerDriver::new(&manual).unwrap();
    let network = ControlledSimulatedNetwork::new(&manual, &driver).unwrap();
    let endpoint = network
        .register_endpoint(std::net::Ipv4Addr::new(10, 74, 0, 1).into())
        .unwrap();
    #[cfg(target_os = "linux")]
    let before = std::fs::read_dir("/proc/self/task").unwrap().count();
    let factory = PeerConnectionFactory::builder()
        .environment(rebound)
        .controlled_driver(&driver)
        .network_manager(endpoint.network_manager().unwrap())
        .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
        .build()
        .unwrap();
    drop(factory);
    // Native construction/retirement may yield to engine lifecycle queues, but
    // never to public queues, including ones returned through an SDK utility.
    assert!(!completed.load(Ordering::SeqCst));
    assert_eq!(manual.now(), Duration::from_secs(1));
    #[cfg(target_os = "linux")]
    assert_eq!(
        std::fs::read_dir("/proc/self/task").unwrap().count(),
        before
    );
    assert_eq!(driver.pump(1).dispatched, 1);
    assert!(completed.load(Ordering::SeqCst));
}

#[test]
fn default_utility_retains_system_clock_admission_and_executes_real_queues() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let mut environment = Environment::builder().build().unwrap();
    let utility = environment.task_queue_factory().unwrap();
    assert!(!utility.is_cooperative());
    assert!(!utility.is_provider_backed());
    assert!(ControlledWorld::acquire(0x7402, Duration::from_secs(1)).is_err());
    let mut queue = utility
        .create_queue("native-default", QueuePriority::Normal)
        .unwrap();
    environment.close();
    drop(utility);
    let (tx, rx) = std::sync::mpsc::channel();
    assert!(queue.post(move || tx.send(()).unwrap()));
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(ControlledWorld::acquire(0x7402, Duration::from_secs(1)).is_err());
    queue.close();
    drop(queue);
    drop(ControlledWorld::acquire(0x7402, Duration::from_secs(1)).unwrap());
}

#[test]
fn controlled_world_rejects_production_utilities_and_public_queue_creation() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let world = ControlledWorld::acquire(0x7405, Duration::from_secs(1)).unwrap();
    let manual = ManualClock::new(Duration::from_secs(1)).unwrap();
    let environment = Environment::builder().clock(&manual).build().unwrap();
    assert!(matches!(
        environment.task_queue_factory(),
        Err(BuildEnvironmentError::ProductionRuntimeUnavailable)
    ));
    let stock = TaskQueueFactory::default_threaded().unwrap();
    assert!(matches!(
        stock.create_queue("forbidden", QueuePriority::Normal),
        Err(BuildEnvironmentError::NativeConstructionFailed)
    ));
    let observed = Arc::new(Observed::default());
    let provider = TaskQueueFactory::from_provider(Provider(observed.clone())).unwrap();
    assert!(matches!(
        provider.create_queue("forbidden-provider", QueuePriority::Normal),
        Err(BuildEnvironmentError::NativeConstructionFailed)
    ));
    assert!(observed.requests.lock().unwrap().is_empty());
    assert!(!provider.has_failed());
    assert_eq!(world.now(), Duration::from_secs(1));
}

#[test]
fn standalone_production_queue_reserves_execution_through_native_deletion() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let factory = TaskQueueFactory::default_threaded().unwrap();
    let mut queue = factory
        .create_queue("production-admission", QueuePriority::Normal)
        .unwrap();
    assert!(ControlledWorld::acquire(0x7406, Duration::from_secs(1)).is_err());
    queue.close();
    // Unlike an environment utility, the bare factory does not retain an SDK
    // environment or reserve production before any queue is actually created.
    drop(ControlledWorld::acquire(0x7406, Duration::from_secs(1)).unwrap());
}

struct Capture(Arc<Mutex<Vec<TaskQueueFactory>>>);
struct Logger;
impl RtcEventLogFactory for Capture {
    fn create(&self, context: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        assert_eq!(
            context.field_trials().lookup("Queue-Context"),
            "EnabledPeer"
        );
        self.0
            .lock()
            .unwrap()
            .push(context.environment().task_queue_factory().unwrap());
        Box::new(Logger)
    }
}
impl RtcEventLogger for Logger {
    fn start(&self, _: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        false
    }
    fn stop(&self) {}
    fn log(&self, _: RtcEvent) {}
}
#[test]
fn actual_peer_creation_utility_outlives_native_peer_factory_and_context() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let lease = RandomnessLease::acquire(0x7403).unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let environment = Environment::builder().randomness(&lease).build().unwrap();
    let factory = PeerConnectionFactory::builder()
        .environment(environment)
        .event_log_factory(Capture(contexts.clone()))
        .build()
        .unwrap();
    let mut peer = factory
        .create_peer_connection_with_dependencies(
            RtcConfiguration::default(),
            PeerConnectionDependencies::default()
                .field_trials(FieldTrials::parse("Queue-Context/EnabledPeer/").unwrap()),
        )
        .unwrap();
    let utility = contexts
        .lock()
        .unwrap()
        .pop()
        .expect("selected native event log factory was not called");
    peer.close().unwrap();
    drop(peer);
    drop(factory);
    drop(lease);
    let mut queue = utility
        .create_queue("retained-context", QueuePriority::Normal)
        .unwrap();
    drop(utility);
    let (tx, rx) = std::sync::mpsc::channel();
    assert!(queue.post(move || tx.send(()).unwrap()));
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        RandomnessLease::acquire(0x7404),
        Err(RandomnessLeaseError::AlreadyAcquired)
    ));
    queue.close();
    drop(queue);
    drop(RandomnessLease::acquire(0x7404).unwrap());
}
