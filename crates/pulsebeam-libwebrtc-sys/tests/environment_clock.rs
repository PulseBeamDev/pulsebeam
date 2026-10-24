use pulsebeam_webrtc_sys::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};

static SERIAL: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct Observed {
    time: AtomicI64,
    conversions: Mutex<Vec<Timestamp>>,
    fail: AtomicBool,
    drops: AtomicUsize,
}
struct Provider(Arc<Observed>);
impl ClockProvider for Provider {
    fn current_time(&self) -> FiniteTimestamp {
        assert!(!self.0.fail.load(Ordering::SeqCst), "selected clock failed");
        FiniteTimestamp::from_microseconds(self.0.time.load(Ordering::SeqCst)).unwrap()
    }
    fn convert_timestamp_to_ntp_time(&self, timestamp: Timestamp) -> NtpTime {
        self.0.conversions.lock().unwrap().push(timestamp);
        NtpTime::from_parts(123, 456)
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}
fn selected(observed: &Arc<Observed>) -> Clock {
    observed.time.store(200, Ordering::SeqCst);
    Clock::from_monotonic_bounded_provider(
        Provider(observed.clone()),
        ClockTimeBounds::new(
            FiniteTimestamp::from_microseconds(50).unwrap(),
            FiniteTimestamp::from_microseconds(500).unwrap(),
        )
        .unwrap(),
    )
}

#[test]
fn utility_delegates_native_clock_and_retains_provider_failure_and_bounds() {
    let _serial = SERIAL.lock().unwrap();
    let observed = Arc::new(Observed::default());
    let source = selected(&observed);
    let mut environment = Environment::builder()
        .clock_handle(&source)
        .build()
        .unwrap();
    let utility = environment.clock().unwrap();
    assert_eq!(utility.time_bounds(), source.time_bounds());
    assert!(utility.is_monotonic());
    drop(source);
    environment.close();
    assert!(matches!(
        environment.clock(),
        Err(EnvironmentClockError::Closed)
    ));
    observed.time.store(300, Ordering::SeqCst);
    let mut rebound = Environment::builder()
        .clock_handle(&utility)
        .build()
        .unwrap();
    assert_eq!(
        rebound.timestamp(),
        Timestamp::from_microseconds(300).unwrap()
    );
    rebound.close();
    assert_eq!(utility.time_in_microseconds(), 300);
    assert_eq!(utility.time_in_milliseconds(), 0);
    assert_eq!(
        utility.current_ntp_time().unwrap(),
        NtpTime::from_parts(123, 456)
    );
    utility
        .convert_timestamp_to_ntp_time(Timestamp::plus_infinity())
        .unwrap();
    assert_eq!(
        observed.conversions.lock().unwrap().last(),
        Some(&Timestamp::plus_infinity())
    );
    assert_eq!(observed.drops.load(Ordering::SeqCst), 0);
    observed.fail.store(true, Ordering::SeqCst);
    assert_eq!(utility.time_in_microseconds(), 300);
    assert!(utility.has_failed());
    drop(utility);
    assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn manual_utility_keeps_native_time_and_rust_only_lease_without_progress() {
    let _serial = SERIAL.lock().unwrap();
    let lease = RandomnessLease::acquire(0xc10c).unwrap();
    let manual = ManualClock::new(Duration::from_secs(1)).unwrap();
    let queues = TaskQueueFactory::cooperative(&manual).unwrap();
    let mut environment = Environment::builder()
        .task_queue_factory(&queues)
        .randomness(&lease)
        .build()
        .unwrap();
    let utility = environment.clock().unwrap();
    let mut rebound = Environment::builder()
        .task_queue_factory(&queues)
        .clock_handle(&utility)
        .build()
        .unwrap();
    let rebound_clock = rebound.clock().unwrap();
    let other = ManualClock::new(Duration::from_secs(1)).unwrap();
    let other_queues = TaskQueueFactory::cooperative(&other).unwrap();
    assert!(matches!(
        Environment::builder()
            .task_queue_factory(&other_queues)
            .clock_handle(&utility)
            .build(),
        Err(BuildEnvironmentError::MismatchedCooperativeClock)
    ));
    drop(other_queues);
    drop(other);
    manual.advance(Duration::from_millis(250)).unwrap();
    let reached = Arc::new(AtomicI64::new(0));
    let result = reached.clone();
    let queue = queues
        .create_queue("clock-utility", QueuePriority::Normal)
        .unwrap();
    queue.post(move || result.store(rebound_clock.time_in_microseconds(), Ordering::SeqCst));
    assert_eq!(queues.run_ready(), 1);
    assert_eq!(reached.load(Ordering::SeqCst), 1_250_000);
    drop(queue);
    rebound.close();
    assert_eq!(utility.time_in_microseconds(), 1_250_000);
    assert_eq!(utility.current_ntp_time().unwrap().seconds(), 2_208_988_801);
    assert_eq!(
        utility.convert_timestamp_to_ntp_time(Timestamp::from_microseconds(-1_000_000).unwrap()),
        Ok(NtpTime::from_parts(2_208_988_799, 0))
    );
    assert_eq!(
        utility.convert_timestamp_to_ntp_time(Timestamp::plus_infinity()),
        Err(TimeValueError::InfiniteValue)
    );
    drop(lease);
    drop(queues);
    drop(manual);
    environment.close();
    assert_eq!(utility.time_in_microseconds(), 1_250_000);
    assert!(matches!(
        RandomnessLease::acquire(0xc10d),
        Err(RandomnessLeaseError::AlreadyAcquired)
    ));
    drop(utility);
    drop(RandomnessLease::acquire(0xc10d).unwrap());
}

#[test]
fn default_utility_uses_sdk_time_and_retains_system_clock_admission() {
    let _serial = SERIAL.lock().unwrap();
    let mut environment = Environment::builder().build().unwrap();
    let before = environment.timestamp();
    let utility = environment.clock().unwrap();
    let actual = utility.current_time();
    let after = environment.timestamp();
    assert!(before <= actual && actual <= after);
    assert!(utility.is_monotonic());
    assert!(utility.current_ntp_time().unwrap().is_valid());
    assert_eq!(
        utility.convert_timestamp_to_ntp_time(Timestamp::minus_infinity()),
        Err(TimeValueError::InfiniteValue)
    );
    assert!(utility.convert_timestamp_to_ntp_time(actual).is_ok());
    environment.close();
    assert!(ControlledWorld::acquire(0xc10e, Duration::from_secs(1)).is_err());
    drop(utility);
    drop(ControlledWorld::acquire(0xc10e, Duration::from_secs(1)).unwrap());
}

struct Capture(Arc<Mutex<Vec<Clock>>>);
struct Logger;
impl RtcEventLogFactory for Capture {
    fn create(&self, environment: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        assert_eq!(
            environment.field_trials().lookup("Clock-Context"),
            "EnabledPeer"
        );
        self.0
            .lock()
            .unwrap()
            .push(environment.environment().clock().unwrap());
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
fn actual_peer_creation_clock_utility_outlives_peer_factory_and_registration() {
    let _serial = SERIAL.lock().unwrap();
    let observed = Arc::new(Observed::default());
    let source = selected(&observed);
    let lease = RandomnessLease::acquire(0xc10f).unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let environment = Environment::builder()
        .clock_handle(&source)
        .randomness(&lease)
        .field_trials(FieldTrials::parse("Clock-Context/EnabledFactory/").unwrap())
        .build()
        .unwrap();
    let factory = PeerConnectionFactory::builder()
        .environment(environment)
        .event_log_factory(Capture(contexts.clone()))
        .build()
        .unwrap();
    let mut peer = factory
        .create_peer_connection_with_dependencies(
            RtcConfiguration::default(),
            PeerConnectionDependencies::default()
                .field_trials(FieldTrials::parse("Clock-Context/EnabledPeer/").unwrap()),
        )
        .unwrap();
    let utility = contexts
        .lock()
        .unwrap()
        .pop()
        .expect("native selected factory was not called");
    assert_eq!(utility.time_bounds(), source.time_bounds());
    assert!(utility.is_monotonic());
    peer.close().unwrap();
    drop(peer);
    drop(factory);
    drop(source);
    drop(lease);
    observed.time.store(400, Ordering::SeqCst);
    assert_eq!(utility.time_in_microseconds(), 400);
    assert_eq!(observed.drops.load(Ordering::SeqCst), 0);
    assert!(matches!(
        RandomnessLease::acquire(0xc110),
        Err(RandomnessLeaseError::AlreadyAcquired)
    ));
    drop(utility);
    assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
    drop(RandomnessLease::acquire(0xc110).unwrap());
}
