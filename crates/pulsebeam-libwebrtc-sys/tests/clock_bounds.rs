use pulsebeam_webrtc_sys::{
    Clock, ClockProvider, ClockTimeBounds, Environment, FiniteTimestamp, MAX_CONTROLLED_TIME,
    ManualClock, NtpTime, SimulatedClock, TimeValueError, Timestamp,
};
use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};

fn bounds(minimum: i64, maximum: i64) -> ClockTimeBounds {
    ClockTimeBounds::new(
        FiniteTimestamp::from_microseconds(minimum).unwrap(),
        FiniteTimestamp::from_microseconds(maximum).unwrap(),
    )
    .unwrap()
}
struct Provider {
    time: AtomicI64,
    calls: AtomicUsize,
}
impl ClockProvider for Provider {
    fn current_time(&self) -> FiniteTimestamp {
        self.calls.fetch_add(1, Ordering::SeqCst);
        FiniteTimestamp::from_microseconds(self.time.load(Ordering::SeqCst)).unwrap()
    }
    fn convert_timestamp_to_ntp_time(&self, timestamp: Timestamp) -> NtpTime {
        NtpTime::from_raw(timestamp.native_microsecond_bits() as u64)
    }
}

#[test]
fn bounds_are_validated_and_existing_unrestricted_provider_values_are_preserved() {
    assert_eq!(
        ClockTimeBounds::new(
            FiniteTimestamp::from_microseconds(1).unwrap(),
            FiniteTimestamp::from_microseconds(-1).unwrap()
        ),
        Err(TimeValueError::OutOfRange)
    );
    let provider = Arc::new(Provider {
        time: AtomicI64::new(0),
        calls: AtomicUsize::new(0),
    });
    let clock = Clock::from_provider(provider.clone());
    assert_eq!(clock.time_bounds(), ClockTimeBounds::all_finite());
    for time in [i64::MIN + 1, -1, 0, 1, i64::MAX - 1] {
        provider.time.store(time, Ordering::SeqCst);
        assert_eq!(clock.time_in_microseconds(), time);
        assert!(
            clock
                .time_bounds()
                .contains(clock.current_time().finite().unwrap())
        );
    }
    assert!(!clock.has_failed());
}

#[test]
fn bounded_provider_retains_native_ownership_and_latches_out_of_range_reads() {
    let provider = Arc::new(Provider {
        time: AtomicI64::new(20),
        calls: AtomicUsize::new(0),
    });
    let weak = Arc::downgrade(&provider);
    let clock = Clock::from_bounded_provider(provider.clone(), bounds(10, 30));
    let mut environment = Environment::builder().clock_handle(&clock).build().unwrap();
    let mut retained = environment.clone();
    assert_eq!(retained.clock_time_bounds(), bounds(10, 30));
    assert_eq!(retained.timestamp().microseconds().unwrap(), 20);
    // Bounds govern current time only. Conversion receives unchanged inputs.
    for time in [
        Timestamp::minus_infinity(),
        Timestamp::plus_infinity(),
        Timestamp::from_microseconds(-100).unwrap(),
    ] {
        assert_eq!(
            clock.convert_timestamp_to_ntp_time(time).unwrap().raw(),
            time.native_microsecond_bits() as u64
        );
    }
    provider.time.store(31, Ordering::SeqCst);
    assert_eq!(retained.timestamp().microseconds().unwrap(), 20);
    assert!(clock.has_failed());
    let calls = provider.calls.load(Ordering::SeqCst);
    provider.time.store(25, Ordering::SeqCst);
    assert_eq!(clock.time_in_microseconds(), 20);
    assert_eq!(provider.calls.load(Ordering::SeqCst), calls);
    assert!(!clock.current_ntp_time().unwrap().is_valid());
    drop(provider);
    drop(clock);
    environment.close();
    assert!(weak.upgrade().is_some());
    assert_eq!(retained.timestamp().microseconds().unwrap(), 20);
    assert_eq!(retained.clock_time_bounds(), bounds(10, 30));
    retained.close();
    assert!(weak.upgrade().is_none());
}

#[test]
fn failure_before_first_success_always_uses_an_in_range_fallback() {
    struct PanicProvider;
    impl ClockProvider for PanicProvider {
        fn current_time(&self) -> FiniteTimestamp {
            panic!("expected callback panic")
        }
        fn convert_timestamp_to_ntp_time(&self, _: Timestamp) -> NtpTime {
            panic!("expected callback panic")
        }
    }
    for (range, fallback) in [
        (bounds(10, 30), 10),
        (bounds(-30, -10), -10),
        (bounds(-10, 10), 0),
    ] {
        let panic_clock = Clock::from_bounded_provider(PanicProvider, range);
        assert_eq!(panic_clock.time_in_microseconds(), fallback);
        assert!(panic_clock.has_failed());
        let conversion_clock = Clock::from_bounded_provider(PanicProvider, range);
        assert!(
            !conversion_clock
                .convert_timestamp_to_ntp_time(Timestamp::zero())
                .unwrap()
                .is_valid()
        );
        assert_eq!(conversion_clock.time_in_microseconds(), fallback);
        let out_of_range = Clock::from_bounded_provider(
            Provider {
                time: AtomicI64::new(100),
                calls: AtomicUsize::new(0),
            },
            range,
        );
        assert_eq!(out_of_range.time_in_microseconds(), fallback);
        assert!(out_of_range.has_failed());
    }
}

#[test]
fn already_entered_callbacks_cannot_publish_out_of_range_time_after_failure() {
    struct RacingProvider {
        calls: AtomicUsize,
        entered: Barrier,
        release: Barrier,
    }
    impl ClockProvider for RacingProvider {
        fn current_time(&self) -> FiniteTimestamp {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.entered.wait();
                self.release.wait();
                FiniteTimestamp::from_microseconds(23).unwrap()
            } else {
                FiniteTimestamp::from_microseconds(31).unwrap()
            }
        }
        fn convert_timestamp_to_ntp_time(&self, _: Timestamp) -> NtpTime {
            NtpTime::from_raw(0)
        }
    }
    let provider = Arc::new(RacingProvider {
        calls: AtomicUsize::new(0),
        entered: Barrier::new(2),
        release: Barrier::new(2),
    });
    let clock = Clock::from_bounded_provider(provider.clone(), bounds(17, 30));
    let concurrent = clock.clone();
    let reader = std::thread::spawn(move || concurrent.time_in_microseconds());
    provider.entered.wait();
    assert_eq!(clock.time_in_microseconds(), 17);
    assert!(clock.has_failed());
    provider.release.wait();
    assert_eq!(reader.join().unwrap(), 23);
    assert_eq!(clock.time_in_microseconds(), 23);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn bounded_simulated_advances_and_environment_copies_keep_the_durable_range() {
    let range = bounds(-100, 100);
    let clock = SimulatedClock::bounded(Timestamp::zero(), range).unwrap();
    let environment = Environment::builder()
        .clock_handle(clock.clock())
        .build()
        .unwrap();
    let mut retained = environment.clone();
    assert_eq!(clock.time_bounds(), range);
    assert_eq!(retained.clock_time_bounds(), range);
    clock.advance_microseconds(100).unwrap();
    assert_eq!(
        clock.advance_microseconds(1),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(clock.time_in_microseconds(), 100);
    clock.advance_microseconds(-200).unwrap();
    assert_eq!(
        clock.advance_microseconds(-1),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(clock.time_in_microseconds(), -100);
    assert!(matches!(
        SimulatedClock::bounded(Timestamp::from_microseconds(101).unwrap(), range),
        Err(TimeValueError::OutOfRange)
    ));
    assert!(matches!(
        SimulatedClock::bounded(Timestamp::plus_infinity(), range),
        Err(TimeValueError::InfiniteValue)
    ));
    // Opt-in current-time bounds do not narrow timestamp-to-NTP inputs.
    assert!(
        clock
            .convert_timestamp_to_ntp_time(Timestamp::from_microseconds(-1_000_000).unwrap())
            .is_ok()
    );
    drop(clock);
    assert_eq!(retained.timestamp().microseconds().unwrap(), -100);
    retained.close();
}

#[test]
fn bounded_simulated_concurrent_advances_check_and_mutate_atomically() {
    let clock = SimulatedClock::bounded(Timestamp::zero(), bounds(0, 100)).unwrap();
    let successes = Arc::new(AtomicUsize::new(0));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let clock = clock.clone();
            let successes = successes.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    if clock.advance_microseconds(1).is_ok() {
                        successes.fetch_add(1, Ordering::SeqCst);
                    }
                    assert!(
                        clock
                            .time_bounds()
                            .contains(clock.current_time().finite().unwrap())
                    );
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(successes.load(Ordering::SeqCst), 100);
    assert_eq!(clock.time_in_microseconds(), 100);
}

#[test]
fn manual_and_default_environment_bounds_do_not_depend_on_a_clock_sample() {
    let manual = ManualClock::new(Duration::ZERO).unwrap();
    let range = bounds(0, MAX_CONTROLLED_TIME.as_micros() as i64);
    assert_eq!(manual.time_bounds(), range);
    let environment = Environment::builder().clock(&manual).build().unwrap();
    manual.advance(MAX_CONTROLLED_TIME).unwrap();
    assert_eq!(environment.clock_time_bounds(), range);
    assert_eq!(
        environment.timestamp().microseconds().unwrap(),
        range.maximum().microseconds()
    );
    assert_eq!(
        manual.advance(Duration::from_micros(1)),
        Err(pulsebeam_webrtc_sys::BuildEnvironmentError::TimestampOutOfRange)
    );
    assert_eq!(
        Environment::builder().build().unwrap().clock_time_bounds(),
        bounds(i64::MIN / 1000, i64::MAX / 1000)
    );
}
