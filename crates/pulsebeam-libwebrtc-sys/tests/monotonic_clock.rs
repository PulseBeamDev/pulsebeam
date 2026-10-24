use pulsebeam_webrtc_sys::{
    Clock, ClockProvider, ClockTimeBounds, Environment, FiniteTimestamp, ManualClock, NtpTime,
    SimulatedClock, Timestamp,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, Ordering},
    mpsc,
};

fn bounds(minimum: i64, maximum: i64) -> ClockTimeBounds {
    ClockTimeBounds::new(
        FiniteTimestamp::from_microseconds(minimum).unwrap(),
        FiniteTimestamp::from_microseconds(maximum).unwrap(),
    )
    .unwrap()
}
struct Provider(AtomicI64);
impl ClockProvider for Provider {
    fn current_time(&self) -> FiniteTimestamp {
        FiniteTimestamp::from_microseconds(self.0.load(Ordering::SeqCst)).unwrap()
    }
    fn convert_timestamp_to_ntp_time(&self, timestamp: Timestamp) -> NtpTime {
        NtpTime::from_raw(timestamp.native_microsecond_bits() as u64)
    }
}

#[test]
fn first_negative_time_is_preserved_and_backwards_time_latches_across_native_copies() {
    let provider = Arc::new(Provider(AtomicI64::new(-10)));
    let clock = Clock::from_monotonic_bounded_provider(provider.clone(), bounds(-100, 100));
    assert!(clock.is_monotonic());
    assert_eq!(clock.time_in_microseconds(), -10);
    let mut environment = Environment::builder().clock_handle(&clock).build().unwrap();
    let retained = environment.clone();
    assert!(retained.clock_is_monotonic());
    provider.0.store(-9, Ordering::SeqCst);
    assert_eq!(retained.timestamp().native_microsecond_bits(), -9);
    provider.0.store(-10, Ordering::SeqCst);
    assert_eq!(environment.timestamp().native_microsecond_bits(), -9);
    assert!(clock.has_failed());
    environment.close();
    drop(environment);
    drop(clock);
    provider.0.store(100, Ordering::SeqCst);
    assert_eq!(retained.timestamp().native_microsecond_bits(), -9);
    assert!(retained.clock_is_monotonic());
}

#[test]
fn existing_rewindable_clocks_do_not_acquire_monotonic_evidence() {
    let clock = Clock::from_bounded_provider(Provider(AtomicI64::new(0)), bounds(-100, 100));
    assert!(!clock.is_monotonic());
    let environment = Environment::builder().clock_handle(&clock).build().unwrap();
    assert!(!environment.clock_is_monotonic());
    let simulated = SimulatedClock::new(Timestamp::zero()).unwrap();
    assert!(!simulated.clock().is_monotonic());
    let manual = ManualClock::new(std::time::Duration::ZERO).unwrap();
    assert!(
        Environment::builder()
            .clock(&manual)
            .build()
            .unwrap()
            .clock_is_monotonic()
    );
    assert!(!Environment::builder().build().unwrap().clock_is_monotonic());
}

#[test]
fn ntp_failure_cannot_publish_fallback_before_an_already_entered_negative_read() {
    struct Blocking {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl ClockProvider for Blocking {
        fn current_time(&self) -> FiniteTimestamp {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            FiniteTimestamp::from_microseconds(-10).unwrap()
        }
        fn convert_timestamp_to_ntp_time(&self, _: Timestamp) -> NtpTime {
            panic!("NTP provider failure")
        }
    }
    let (entered, observed) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let clock = Clock::from_monotonic_bounded_provider(
        Blocking {
            entered,
            release: Mutex::new(wait),
        },
        bounds(-100, 100),
    );
    let copy = clock.clone();
    let thread = std::thread::spawn(move || copy.time_in_microseconds());
    observed.recv().unwrap();
    assert!(
        !clock
            .convert_timestamp_to_ntp_time(Timestamp::zero())
            .unwrap()
            .is_valid()
    );
    assert!(clock.has_failed());
    assert_eq!(clock.time_in_microseconds(), 0);
    release.send(()).unwrap();
    assert_eq!(thread.join().unwrap(), 0);
    assert_eq!(clock.time_in_microseconds(), 0);
}
