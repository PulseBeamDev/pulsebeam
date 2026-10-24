use pulsebeam_webrtc_sys::{FiniteTimestamp, TimeDelta, TimeValueError, Timestamp};

fn ts(value: i64) -> Timestamp {
    Timestamp::from_microseconds(value).unwrap()
}
fn delta(value: i64) -> TimeDelta {
    TimeDelta::from_microseconds(value).unwrap()
}

#[test]
fn signed_native_constructors_states_and_rounding() {
    for value in [i64::MIN + 1, -1_500, -1, 0, 1, 1_500, i64::MAX - 1] {
        let time = ts(value);
        assert_eq!(time.microseconds().unwrap(), value);
        assert_eq!(
            FiniteTimestamp::try_from(time).unwrap().microseconds(),
            value
        );
        assert_eq!(delta(value).microseconds().unwrap(), value);
        assert_eq!(time.to_string(), delta(value).to_string());
    }
    for value in [i64::MIN, i64::MAX] {
        assert_eq!(
            Timestamp::from_microseconds(value),
            Err(TimeValueError::OutOfRange)
        );
        let time = Timestamp::from_native_microsecond_bits(value);
        assert!(time.is_infinite());
        assert_eq!(time.microseconds(), Err(TimeValueError::InfiniteValue));
        assert_eq!(time.microseconds_or(42), 42);
        assert_eq!(time.millis_or(42), 42);
        assert_eq!(time.seconds_or(42), 42);
        assert_eq!(
            FiniteTimestamp::try_from(time),
            Err(TimeValueError::InfiniteValue)
        );
    }
    assert_eq!(ts(1_500).millis().unwrap(), 2);
    assert_eq!(ts(-1_500).millis().unwrap(), -1);
    assert_eq!(ts(-1_501).millis().unwrap(), -2);
    assert_eq!(ts(1_500_000).seconds().unwrap(), 2);
    assert_eq!(ts(-1_500_000).seconds().unwrap(), -1);
    assert_eq!(
        Timestamp::from_millis(i64::MIN / 1_000),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(
        Timestamp::from_millis(i64::MAX / 1_000),
        Err(TimeValueError::OutOfRange)
    );
    // Constructor strict limits are not the same as representable raw states.
    assert_eq!(
        ts((i64::MIN / 1_000) * 1_000).millis().unwrap(),
        i64::MIN / 1_000
    );
    assert_eq!(
        ts(i64::MAX / 1_000).nanoseconds().unwrap(),
        (i64::MAX / 1_000) * 1_000
    );
    assert_eq!(
        ts(i64::MAX / 1_000 + 1).nanoseconds(),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(
        ts(i64::MIN / 1_000 - 1).nanoseconds(),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(
        TimeDelta::from_minutes(2).unwrap(),
        TimeDelta::from_seconds(120).unwrap()
    );
    assert_eq!(
        TimeDelta::from_minutes(i64::MAX),
        Err(TimeValueError::OutOfRange)
    );
}

#[test]
fn floating_constructors_preserve_precision_truncation_and_infinity() {
    assert_eq!(Timestamp::from_microseconds_f64(1.9).unwrap(), ts(1));
    assert_eq!(Timestamp::from_microseconds_f64(-1.9).unwrap(), ts(-1));
    assert_eq!(Timestamp::from_millis_f64(-1.2345).unwrap(), ts(-1_234));
    assert_eq!(
        Timestamp::from_microseconds_f64(f64::NAN),
        Err(TimeValueError::NotANumber)
    );
    assert_eq!(
        Timestamp::from_microseconds_f32(f32::NAN),
        Err(TimeValueError::NotANumber)
    );
    assert!(
        Timestamp::from_seconds_f64(f64::MAX)
            .unwrap()
            .is_plus_infinity()
    );
    assert!(
        Timestamp::from_seconds_f32(-f32::MAX)
            .unwrap()
            .is_minus_infinity()
    );
    assert_eq!(
        Timestamp::from_microseconds_f64(2.0_f64.powi(63)),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(
        Timestamp::from_microseconds_f64(-2.0_f64.powi(63)),
        Err(TimeValueError::OutOfRange)
    );
    assert!(Timestamp::plus_infinity().nanoseconds_f64().is_infinite());
    assert_eq!(Timestamp::minus_infinity().seconds_f32(), f32::NEG_INFINITY);
    // A native float multiplication is not widening-first double arithmetic.
    let input = 123_456.789_f32;
    assert_eq!(
        Timestamp::from_seconds_f32(input)
            .unwrap()
            .microseconds()
            .unwrap(),
        (input * 1_000_000.0_f32) as i64
    );
    assert_eq!(
        TimeDelta::from_seconds_f32(input)
            .unwrap()
            .microseconds()
            .unwrap(),
        (input * 1_000_000.0_f32) as i64
    );
}

#[test]
fn relative_operations_preserve_infinities_and_reject_native_undefined_domains() {
    assert_eq!(ts(10).checked_add(delta(-20)).unwrap(), ts(-10));
    assert_eq!(ts(-10).duration_since(ts(20)).unwrap(), delta(-30));
    assert_eq!(
        Timestamp::plus_infinity()
            .duration_since(Timestamp::minus_infinity())
            .unwrap(),
        TimeDelta::plus_infinity()
    );
    assert_eq!(
        Timestamp::plus_infinity().duration_since(Timestamp::plus_infinity()),
        Err(TimeValueError::IndeterminateArithmetic)
    );
    assert_eq!(
        TimeDelta::plus_infinity().checked_add(TimeDelta::minus_infinity()),
        Err(TimeValueError::IndeterminateArithmetic)
    );
    assert_eq!(
        TimeDelta::minus_infinity().checked_sub(TimeDelta::minus_infinity()),
        Err(TimeValueError::IndeterminateArithmetic)
    );
    assert_eq!(
        TimeDelta::plus_infinity().checked_sub(delta(1)).unwrap(),
        TimeDelta::plus_infinity()
    );
    assert_eq!(
        ts(i64::MAX - 1).checked_add(delta(1)),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(
        delta(i64::MIN + 1).checked_sub(delta(1)),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(
        delta(i64::MIN + 1).checked_neg(),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(delta(i64::MIN + 1).abs(), Err(TimeValueError::OutOfRange));
    assert_eq!(
        TimeDelta::plus_infinity().abs(),
        Err(TimeValueError::InfiniteValue)
    );
    assert_eq!(
        TimeDelta::plus_infinity().checked_neg().unwrap(),
        TimeDelta::minus_infinity()
    );
    assert_eq!(delta(-11).abs().unwrap(), delta(11));
    let mut time = ts(i64::MAX - 1);
    assert_eq!(
        time.checked_add_assign(delta(1)),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(time, ts(i64::MAX - 1));
}

#[test]
fn native_scalar_and_resolution_semantics() {
    assert_eq!(delta(-11).round_down_to(delta(10)).unwrap(), delta(-10));
    assert_eq!(delta(-11).round_up_to(delta(10)).unwrap(), delta(0));
    assert_eq!(delta(-11).round_to(delta(10)).unwrap(), delta(0));
    assert_eq!(delta(15).round_to(delta(10)).unwrap(), delta(20));
    assert_eq!(
        delta(1).round_to(TimeDelta::zero()),
        Err(TimeValueError::InvalidResolution)
    );
    assert_eq!(
        delta(1).round_to(TimeDelta::plus_infinity()),
        Err(TimeValueError::InvalidResolution)
    );
    assert_eq!(
        delta(i64::MAX - 1).round_up_to(delta(2)),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(delta(7).checked_div_i64(2).unwrap(), delta(3));
    assert_eq!(delta(7).checked_div_f64(2.0).unwrap(), delta(4));
    assert_eq!(delta(-7).checked_div_f32(2.0).unwrap(), delta(-4));
    assert_eq!(delta(-3).checked_mul_f64(0.5).unwrap(), delta(-2));
    assert_eq!(
        delta(1).checked_div_i64(0),
        Err(TimeValueError::DivisionByZero)
    );
    assert_eq!(
        delta(1).checked_div_f64(0.0),
        Err(TimeValueError::DivisionByZero)
    );
    assert_eq!(
        delta(1).checked_mul_f64(f64::NAN),
        Err(TimeValueError::NotANumber)
    );
    assert_eq!(
        delta(1).checked_mul_f64(f64::INFINITY),
        Err(TimeValueError::OutOfRange)
    );
    assert_eq!(
        delta(1).checked_div_f64(f64::INFINITY).unwrap(),
        TimeDelta::zero()
    );
    assert_eq!(
        TimeDelta::plus_infinity().checked_mul_i64(0),
        Err(TimeValueError::InfiniteValue)
    );
    assert!(TimeDelta::zero().ratio(TimeDelta::zero()).is_nan());
    assert!(
        TimeDelta::plus_infinity()
            .ratio(TimeDelta::plus_infinity())
            .is_nan()
    );
    assert_eq!(delta(1).ratio(TimeDelta::zero()), f64::INFINITY);
    if usize::BITS == 64 {
        assert_eq!(delta(-1).checked_mul_usize(usize::MAX).unwrap(), delta(1));
        assert_eq!(
            delta(-1)
                .checked_div_usize(3)
                .unwrap()
                .microseconds()
                .unwrap(),
            (u64::MAX / 3) as i64
        );
        assert_eq!(
            delta(-1).checked_mul_usize(1),
            Err(TimeValueError::OutOfRange)
        );
    }
}
