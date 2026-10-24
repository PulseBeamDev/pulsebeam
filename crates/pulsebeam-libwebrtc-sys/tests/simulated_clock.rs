use pulsebeam_webrtc_sys::{
    Environment, NtpTime, ProductionSession, ProductionSessionConfig, SimulatedClock, TimeDelta,
    TimeValueError, Timestamp,
};

#[test]
fn signed_time_and_wrapping_advances_preserve_native_finite_states() {
    let clock = SimulatedClock::from_microseconds(-1_500).unwrap();
    assert_eq!(clock.time_in_microseconds(), -1_500);
    assert_eq!(clock.time_in_milliseconds(), -1);
    clock.advance_milliseconds(-2).unwrap();
    assert_eq!(clock.time_in_microseconds(), -3_500);
    clock
        .advance(TimeDelta::from_microseconds(4_000).unwrap())
        .unwrap();
    assert_eq!(clock.time_in_microseconds(), 500);
    assert_eq!(clock.time_in_milliseconds(), 1);

    let clock = SimulatedClock::from_microseconds(i64::MAX - 1).unwrap();
    for delta in [1, 2] {
        assert_eq!(
            clock.advance_microseconds(delta),
            Err(TimeValueError::OutOfRange)
        );
        assert_eq!(clock.time_in_microseconds(), i64::MAX - 1);
    }
    clock.advance_microseconds(3).unwrap();
    assert_eq!(clock.time_in_microseconds(), i64::MIN + 1);
    for delta in [-1, -2] {
        assert_eq!(
            clock.advance_microseconds(delta),
            Err(TimeValueError::OutOfRange)
        );
        assert_eq!(clock.time_in_microseconds(), i64::MIN + 1);
    }
    clock.advance_microseconds(-3).unwrap();
    assert_eq!(clock.time_in_microseconds(), i64::MAX - 1);
    assert_eq!(
        clock.advance(TimeDelta::plus_infinity()),
        Err(TimeValueError::InfiniteValue)
    );
    assert_eq!(
        clock.advance(TimeDelta::minus_infinity()),
        Err(TimeValueError::InfiniteValue)
    );
    assert_eq!(
        clock.advance_milliseconds(i64::MAX),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(clock.time_in_microseconds(), i64::MAX - 1);
    assert!(matches!(
        SimulatedClock::new(Timestamp::plus_infinity()),
        Err(TimeValueError::InfiniteValue)
    ));
    assert!(matches!(
        SimulatedClock::new(Timestamp::minus_infinity()),
        Err(TimeValueError::InfiniteValue)
    ));
    assert!(matches!(
        SimulatedClock::from_microseconds(i64::MAX),
        Err(TimeValueError::OutOfRange)
    ));
    assert!(matches!(
        SimulatedClock::from_microseconds(i64::MIN),
        Err(TimeValueError::OutOfRange)
    ));
}

#[test]
fn ntp_conversion_retains_native_seconds_era_wrap_and_finite_unit_contract() {
    let clock = SimulatedClock::from_microseconds(500_000).unwrap();
    assert_eq!(
        clock.current_ntp_time().unwrap(),
        NtpTime::from_parts(2_208_988_800, 0x8000_0000)
    );
    assert_eq!(
        clock.current_ntp_in_milliseconds().unwrap(),
        2_208_988_800_500
    );
    assert_eq!(
        clock
            .convert_timestamp_to_ntp_time(Timestamp::from_microseconds(-1_000_000).unwrap())
            .unwrap(),
        NtpTime::from_parts(2_208_988_799, 0)
    );
    assert_eq!(
        clock
            .convert_timestamp_to_ntp_time_in_milliseconds(-1_000)
            .unwrap(),
        2_208_988_799_000
    );
    let era_boundary = (u32::MAX as i64 + 1 - 2_208_988_800) * 1_000_000;
    assert_eq!(
        clock
            .convert_timestamp_to_ntp_time(Timestamp::from_microseconds(era_boundary).unwrap())
            .unwrap(),
        NtpTime::from_raw(0)
    );
    assert_eq!(
        clock
            .convert_timestamp_to_ntp_time(
                Timestamp::from_microseconds(era_boundary + 500_000).unwrap()
            )
            .unwrap(),
        NtpTime::from_parts(0, 0x8000_0000)
    );
    // A negative integral UTC second may wrap the unsigned NTP epoch too.
    let before_ntp_epoch = -(2_208_988_800 + 1) * 1_000_000i64;
    assert_eq!(
        clock
            .convert_timestamp_to_ntp_time(Timestamp::from_microseconds(before_ntp_epoch).unwrap())
            .unwrap(),
        NtpTime::from_parts(u32::MAX, 0)
    );
    for timestamp in [Timestamp::plus_infinity(), Timestamp::minus_infinity()] {
        assert_eq!(
            clock.convert_timestamp_to_ntp_time(timestamp),
            Err(TimeValueError::InfiniteValue)
        );
    }
    clock.advance_microseconds(-1_500_000).unwrap();
    assert_eq!(clock.time_in_microseconds(), -1_000_000);
    assert_eq!(
        clock.current_ntp_time().unwrap(),
        NtpTime::from_parts(2_208_988_799, 0)
    );
    assert_eq!(
        clock.current_ntp_in_milliseconds().unwrap(),
        2_208_988_799_000
    );
}

#[test]
fn cloned_advances_are_shared_and_concurrent() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SimulatedClock>();
    let clock = SimulatedClock::from_microseconds(0).unwrap();
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let clock = clock.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    clock.advance_microseconds(1).unwrap();
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(clock.time_in_microseconds(), 400);
}

#[test]
fn environment_and_migrating_actor_retain_the_same_native_clock() {
    let clock = SimulatedClock::from_microseconds(1_000_000).unwrap();
    let environment = Environment::builder()
        .clock_handle(clock.clock())
        .build()
        .unwrap();
    let mut actor = ProductionSession::builder(ProductionSessionConfig::default())
        .clock_handle(clock.clock())
        .build()
        .unwrap();
    clock.advance_microseconds(1_000).unwrap();
    assert_eq!(environment.timestamp().microseconds().unwrap(), 1_001_000);
    actor = std::thread::spawn(move || {
        assert_eq!(
            actor.clock_timestamp().unwrap().microseconds().unwrap(),
            1_001_000
        );
        actor
            .create_peer(pulsebeam_webrtc_sys::PeerConfiguration::default())
            .unwrap();
        actor
    })
    .join()
    .unwrap();
    let retained = clock.clock().clone();
    drop(clock);
    assert_eq!(environment.timestamp().microseconds().unwrap(), 1_001_000);
    actor.shutdown().unwrap();
    assert_eq!(retained.time_in_microseconds(), 1_001_000);
}
