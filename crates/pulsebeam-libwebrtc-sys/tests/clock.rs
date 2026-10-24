use pulsebeam_webrtc_sys::{
    Clock, ClockProvider, Environment, FiniteTimestamp, ManualClock, NtpTime,
    PeerConnectionFactory, ProductionSession, ProductionSessionConfig, TaskQueueFactory,
    TimeValueError, Timestamp,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
};
use std::time::Duration;

struct Provider {
    time: AtomicI64,
    time_calls: AtomicUsize,
    conversion_calls: AtomicUsize,
    panic_time: AtomicBool,
    panic_conversion: AtomicBool,
    seen: Mutex<Vec<Timestamp>>,
}
impl Provider {
    fn new(time: i64) -> Self {
        Self {
            time: AtomicI64::new(time),
            time_calls: AtomicUsize::new(0),
            conversion_calls: AtomicUsize::new(0),
            panic_time: AtomicBool::new(false),
            panic_conversion: AtomicBool::new(false),
            seen: Mutex::new(Vec::new()),
        }
    }
}
impl ClockProvider for Provider {
    fn current_time(&self) -> FiniteTimestamp {
        self.time_calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.panic_time.load(Ordering::SeqCst),
            "provider time panic"
        );
        FiniteTimestamp::from_microseconds(self.time.load(Ordering::SeqCst)).unwrap()
    }
    fn convert_timestamp_to_ntp_time(&self, timestamp: Timestamp) -> NtpTime {
        self.conversion_calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.panic_conversion.load(Ordering::SeqCst),
            "provider conversion panic"
        );
        self.seen.lock().unwrap().push(timestamp);
        NtpTime::from_parts(2_208_988_800, 0x8000_0000)
    }
}

#[test]
fn complete_custom_clock_methods_and_signed_environment_values() {
    let provider = Arc::new(Provider::new(-1_500));
    let clock = Clock::from_provider(provider.clone());
    assert_eq!(clock.time_in_microseconds(), -1_500);
    // Native exact negative halves round toward zero, not away from it.
    assert_eq!(clock.time_in_milliseconds(), -1);
    assert_eq!(
        clock.current_ntp_time().unwrap().raw(),
        NtpTime::from_parts(2_208_988_800, 0x8000_0000).raw()
    );
    assert_eq!(
        clock.current_ntp_in_milliseconds().unwrap(),
        2_208_988_800_500
    );
    clock
        .convert_timestamp_to_ntp_time(Timestamp::plus_infinity())
        .unwrap();
    clock
        .convert_timestamp_to_ntp_time(Timestamp::minus_infinity())
        .unwrap();
    assert!(
        provider
            .seen
            .lock()
            .unwrap()
            .contains(&Timestamp::plus_infinity())
    );
    assert!(
        provider
            .seen
            .lock()
            .unwrap()
            .contains(&Timestamp::minus_infinity())
    );
    assert_eq!(
        clock
            .convert_timestamp_to_ntp_time_in_milliseconds(-2)
            .unwrap(),
        2_208_988_800_500
    );
    assert_eq!(
        provider
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .microseconds()
            .unwrap(),
        -2_000
    );
    let calls = provider.conversion_calls.load(Ordering::SeqCst);
    assert_eq!(
        clock.convert_timestamp_to_ntp_time_in_milliseconds(i64::MAX),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(provider.conversion_calls.load(Ordering::SeqCst), calls);
    let environment = Environment::builder().clock_handle(&clock).build().unwrap();
    assert_eq!(environment.timestamp().microseconds().unwrap(), -1_500);
    assert_eq!(environment.try_now(), Err(TimeValueError::OutOfRange));
    provider.time.store(2_000, Ordering::SeqCst);
    assert_eq!(environment.now(), Duration::from_millis(2));
    for value in [i64::MIN + 1, i64::MAX - 1] {
        provider.time.store(value, Ordering::SeqCst);
        assert_eq!(clock.current_time().microseconds().unwrap(), value);
        assert!(clock.current_time().millis().is_ok());
        assert_eq!(
            clock.current_time().nanoseconds(),
            Err(TimeValueError::OutOfRange)
        );
    }
}

#[test]
fn native_environments_retain_provider_after_registration_handle_drop() {
    let provider = Arc::new(Provider::new(12_345));
    let weak = Arc::downgrade(&provider);
    let clock = Clock::from_provider(provider);
    let mut environment = Environment::builder().clock_handle(&clock).build().unwrap();
    let mut retained = environment.clone();
    drop(clock);
    assert!(weak.upgrade().is_some());
    environment.close();
    assert_eq!(retained.timestamp().microseconds().unwrap(), 12_345);
    retained.close();
    assert!(weak.upgrade().is_none());
}

#[test]
fn callback_panics_latch_without_reentering_provider() {
    let provider = Arc::new(Provider::new(321));
    let clock = Clock::from_provider(provider.clone());
    assert_eq!(clock.time_in_microseconds(), 321);
    provider.panic_time.store(true, Ordering::SeqCst);
    assert_eq!(clock.time_in_microseconds(), 321);
    assert!(clock.has_failed());
    let calls = provider.time_calls.load(Ordering::SeqCst);
    assert_eq!(clock.time_in_microseconds(), 321);
    assert_eq!(provider.time_calls.load(Ordering::SeqCst), calls);
    assert!(!clock.current_ntp_time().unwrap().is_valid());
    assert_eq!(provider.conversion_calls.load(Ordering::SeqCst), 0);

    let provider = Arc::new(Provider::new(654));
    let clock = Clock::from_provider(provider.clone());
    assert_eq!(clock.time_in_microseconds(), 654);
    provider.panic_conversion.store(true, Ordering::SeqCst);
    assert!(!clock.current_ntp_time().unwrap().is_valid());
    assert!(clock.has_failed());
    let calls = provider.time_calls.load(Ordering::SeqCst);
    assert_eq!(clock.time_in_microseconds(), 654);
    assert_eq!(provider.time_calls.load(Ordering::SeqCst), calls);
    assert!(!clock.current_ntp_time().unwrap().is_valid());
    assert_eq!(provider.conversion_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn cooperative_factory_requires_its_actual_manual_clock() {
    let manual = ManualClock::new(Duration::ZERO).unwrap();
    let queues = TaskQueueFactory::cooperative(&manual).unwrap();
    let clock = Clock::from_provider(Provider::new(0));
    assert!(matches!(
        Environment::builder()
            .task_queue_factory(&queues)
            .clock_handle(&clock)
            .build(),
        Err(pulsebeam_webrtc_sys::BuildEnvironmentError::MismatchedCooperativeClock)
    ));
    assert!(
        Environment::builder()
            .task_queue_factory(&queues)
            .clock_handle(&clock)
            .clock(&manual)
            .build()
            .is_ok()
    );
}

#[test]
fn local_and_migrating_actor_creation_use_selected_clock() {
    let provider = Arc::new(Provider::new(1_000_000));
    let clock = Clock::from_provider(provider.clone());
    let factory = PeerConnectionFactory::builder()
        .environment(Environment::builder().clock_handle(&clock).build().unwrap())
        .build()
        .unwrap();
    assert_eq!(factory.clock_timestamp().microseconds().unwrap(), 1_000_000);
    drop(factory);
    let mut actor = ProductionSession::builder(ProductionSessionConfig::default())
        .clock_handle(&clock)
        .build()
        .unwrap();
    assert_eq!(
        actor.clock_timestamp().unwrap().microseconds().unwrap(),
        1_000_000
    );
    provider.time.store(2_000_000, Ordering::SeqCst);
    actor = std::thread::spawn(move || {
        assert_eq!(
            actor.clock_timestamp().unwrap().microseconds().unwrap(),
            2_000_000
        );
        actor
            .create_peer(pulsebeam_webrtc_sys::PeerConfiguration::default())
            .unwrap();
        actor
    })
    .join()
    .unwrap();
    actor.shutdown();
    assert_eq!(clock.time_in_microseconds(), 2_000_000);
}
