//! Stock-only proof for the receive-time subtraction in NetEqImpl::ExtractPackets.
//! This checks a durable clock range, never an extra clock read or a clock freeze.
//! It neither alters a timestamp value nor removes packet metadata.
use crate::{ClockTimeBounds, Timestamp};

/// For every C in [minimum, maximum], prove INT64_MIN < C - R < INT64_MAX.
/// Subtraction is monotone in C, so endpoint checks suffice. Widen before
/// subtracting; the proof computation itself must not overflow signed i64.
/// Minus infinity skips the native statistics calculation. Plus infinity
/// reaches a non-finite TimeDelta::us() call and is never allowed for stock.
pub(crate) fn receive_time_is_safe(bounds: ClockTimeBounds, receive_time: Timestamp) -> bool {
    receive_time_bits_are_safe(
        bounds.minimum().microseconds(),
        bounds.maximum().microseconds(),
        receive_time.native_microsecond_bits(),
    )
}

// Shared with the guarded SDK product, so native virtual calls cannot bypass
// the Rust facade's receive-time proof. No clock sampling or callbacks occur.
pub(crate) fn receive_time_bits_are_safe(minimum: i64, maximum: i64, receive: i64) -> bool {
    if minimum == i64::MIN || maximum == i64::MAX || minimum > maximum {
        return false;
    }
    if receive == i64::MIN {
        return true;
    }
    if receive == i64::MAX {
        return false;
    }
    let minimum_delta = i128::from(minimum) - i128::from(receive);
    let maximum_delta = i128::from(maximum) - i128::from(receive);
    minimum_delta > i128::from(i64::MIN) && maximum_delta < i128::from(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FiniteTimestamp;

    fn bounds(minimum: i64, maximum: i64) -> ClockTimeBounds {
        ClockTimeBounds::new(
            FiniteTimestamp::from_microseconds(minimum).unwrap(),
            FiniteTimestamp::from_microseconds(maximum).unwrap(),
        )
        .unwrap()
    }
    fn timestamp(value: i64) -> Timestamp {
        Timestamp::from_microseconds(value).unwrap()
    }

    #[test]
    fn unrestricted_clock_allows_only_zero_or_absent_receive_time() {
        let bounds = ClockTimeBounds::all_finite();
        assert!(receive_time_is_safe(bounds, Timestamp::minus_infinity()));
        assert!(receive_time_is_safe(bounds, Timestamp::zero()));
        for time in [
            Timestamp::plus_infinity(),
            timestamp(i64::MIN + 1),
            timestamp(-1),
            timestamp(1),
            timestamp(i64::MAX - 1),
        ] {
            assert!(!receive_time_is_safe(bounds, time));
        }
    }

    #[test]
    fn finite_sentinel_collisions_are_rejected_without_narrow_overflow() {
        let positive = bounds(0, 100);
        assert!(!receive_time_is_safe(positive, timestamp(100 - i64::MAX)));
        assert!(receive_time_is_safe(positive, timestamp(101 - i64::MAX)));
        assert!(!receive_time_is_safe(positive, timestamp(i64::MIN + 1)));
        assert!(receive_time_is_safe(positive, timestamp(i64::MAX - 1)));
        let negative = bounds(-100, 0);
        assert!(!receive_time_is_safe(negative, timestamp(i64::MAX - 99)));
        assert!(receive_time_is_safe(negative, timestamp(i64::MAX - 100)));
        assert!(!receive_time_is_safe(negative, timestamp(i64::MAX - 1)));
        assert!(!receive_time_is_safe(negative, timestamp(i64::MIN + 1)));
        assert!(receive_time_is_safe(
            bounds(-100, -1),
            timestamp(i64::MIN + 1)
        ));
        assert!(receive_time_is_safe(negative, timestamp(i64::MIN + 2)));
    }

    #[test]
    fn durable_system_and_controlled_ranges_preserve_ordinary_signed_inputs() {
        let system = ClockTimeBounds::system_time();
        let controlled = bounds(0, crate::MAX_CONTROLLED_TIME.as_micros() as i64);
        for range in [system, controlled, bounds(-1_000_000_000, 1_000_000_000)] {
            for receive in [-1_000_000, 0, 1, 1_000_000, 1_800_000_000_000_000] {
                assert!(receive_time_is_safe(range, timestamp(receive)));
            }
            assert!(!receive_time_is_safe(range, timestamp(i64::MIN + 1)));
        }
        // At zero, MIN+1 subtracts to the +infinity sentinel, not a finite
        // TimeDelta. The next receive timestamp and the upper extreme are safe.
        let zero = bounds(0, 0);
        assert!(!receive_time_is_safe(zero, timestamp(i64::MIN + 1)));
        assert!(receive_time_is_safe(zero, timestamp(i64::MIN + 2)));
        assert!(receive_time_is_safe(zero, timestamp(i64::MAX - 1)));
    }

    #[test]
    fn endpoint_proof_matches_exhaustive_finite_domain_near_both_sentinels() {
        let ranges = [
            bounds(-10, 10),
            bounds(-10, -5),
            bounds(5, 10),
            bounds(i64::MIN + 1, i64::MIN + 5),
            bounds(i64::MAX - 5, i64::MAX - 1),
        ];
        let receives = [
            i64::MIN + 1,
            i64::MIN + 5,
            -11,
            -10,
            -1,
            0,
            1,
            10,
            11,
            i64::MAX - 5,
            i64::MAX - 1,
        ];
        for range in ranges {
            for receive in receives {
                let all_safe = (range.minimum().microseconds()..=range.maximum().microseconds())
                    .all(|current| {
                        let delta = i128::from(current) - i128::from(receive);
                        delta > i128::from(i64::MIN) && delta < i128::from(i64::MAX)
                    });
                assert_eq!(receive_time_is_safe(range, timestamp(receive)), all_safe);
            }
        }
    }
}
