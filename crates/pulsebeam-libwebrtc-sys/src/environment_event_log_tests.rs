use crate::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
#[derive(Default)]
struct Observed {
    token: Mutex<Option<EventLogStopCompletion>>,
    events: Mutex<Vec<RtcEvent>>,
    drops: AtomicUsize,
}
struct Logger(Arc<Observed>);
impl RtcEventLogger for Logger {
    fn start(&self, _: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        true
    }
    fn stop(&self) {}
    fn stop_async(&self, token: EventLogStopCompletion) {
        *self.0.token.lock().unwrap() = Some(token);
    }
    fn log(&self, event: RtcEvent) {
        self.0.events.lock().unwrap().push(event);
    }
}
impl Drop for Logger {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}
#[test]
fn sdk_producer_alias_defers_owned_events_until_real_stop_completion() {
    let observed = Arc::new(Observed::default());
    let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
    let queues = TaskQueueFactory::cooperative(&clock).unwrap();
    let mut environment = Environment::builder()
        .task_queue_factory(&queues)
        .event_log(Logger(observed.clone()))
        .build()
        .unwrap();
    let mut view = environment.event_log().unwrap();
    let mut stop = view.stop_async().unwrap();
    let mut event = RtcEvent::from_data(RtcEventData::AlrState { in_alr: true }).unwrap();
    event.set_timestamp_us(-431).unwrap();
    // This enters through the actual SDK virtual alias, as native producers
    // do. Public utility calls instead reject while stop is pending.
    ffi::test_event_log_sdk_dispatch(environment.native(), event.into_native());
    assert!(observed.events.lock().unwrap().is_empty());
    view.close();
    environment.close();
    assert_eq!(observed.drops.load(Ordering::SeqCst), 0);
    observed.token.lock().unwrap().take().unwrap().complete();
    assert_eq!(stop.try_take(), Some(Ok(())));
    assert_eq!(observed.events.lock().unwrap()[0].timestamp_us(), -431);
    assert_eq!(
        observed.events.lock().unwrap()[0].data(),
        RtcEventData::AlrState { in_alr: true }
    );
    assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
    assert_eq!(clock.now(), Duration::from_secs(1));
}
struct PanicStop;
impl RtcEventLogger for PanicStop {
    fn start(&self, _: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        false
    }
    fn stop(&self) {}
    fn log(&self, _: RtcEvent) {}
    fn stop_async(&self, _: EventLogStopCompletion) {
        panic!("stop callback failed");
    }
}
#[test]
fn inline_completion_from_unwinding_waits_for_failure_latch() {
    let environment = Environment::builder().event_log(PanicStop).build().unwrap();
    let mut view = environment.event_log().unwrap();
    let mut stop = view.stop_async().unwrap();
    assert_eq!(stop.try_take(), Some(Err(EventLogError::ProviderFailed)));
}

fn stock_environment(source: &Environment) -> Environment {
    let native = ffi::test_stock_event_log_environment(
        source.native(),
        Box::new(RustEventLogRetention {
            _environment: source.clone(),
        }),
    );
    Environment::from_sdk(native, Some(source))
}
struct Bytes(Arc<Mutex<Vec<u8>>>);
impl EventLogOutputSink for Bytes {
    fn is_active(&self) -> bool {
        true
    }
    fn write(&mut self, bytes: &[u8]) -> bool {
        self.0.lock().unwrap().extend_from_slice(bytes);
        true
    }
}
#[test]
fn stock_sdk_utility_calls_real_encoder_and_async_output_retirement() {
    let source = Environment::builder().build().unwrap();
    let mut environment = stock_environment(&source);
    let mut utility = environment.event_log().unwrap();
    let bytes = Arc::new(Mutex::new(Vec::new()));
    utility
        .start(
            EventLogOutput::from_sink(Bytes(bytes.clone())),
            EventLogOutputPeriod::Immediate,
        )
        .unwrap();
    utility
        .log(RtcEvent::from_data(RtcEventData::AlrState { in_alr: true }).unwrap())
        .unwrap();
    let mut stop = utility.stop_async().unwrap();
    utility.close();
    assert_eq!(stop.try_take(), Some(Ok(())));
    assert!(!bytes.lock().unwrap().is_empty());
    environment.close();
}

struct DropLastSdkOwner {
    owner: Arc<Mutex<Option<Environment>>>,
    entered: Option<std::sync::mpsc::Sender<()>>,
    resume: std::sync::mpsc::Receiver<()>,
    retired: std::sync::mpsc::Sender<()>,
}
impl EventLogOutputSink for DropLastSdkOwner {
    fn is_active(&self) -> bool {
        true
    }
    fn write(&mut self, _: &[u8]) -> bool {
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
            self.resume.recv_timeout(Duration::from_secs(5)).unwrap();
            // Drop the final SDK logger owner on its own output queue. The
            // native logger must join off-queue with all creation leases alive.
            drop(self.owner.lock().unwrap().take());
        }
        true
    }
}
impl Drop for DropLastSdkOwner {
    fn drop(&mut self) {
        assert!(
            RandomnessLease::acquire(0xe105).is_err(),
            "output destruction outlived the composed environment lease"
        );
        let _ = self.retired.send(());
    }
}
#[test]
fn final_stock_sdk_owner_on_output_queue_retires_off_queue_with_leases() {
    let lease = RandomnessLease::acquire(0xe104).unwrap();
    // The logger was created without this lease. Composition must attach only
    // the independent lease guard, never an Environment that owns the logger.
    let mut source = Environment::builder().build().unwrap();
    let mut stock = stock_environment(&source);
    let mut factory = EnvironmentFactory::from_environment(&stock).unwrap();
    factory.set_randomness(&lease).unwrap();
    let mut environment = factory.create().unwrap();
    factory.close();
    stock.close();
    let owner = Arc::new(Mutex::new(Some(environment.clone())));
    let (entered, entry) = std::sync::mpsc::channel();
    let (resume, continuation) = std::sync::mpsc::channel();
    let (retired, retirement) = std::sync::mpsc::channel();
    let mut utility = environment.event_log().unwrap();
    utility
        .start(
            EventLogOutput::from_sink(DropLastSdkOwner {
                owner,
                entered: Some(entered),
                resume: continuation,
                retired,
            }),
            EventLogOutputPeriod::Immediate,
        )
        .unwrap();
    entry.recv_timeout(Duration::from_secs(5)).unwrap();
    utility.close();
    drop(utility);
    environment.close();
    source.close();
    drop(lease);
    assert!(RandomnessLease::acquire(0xe105).is_err());
    resume.send(()).unwrap();
    retirement.recv_timeout(Duration::from_secs(5)).unwrap();
    // Sink retirement precedes the SDK queue join and root-lease release.
    // Only production retirement is involved; never spin a controlled clock.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match RandomnessLease::acquire(0xe105) {
            Ok(lease) => {
                drop(lease);
                break;
            }
            Err(_) => {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
        }
    }
}
