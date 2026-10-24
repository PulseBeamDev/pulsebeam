use pulsebeam_webrtc_sys::*;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake},
    time::Duration,
};
static GLOBAL: Mutex<()> = Mutex::new(());
#[derive(Default)]
struct State {
    output: Mutex<Option<EventLogOutput>>,
    completion: Mutex<Option<EventLogStopCompletion>>,
    events: Mutex<Vec<RtcEvent>>,
    creations: AtomicUsize,
    factory_drops: AtomicUsize,
    product_drops: AtomicUsize,
    output_drops: AtomicUsize,
    times: Mutex<Vec<i64>>,
    bytes: Mutex<Vec<u8>>,
}
struct Factory(Arc<State>);
impl RtcEventLogFactory for Factory {
    fn create(&self, environment: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        self.0.creations.fetch_add(1, Ordering::SeqCst);
        self.0.times.lock().unwrap().push(environment.time_us());
        Box::new(Logger(self.0.clone()))
    }
}
impl Drop for Factory {
    fn drop(&mut self) {
        self.0.factory_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Logger(Arc<State>);
impl RtcEventLogger for Logger {
    fn start(&self, output: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        *self.0.output.lock().unwrap() = Some(output);
        true
    }
    fn stop(&self) {
        drop(self.0.output.lock().unwrap().take());
    }
    fn stop_async(&self, completion: EventLogStopCompletion) {
        *self.0.completion.lock().unwrap() = Some(completion);
    }
    fn log(&self, event: RtcEvent) {
        self.0.events.lock().unwrap().push(event);
    }
}
impl Drop for Logger {
    fn drop(&mut self) {
        self.0.product_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Output(Arc<State>);
impl EventLogOutputSink for Output {
    fn is_active(&self) -> bool {
        true
    }
    fn write(&mut self, bytes: &[u8]) -> bool {
        self.0.bytes.lock().unwrap().extend_from_slice(bytes);
        true
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        self.0.output_drops.fetch_add(1, Ordering::SeqCst);
    }
}
fn output(state: &Arc<State>) -> EventLogOutput {
    EventLogOutput::from_sink(Output(state.clone()))
}
fn event() -> RtcEvent {
    RtcEvent::from_data(RtcEventData::AlrState { in_alr: true }).unwrap()
}
fn complete(state: &State) {
    drop(state.output.lock().unwrap().take());
    let completion = state.completion.lock().unwrap().take().unwrap();
    completion.complete();
}
#[derive(Default)]
struct Wakes(AtomicUsize);
impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn stop_panic_reports_failure_even_when_unwinding_completes_the_native_token() {
    let _guard = GLOBAL.lock().unwrap();
    struct PanicLogger(Arc<State>);
    impl RtcEventLogger for PanicLogger {
        fn start(&self, output: EventLogOutput, _: EventLogOutputPeriod) -> bool {
            *self.0.output.lock().unwrap() = Some(output);
            true
        }
        fn stop(&self) {
            drop(self.0.output.lock().unwrap().take());
        }
        fn stop_async(&self, _completion: EventLogStopCompletion) {
            panic!("stop failed before output release");
        }
        fn log(&self, _: RtcEvent) {}
    }
    impl Drop for PanicLogger {
        fn drop(&mut self) {
            drop(self.0.output.lock().unwrap().take());
        }
    }
    struct PanicFactory(Arc<State>);
    impl RtcEventLogFactory for PanicFactory {
        fn create(&self, _: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
            Box::new(PanicLogger(self.0.clone()))
        }
    }
    let state = Arc::new(State::default());
    let environment = Environment::builder().build().unwrap();
    let mut log = RtcEventLogFactoryHandle::from_provider(PanicFactory(state.clone()))
        .create(&environment)
        .unwrap();
    log.start(output(&state), EventLogOutputPeriod::Immediate)
        .unwrap();
    let mut stop = log.stop_async().unwrap();
    assert_eq!(stop.try_take(), Some(Err(EventLogError::ProviderFailed)));
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 0);
    assert_eq!(log.log(event()), Err(EventLogError::ProviderFailed));
    assert_eq!(
        log.start(output(&state), EventLogOutputPeriod::Immediate),
        Err(EventLogError::ProviderFailed)
    );
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 1);
    log.close();
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 2);
}

#[test]
fn selected_native_factory_creates_independent_product_and_exact_async_stop() {
    let _guard = GLOBAL.lock().unwrap();
    fn shared<T: Send + Sync>() {}
    fn send<T: Send>() {}
    shared::<RtcEventLogFactoryHandle>();
    send::<RtcEventLogStopOperation>();
    let state = Arc::new(State::default());
    let clock = ManualClock::new(Duration::from_secs(10)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let factory = RtcEventLogFactoryHandle::from_provider(Factory(state.clone()));
    let mut log = factory.create(&environment).unwrap();
    drop((factory, environment));
    assert_eq!(state.creations.load(Ordering::SeqCst), 1);
    assert_eq!(*state.times.lock().unwrap(), [10_000_000]);
    assert_eq!(state.factory_drops.load(Ordering::SeqCst), 1);
    log.start(output(&state), EventLogOutputPeriod::Immediate)
        .unwrap();
    let mut original = event();
    original.set_timestamp_us(-123).unwrap();
    log.log(original).unwrap();
    let retained = state.events.lock().unwrap().pop().unwrap();
    assert_eq!(retained.timestamp_us(), -123);
    assert_eq!(retained.data(), RtcEventData::AlrState { in_alr: true });
    let mut stop = log.stop_async().unwrap();
    assert_eq!(stop.try_take(), None);
    assert_eq!(log.log(event()), Err(EventLogError::StopPending));
    assert_eq!(log.stop(), Err(EventLogError::StopPending));
    assert!(matches!(log.stop_async(), Err(EventLogError::StopPending)));
    assert_eq!(
        log.start(output(&state), EventLogOutputPeriod::Immediate),
        Err(EventLogError::StopPending)
    );
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 1);
    let wakes = Arc::new(Wakes::default());
    let waker = std::task::Waker::from(wakes.clone());
    assert_eq!(
        Pin::new(&mut stop).poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    );
    complete(&state);
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 2);
    assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
    assert_eq!(stop.try_take(), Some(Ok(())));
    assert_eq!(stop.try_take(), None);
    log.start(output(&state), EventLogOutputPeriod::Immediate)
        .unwrap();
    log.stop().unwrap();
    log.close();
    log.close();
    assert_eq!(log.log(event()), Err(EventLogError::Closed));
    drop(log);
    assert_eq!(state.product_drops.load(Ordering::SeqCst), 1);
    std::thread::spawn(move || assert_eq!(retained.timestamp_us(), -123))
        .join()
        .unwrap();
}

#[test]
fn controlled_pending_stop_retains_output_product_and_lease_without_progress_or_wait() {
    let _guard = GLOBAL.lock().unwrap();
    let lease = RandomnessLease::acquire(3404).unwrap();
    let clock = ManualClock::new(Duration::from_secs(10)).unwrap();
    let queues = TaskQueueFactory::cooperative(&clock).unwrap();
    let environment = Environment::builder()
        .clock(&clock)
        .task_queue_factory(&queues)
        .randomness(&lease)
        .build()
        .unwrap();
    assert!(matches!(
        RtcEventLogFactoryHandle::builtin().create(&environment),
        Err(EventLogError::MissingRuntimeAuthority)
    ));
    assert_eq!(clock.now(), Duration::from_secs(10));
    assert_eq!(queues.next_deadline(), None);
    let state = Arc::new(State::default());
    let factory = RtcEventLogFactoryHandle::from_provider(Factory(state.clone()));
    let mut log = factory.create(&environment).unwrap();
    log.start(output(&state), EventLogOutputPeriod::Immediate)
        .unwrap();
    let stop = log.stop_async().unwrap();
    drop(stop);
    log.close();
    drop((log, factory, environment, lease));
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 0);
    assert_eq!(state.product_drops.load(Ordering::SeqCst), 0);
    assert_eq!(
        RandomnessLease::acquire(3404).err(),
        Some(RandomnessLeaseError::AlreadyAcquired)
    );
    assert_eq!(clock.now(), Duration::from_secs(10));
    assert_eq!(queues.next_deadline(), None);
    // Only caller completion retires the product. No pump, time advance,
    // autonomous worker, destructor wait or observation token is needed.
    complete(&state);
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 1);
    assert_eq!(state.product_drops.load(Ordering::SeqCst), 1);
    drop(RandomnessLease::acquire(3404).unwrap());
}

#[test]
fn stock_product_outputs_native_bytes_and_reserves_production_until_final_release() {
    let _guard = GLOBAL.lock().unwrap();
    let environment = Environment::builder().build().unwrap();
    let factory = RtcEventLogFactoryHandle::builtin();
    let mut log = factory.create(&environment).unwrap();
    assert!(ControlledWorld::acquire(3404, Duration::from_secs(10)).is_err());
    let state = Arc::new(State::default());
    log.start(output(&state), EventLogOutputPeriod::Immediate)
        .unwrap();
    log.log(event()).unwrap();
    let mut stop = log.stop_async().unwrap();
    // Production Drop joins pending native output before destroying the SDK
    // logger, rather than destroying the logger on its own output queue.
    drop(log);
    assert_eq!(stop.try_take(), Some(Ok(())));
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 1);
    assert!(!state.bytes.lock().unwrap().is_empty());
    drop(environment);
    let world = ControlledWorld::acquire(3404, Duration::from_secs(10)).unwrap();
    let production_environment = Environment::builder().build().unwrap();
    assert!(matches!(
        factory.create(&production_environment),
        Err(EventLogError::MissingRuntimeAuthority)
    ));
    drop((production_environment, world));
}

#[test]
fn factory_handle_installs_in_production_and_retires_after_migration() {
    let _guard = GLOBAL.lock().unwrap();
    let state = Arc::new(State::default());
    let selected = RtcEventLogFactoryHandle::from_provider(Factory(state.clone()));
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .event_log_factory_handle(&RtcEventLogFactoryHandle::builtin())
        .event_log_factory_handle(&selected)
        .build()
        .unwrap();
    drop(selected);
    session.create_peer(PeerConfiguration::default()).unwrap();
    std::thread::spawn(move || {
        session.create_peer(PeerConfiguration::default()).unwrap();
        session.shutdown().unwrap();
        drop(session);
    })
    .join()
    .unwrap();
    assert_eq!(state.creations.load(Ordering::SeqCst), 2);
    assert_eq!(state.product_drops.load(Ordering::SeqCst), 2);
    assert_eq!(state.factory_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn native_kill_switch_and_factory_failure_reject_without_substituting_stock_output() {
    let _guard = GLOBAL.lock().unwrap();
    let environment = Environment::builder()
        .field_trials(FieldTrials::parse("WebRTC-RtcEventLogKillSwitch/Enabled/").unwrap())
        .build()
        .unwrap();
    let mut log = RtcEventLogFactoryHandle::builtin()
        .create(&environment)
        .unwrap();
    let state = Arc::new(State::default());
    assert_eq!(
        log.start(output(&state), EventLogOutputPeriod::Immediate),
        Err(EventLogError::NativeRejected)
    );
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 1);
    assert!(state.bytes.lock().unwrap().is_empty());
    struct Reject;
    impl RtcEventLogFactory for Reject {
        fn create(&self, _: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
            panic!("factory rejected");
        }
    }
    let factory = RtcEventLogFactoryHandle::from_provider(Reject);
    assert!(matches!(
        factory.create(&environment),
        Err(EventLogError::ProviderFailed)
    ));
    assert!(factory.has_failed());
    assert!(matches!(
        factory.create(&environment),
        Err(EventLogError::ProviderFailed)
    ));
}

#[test]
fn pending_product_installation_preserves_shared_admission_and_final_lease() {
    let _guard = GLOBAL.lock().unwrap();
    let lease = RandomnessLease::acquire(3405).unwrap();
    let clock = ManualClock::new(Duration::from_secs(10)).unwrap();
    let queues = TaskQueueFactory::cooperative(&clock).unwrap();
    let mut source = Environment::builder()
        .task_queue_factory(&queues)
        .randomness(&lease)
        .build()
        .unwrap();
    let state = Arc::new(State::default());
    let provider = RtcEventLogFactoryHandle::from_provider(Factory(state.clone()));
    let mut product = provider.create(&source).unwrap();
    product
        .start(output(&state), EventLogOutputPeriod::Immediate)
        .unwrap();
    let mut stop = product.stop_async().unwrap();
    let mut selected = EnvironmentFactory::from_environment(&source).unwrap();
    selected.set_event_log_product(product).unwrap();
    let mut first = selected.create().unwrap();
    let mut second = selected.clone().create().unwrap();
    let mut a = first.event_log().unwrap();
    let mut b = second.event_log().unwrap();
    assert_eq!(a.log(event()), Err(EventLogError::StopPending));
    assert_eq!(b.stop(), Err(EventLogError::StopPending));
    assert_eq!(state.creations.load(Ordering::SeqCst), 1);
    complete(&state);
    assert_eq!(stop.try_take(), Some(Ok(())));
    a.log(event()).unwrap();
    b.log(event()).unwrap();
    assert_eq!(state.events.lock().unwrap().len(), 2);
    // Admit another stop and retire every public owner before caller completion.
    drop(b.stop_async().unwrap());
    a.close();
    b.close();
    first.close();
    second.close();
    selected.close();
    source.close();
    drop((provider, lease));
    assert_eq!(state.product_drops.load(Ordering::SeqCst), 0);
    assert!(RandomnessLease::acquire(3405).is_err());
    assert_eq!(clock.now(), Duration::from_secs(10));
    assert_eq!(queues.next_deadline(), None);
    complete(&state);
    assert_eq!(state.product_drops.load(Ordering::SeqCst), 1);
    assert_eq!(state.factory_drops.load(Ordering::SeqCst), 1);
    drop(RandomnessLease::acquire(3405).unwrap());
}

#[test]
fn installed_custom_product_receives_real_peer_controls_after_actor_migration() {
    let _guard = GLOBAL.lock().unwrap();
    let source = Environment::builder().build().unwrap();
    let state = Arc::new(State::default());
    let provider = RtcEventLogFactoryHandle::from_provider(Factory(state.clone()));
    let mut product = provider.create(&source).unwrap();
    product
        .start(output(&state), EventLogOutputPeriod::Immediate)
        .unwrap();
    let mut selected = EnvironmentFactory::from_environment(&source).unwrap();
    selected.set_event_log_product(product).unwrap();
    assert!(state.output.lock().unwrap().is_some());
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 0);
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .environment_factory(&selected)
        .build()
        .unwrap();
    selected.close();
    drop((provider, source));
    let observed = state.clone();
    std::thread::spawn(move || {
        let peer = session.create_peer(PeerConfiguration::default()).unwrap();
        session
            .start_peer_event_log(
                peer,
                output(&observed),
                Some(EventLogOutputPeriod::Immediate),
            )
            .unwrap();
        assert!(observed.output.lock().unwrap().is_some());
        session.stop_peer_event_log(peer).unwrap();
        session.shutdown().unwrap();
    })
    .join()
    .unwrap();
    assert_eq!(state.creations.load(Ordering::SeqCst), 1);
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 2);
    assert_eq!(state.product_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn installed_stock_product_binds_first_control_on_sdk_worker_not_creation_thread() {
    let _guard = GLOBAL.lock().unwrap();
    let source = Environment::builder().build().unwrap();
    let product = RtcEventLogFactoryHandle::builtin().create(&source).unwrap();
    let mut selected = EnvironmentFactory::from_environment(&source).unwrap();
    selected.set_event_log_product(product).unwrap();
    let environment = selected.create().unwrap();
    let mut view = environment.event_log().unwrap();
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .environment_factory(&selected)
        .build()
        .unwrap();
    let state = Arc::new(State::default());
    let observed = state.clone();
    std::thread::spawn(move || {
        let peer = session.create_peer(PeerConfiguration::default()).unwrap();
        session
            .start_peer_event_log(
                peer,
                output(&observed),
                Some(EventLogOutputPeriod::Immediate),
            )
            .unwrap();
        session.stop_peer_event_log(peer).unwrap();
        session.shutdown().unwrap();
    })
    .join()
    .unwrap();
    assert!(!state.bytes.lock().unwrap().is_empty());
    assert_eq!(
        view.start(output(&state), EventLogOutputPeriod::Immediate),
        Err(EventLogError::WrongSequence)
    );
    // Thread-safe delivery does not inherit the control-sequence restriction.
    view.log(event()).unwrap();
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 2);
}

#[test]
fn sdk_kill_switch_product_is_valid_on_controlled_queues_without_progress() {
    let _guard = GLOBAL.lock().unwrap();
    let world = ControlledWorld::acquire(3406, Duration::from_secs(2)).unwrap();
    let clock = ManualClock::new(Duration::from_secs(2)).unwrap();
    let queues = TaskQueueFactory::cooperative(&clock).unwrap();
    let source = Environment::builder()
        .task_queue_factory(&queues)
        .field_trials(FieldTrials::parse("WebRTC-RtcEventLogKillSwitch/Enabled/").unwrap())
        .build()
        .unwrap();
    let product = RtcEventLogFactoryHandle::builtin().create(&source).unwrap();
    let mut factory = EnvironmentFactory::from_environment(&source).unwrap();
    factory.set_event_log_product(product).unwrap();
    let environment = factory.create().unwrap();
    let mut log = environment.event_log().unwrap();
    let state = Arc::new(State::default());
    assert_eq!(
        log.start(output(&state), EventLogOutputPeriod::Immediate),
        Err(EventLogError::NativeRejected)
    );
    log.stop().unwrap();
    assert_eq!(log.stop_async().unwrap().try_take(), Some(Ok(())));
    assert_eq!(state.output_drops.load(Ordering::SeqCst), 1);
    assert!(state.bytes.lock().unwrap().is_empty());
    assert_eq!(queues.next_deadline(), None);
    assert_eq!(clock.now(), Duration::from_secs(2));
    drop((log, environment, factory, source, queues, clock, world));
}
