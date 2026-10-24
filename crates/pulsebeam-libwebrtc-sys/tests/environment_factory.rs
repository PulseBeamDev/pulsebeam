use pulsebeam_webrtc_sys::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
static SERIAL: Mutex<()> = Mutex::new(());
struct Logger(Arc<AtomicUsize>);
impl RtcEventLogger for Logger {
    fn start(&self, _: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        false
    }
    fn stop(&self) {}
    fn log(&self, _: RtcEvent) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn event() -> RtcEvent {
    RtcEvent::from_data(RtcEventData::AlrState { in_alr: true }).unwrap()
}
#[test]
fn composed_factories_preserve_shared_scheduler_and_log_selection_after_original_close() {
    let _guard = SERIAL.lock().unwrap();
    let clock = ManualClock::new(Duration::from_secs(4)).unwrap();
    let queues = TaskQueueFactory::cooperative(&clock).unwrap();
    let a = Arc::new(AtomicUsize::new(0));
    let b = Arc::new(AtomicUsize::new(0));
    let mut original = Environment::builder()
        .task_queue_factory(&queues)
        .event_log(Logger(a.clone()))
        .field_trials(FieldTrials::parse("Original/Enabled/").unwrap())
        .build()
        .unwrap();
    let mut factory = EnvironmentFactory::from_environment(&original).unwrap();
    factory
        .set_field_trials(FieldTrials::parse("Composed/Enabled/").unwrap())
        .unwrap();
    let first = factory.create().unwrap();
    let second = factory.create().unwrap();
    let mut branch = factory.clone();
    branch.set_event_log(Logger(b.clone())).unwrap();
    let third = branch.create().unwrap();
    factory.close();
    branch.close();
    original.close();
    assert!(first.field_trials().is_enabled("Composed"));
    assert!(!first.field_trials().is_enabled("Original"));
    first.event_log().unwrap().log(event()).unwrap();
    second.event_log().unwrap().log(event()).unwrap();
    third.event_log().unwrap().log(event()).unwrap();
    assert_eq!(a.load(Ordering::SeqCst), 2);
    assert_eq!(b.load(Ordering::SeqCst), 1);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let one = first
        .task_queue_factory()
        .unwrap()
        .create_queue("one", QueuePriority::Normal)
        .unwrap();
    let two = second
        .task_queue_factory()
        .unwrap()
        .create_queue("two", QueuePriority::Normal)
        .unwrap();
    let values = observed.clone();
    one.post_delayed(Duration::from_micros(8), move || {
        values.lock().unwrap().push(1)
    })
    .unwrap();
    let values = observed.clone();
    two.post_delayed(Duration::from_micros(4), move || {
        values.lock().unwrap().push(2)
    })
    .unwrap();
    assert_eq!(queues.run_ready(), 0);
    clock.advance(Duration::from_micros(8)).unwrap();
    assert_eq!(queues.run_ready(), 2);
    assert_eq!(*observed.lock().unwrap(), vec![2, 1]);
    assert_eq!(third.timestamp(), first.timestamp());
    assert_eq!(first.timestamp().microseconds().unwrap(), 4_000_008);
}
#[test]
fn sdk_defaults_create_real_queues_and_closed_factories_reject_without_replacing_inputs() {
    let _guard = SERIAL.lock().unwrap();
    let mut factory = EnvironmentFactory::new();
    let mut first = factory.create().unwrap();
    let second = factory.create().unwrap();
    factory.close();
    assert!(matches!(
        factory.create(),
        Err(BuildEnvironmentError::Closed)
    ));
    assert!(matches!(
        factory.clone().create(),
        Err(BuildEnvironmentError::Closed)
    ));
    assert_eq!(
        factory.set_field_trials(FieldTrials::default()),
        Err(BuildEnvironmentError::Closed)
    );
    let queue = second
        .task_queue_factory()
        .unwrap()
        .create_queue("sdk-default", QueuePriority::Normal)
        .unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    assert!(queue.post(move || send.send(17).unwrap()));
    assert_eq!(receive.recv_timeout(Duration::from_secs(5)).unwrap(), 17);
    first.close();
    assert!(matches!(
        EnvironmentFactory::from_environment(&first),
        Err(BuildEnvironmentError::Closed)
    ));
}
struct DropWithLease(Arc<AtomicUsize>);
impl RtcEventLogger for DropWithLease {
    fn start(&self, _: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        false
    }
    fn stop(&self) {}
    fn log(&self, _: RtcEvent) {}
}
impl Drop for DropWithLease {
    fn drop(&mut self) {
        assert!(
            RandomnessLease::acquire(0xf102).is_err(),
            "SDK destruction outlived the selected Rust lease"
        );
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[test]
fn trial_copies_retain_sdk_storage_and_replaced_input_dependencies_until_final_drop() {
    let _guard = SERIAL.lock().unwrap();
    let lease = RandomnessLease::acquire(0xf101).unwrap();
    let mut source = Environment::builder().randomness(&lease).build().unwrap();
    let delegated = source.task_queue_factory().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut factory = EnvironmentFactory::new();
    factory.set_randomness(&lease).unwrap();
    factory.set_task_queue_factory(&delegated).unwrap();
    factory
        .set_task_queue_factory(&TaskQueueFactory::default_threaded().unwrap())
        .unwrap();
    factory.set_event_log(DropWithLease(drops.clone())).unwrap();
    factory
        .set_field_trials(FieldTrials::parse("Retained/Enabled/").unwrap())
        .unwrap();
    let mut environment = factory.create().unwrap();
    let snapshot = environment.field_trials();
    let copy = snapshot.clone();
    factory.close();
    source.close();
    drop(delegated);
    drop(lease);
    environment.close();
    drop(snapshot);
    assert!(copy.is_enabled("Retained"));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(RandomnessLease::acquire(0xf102).is_err());
    drop(copy);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(RandomnessLease::acquire(0xf102).unwrap());
}
struct Capture(Arc<Mutex<Vec<EventLogEnvironment>>>);
impl RtcEventLogFactory for Capture {
    fn create(&self, context: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        self.0.lock().unwrap().push(context);
        Box::new(Logger(Arc::new(AtomicUsize::new(0))))
    }
}
#[test]
fn actor_uses_factory_snapshot_overrides_and_real_sdk_creation_after_migration() {
    let _guard = SERIAL.lock().unwrap();
    let clock = ManualClock::new(Duration::from_secs(5)).unwrap();
    let logs = Arc::new(AtomicUsize::new(0));
    let mut factory = EnvironmentFactory::new();
    factory.set_clock(&clock).unwrap();
    factory
        .set_field_trials(FieldTrials::parse("Factory/Enabled/").unwrap())
        .unwrap();
    factory.set_event_log(Logger(logs.clone())).unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let builder = ProductionSession::builder(ProductionSessionConfig::default())
        .environment_factory(&factory)
        .field_trials(FieldTrials::parse("Actor/Enabled/").unwrap())
        .event_log_factory(Capture(contexts.clone()));
    factory
        .set_field_trials(FieldTrials::parse("Later/Enabled/").unwrap())
        .unwrap();
    factory.close();
    let mut actor = builder.build().unwrap();
    actor.create_peer(PeerConfiguration::default()).unwrap();
    clock.advance(Duration::from_micros(1)).unwrap();
    std::thread::spawn(move || {
        actor.create_peer(PeerConfiguration::default()).unwrap();
        actor.shutdown().unwrap();
    })
    .join()
    .unwrap();
    let captured = std::mem::take(&mut *contexts.lock().unwrap());
    assert_eq!(captured.len(), 2);
    let before = logs.load(Ordering::SeqCst);
    for context in captured {
        assert_eq!(context.timestamp().microseconds().unwrap(), 5_000_001);
        assert!(context.field_trials().is_enabled("Actor"));
        assert!(!context.field_trials().is_enabled("Factory"));
        assert!(!context.field_trials().is_enabled("Later"));
        context
            .environment()
            .event_log()
            .unwrap()
            .log(event())
            .unwrap();
    }
    assert_eq!(logs.load(Ordering::SeqCst), before + 2);
}
#[test]
fn controlled_factory_keeps_creator_and_requires_explicit_matching_clock() {
    let _guard = SERIAL.lock().unwrap();
    let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
    let queues = TaskQueueFactory::cooperative(&clock).unwrap();
    let mut factory = EnvironmentFactory::new();
    factory.set_task_queue_factory(&queues).unwrap();
    assert!(matches!(
        factory.create(),
        Err(BuildEnvironmentError::MismatchedCooperativeClock)
    ));
    factory.set_clock(&clock).unwrap();
    factory.create().unwrap();
    let other = factory.clone();
    assert_eq!(
        std::thread::spawn(move || other.create().err())
            .join()
            .unwrap(),
        Some(BuildEnvironmentError::WrongThread)
    );
    assert!(
        ProductionSession::builder(ProductionSessionConfig::default())
            .environment_factory(&factory)
            .build()
            .is_err()
    );
    assert_eq!(clock.now(), Duration::from_secs(1));
}
