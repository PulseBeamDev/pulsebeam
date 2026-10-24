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
    events: Mutex<Vec<RtcEvent>>,
    output: Mutex<Option<EventLogOutput>>,
    completion: Mutex<Option<EventLogStopCompletion>>,
    periods: Mutex<Vec<EventLogOutputPeriod>>,
    dropped: AtomicUsize,
    stops: AtomicUsize,
    fail: AtomicBool,
    reenter: Mutex<Option<Environment>>,
}
struct Logger(Arc<Observed>);
impl RtcEventLogger for Logger {
    fn start(&self, output: EventLogOutput, period: EventLogOutputPeriod) -> bool {
        self.0.periods.lock().unwrap().push(period);
        *self.0.output.lock().unwrap() = Some(output);
        true
    }
    fn stop(&self) {
        self.0.stops.fetch_add(1, Ordering::SeqCst);
        drop(self.0.output.lock().unwrap().take());
    }
    fn stop_async(&self, completion: EventLogStopCompletion) {
        *self.0.completion.lock().unwrap() = Some(completion);
    }
    fn log(&self, event: RtcEvent) {
        assert!(!self.0.fail.load(Ordering::SeqCst), "logger failed");
        self.0.events.lock().unwrap().push(event);
        let environment = self.0.reenter.lock().unwrap().take();
        if let Some(environment) = environment {
            let mut alias = environment.event_log().unwrap();
            assert!(matches!(alias.stop_async(), Err(EventLogError::Busy)));
            alias.log(probe(99)).unwrap();
        }
    }
}
impl Drop for Logger {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Sink(Arc<AtomicUsize>);
impl EventLogOutputSink for Sink {
    fn is_active(&self) -> bool {
        true
    }
    fn write(&mut self, _: &[u8]) -> bool {
        true
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn output(drops: &Arc<AtomicUsize>) -> EventLogOutput {
    EventLogOutput::from_sink(Sink(drops.clone()))
}
fn probe(timestamp: i64) -> RtcEvent {
    let mut event = RtcEvent::from_data(RtcEventData::AlrState { in_alr: true }).unwrap();
    event.set_timestamp_us(timestamp).unwrap();
    event
}
fn complete(observed: &Observed) {
    drop(observed.output.lock().unwrap().take());
    let completion = observed.completion.lock().unwrap().take().unwrap();
    completion.complete();
}
#[test]
fn default_sdk_noop_is_exact_and_closing_views_does_not_close_other_owners() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut environment = Environment::builder().build().unwrap();
    let mut first = environment.event_log().unwrap();
    let mut second = environment.event_log().unwrap();
    environment.close();
    assert!(matches!(
        environment.event_log(),
        Err(EventLogError::Closed)
    ));
    let drops = Arc::new(AtomicUsize::new(0));
    assert_eq!(
        first.start(output(&drops), EventLogOutputPeriod::Immediate),
        Err(EventLogError::NativeRejected)
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    first.log(probe(1)).unwrap();
    first.close();
    first.close();
    assert_eq!(first.log(probe(2)), Err(EventLogError::Closed));
    second.log(probe(3)).unwrap();
    second.stop().unwrap();
    assert_eq!(second.stop_async().unwrap().try_take(), Some(Ok(())));
}
#[test]
fn owning_custom_utility_preserves_events_periods_aliases_and_final_lease() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let lease = RandomnessLease::acquire(0xe100).unwrap();
    let observed = Arc::new(Observed::default());
    let drops = Arc::new(AtomicUsize::new(0));
    let mut environment = Environment::builder()
        .event_log(Logger(observed.clone()))
        .randomness(&lease)
        .build()
        .unwrap();
    let mut first = environment.event_log().unwrap();
    let mut alias = environment.clone().event_log().unwrap();
    environment.close();
    drop(lease);
    first
        .start(
            output(&drops),
            EventLogOutputPeriod::Periodic(Duration::from_millis(17)),
        )
        .unwrap();
    first.log(probe(-123)).unwrap();
    assert_eq!(observed.events.lock().unwrap()[0].timestamp_us(), -123);
    assert_eq!(
        *observed.periods.lock().unwrap(),
        vec![EventLogOutputPeriod::Periodic(Duration::from_millis(17))]
    );
    first.close();
    drop(first);
    assert_eq!(observed.stops.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    alias.stop().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(observed.dropped.load(Ordering::SeqCst), 0);
    assert!(RandomnessLease::acquire(0xe101).is_err());
    alias.close();
    drop(alias);
    assert_eq!(observed.dropped.load(Ordering::SeqCst), 1);
    drop(RandomnessLease::acquire(0xe101).unwrap());
}
#[test]
fn controlled_async_stop_is_shared_and_retains_everything_without_progress() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let lease = RandomnessLease::acquire(0xe102).unwrap();
    let manual = ManualClock::new(Duration::from_secs(1)).unwrap();
    let queues = TaskQueueFactory::cooperative(&manual).unwrap();
    let observed = Arc::new(Observed::default());
    let drops = Arc::new(AtomicUsize::new(0));
    let mut environment = Environment::builder()
        .task_queue_factory(&queues)
        .event_log(Logger(observed.clone()))
        .randomness(&lease)
        .build()
        .unwrap();
    let mut first = environment.event_log().unwrap();
    let mut alias = environment.event_log().unwrap();
    first
        .start(output(&drops), EventLogOutputPeriod::Immediate)
        .unwrap();
    let mut stop = first.stop_async().unwrap();
    assert_eq!(stop.try_take(), None);
    assert_eq!(alias.log(probe(0)), Err(EventLogError::StopPending));
    assert_eq!(alias.stop(), Err(EventLogError::StopPending));
    assert!(matches!(
        alias.stop_async(),
        Err(EventLogError::StopPending)
    ));
    first.close();
    alias.close();
    environment.close();
    drop(first);
    drop(alias);
    drop(lease);
    drop(queues);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(observed.dropped.load(Ordering::SeqCst), 0);
    assert!(RandomnessLease::acquire(0xe103).is_err());
    assert_eq!(manual.now(), Duration::from_secs(1));
    complete(&observed);
    assert_eq!(stop.try_take(), Some(Ok(())));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(observed.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(manual.now(), Duration::from_secs(1));
    drop(RandomnessLease::acquire(0xe103).unwrap());
}
#[test]
fn callback_reentry_and_concurrent_views_never_hold_a_bridge_lock() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let observed = Arc::new(Observed::default());
    let environment = Environment::builder()
        .event_log(Logger(observed.clone()))
        .build()
        .unwrap();
    *observed.reenter.lock().unwrap() = Some(environment.clone());
    environment.event_log().unwrap().log(probe(1)).unwrap();
    let workers = (0..4)
        .map(|n| {
            let environment = environment.clone();
            std::thread::spawn(move || {
                let mut view = environment.event_log().unwrap();
                for i in 0..20 {
                    view.log(probe(100 + 20 * n + i)).unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(observed.events.lock().unwrap().len(), 82);
}
#[test]
fn provider_panics_are_reported_across_aliases_without_stock_substitution() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let observed = Arc::new(Observed::default());
    let environment = Environment::builder()
        .event_log(Logger(observed.clone()))
        .build()
        .unwrap();
    let mut first = environment.event_log().unwrap();
    let mut alias = environment.event_log().unwrap();
    observed.fail.store(true, Ordering::SeqCst);
    assert_eq!(first.log(probe(0)), Err(EventLogError::ProviderFailed));
    let drops = Arc::new(AtomicUsize::new(0));
    assert_eq!(
        alias.start(output(&drops), EventLogOutputPeriod::Immediate),
        Err(EventLogError::ProviderFailed)
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    alias.stop().unwrap_err();
}
#[test]
fn actual_peer_control_uses_installed_utility_and_transports_worker_errors() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let observed = Arc::new(Observed::default());
    let environment = Environment::builder()
        .event_log(Logger(observed.clone()))
        .build()
        .unwrap();
    let mut utility = environment.event_log().unwrap();
    let factory = PeerConnectionFactory::builder()
        .environment(environment)
        .build()
        .unwrap();
    let mut peer = factory
        .create_peer_connection_with_dependencies(
            RtcConfiguration::default(),
            PeerConnectionDependencies::default(),
        )
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    peer.start_event_log(output(&drops), Some(EventLogOutputPeriod::Immediate))
        .unwrap();
    utility.stop().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    utility
        .start(output(&drops), EventLogOutputPeriod::Immediate)
        .unwrap();
    let mut stop = utility.stop_async().unwrap();
    assert_eq!(peer.stop_event_log(), Err(EventLogError::StopPending));
    assert_eq!(
        peer.start_event_log(output(&drops), None),
        Err(EventLogError::StopPending)
    );
    complete(&observed);
    assert_eq!(stop.try_take(), Some(Ok(())));
    observed.fail.store(true, Ordering::SeqCst);
    assert_eq!(utility.log(probe(0)), Err(EventLogError::ProviderFailed));
    assert_eq!(peer.stop_event_log(), Err(EventLogError::ProviderFailed));
    peer.close().unwrap();
}
struct FactoryCapture {
    contexts: Arc<Mutex<Vec<Environment>>>,
    logger: Arc<Observed>,
}
impl RtcEventLogFactory for FactoryCapture {
    fn create(&self, context: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        self.contexts
            .lock()
            .unwrap()
            .push(context.environment().clone());
        Box::new(Logger(self.logger.clone()))
    }
}
#[test]
fn factory_context_getter_uses_prior_sdk_utility_not_newly_created_logger() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let prior = Arc::new(Observed::default());
    let selected = Arc::new(Observed::default());
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let environment = Environment::builder()
        .event_log(Logger(prior.clone()))
        .build()
        .unwrap();
    let factory = PeerConnectionFactory::builder()
        .environment(environment)
        .event_log_factory(FactoryCapture {
            contexts: contexts.clone(),
            logger: selected.clone(),
        })
        .build()
        .unwrap();
    let mut peer = factory
        .create_peer_connection_with_dependencies(
            RtcConfiguration::default(),
            PeerConnectionDependencies::default(),
        )
        .unwrap();
    let mut utility = contexts.lock().unwrap().pop().unwrap().event_log().unwrap();
    peer.close().unwrap();
    drop(peer);
    drop(factory);
    let before = selected.events.lock().unwrap().len();
    utility.log(probe(444)).unwrap();
    assert_eq!(
        prior.events.lock().unwrap().last().unwrap().timestamp_us(),
        444
    );
    assert_eq!(selected.events.lock().unwrap().len(), before);
}
