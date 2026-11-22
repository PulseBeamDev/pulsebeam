use crate::{
    Error,
    deadline::*,
    domain::Quality,
    time::{GlobalDuration, GlobalInstant},
};

fn time(center: i64, radius: u64) -> Timing {
    Timing {
        instant: GlobalInstant::from_nanos(center),
        absolute_uncertainty_ns: Some(radius),
        quality: Quality::Synchronized,
    }
}

fn interval(lo: i64, hi: i64) -> Option<Interval> {
    Some(Interval {
        earliest: GlobalDuration::from_nanos(lo),
        latest: GlobalDuration::from_nanos(hi),
    })
}

#[test]
fn independent_targets_use_both_clocks_uncertainty() {
    let capture = time(10, 7);
    let now = time(70, 8);
    let tight = evaluate(capture, now, GlobalDuration::from_nanos(40)).unwrap();
    let loose = evaluate(capture, now, GlobalDuration::from_nanos(90)).unwrap();
    assert_eq!(tight.age, interval(45, 75));
    assert_eq!(loose.age, tight.age);
    assert_eq!(tight.remaining, interval(-35, -5));
    assert_eq!(loose.remaining, interval(15, 45));
    assert_eq!(tight.expiry, Expiry::Definite);
    assert_eq!(loose.expiry, Expiry::Inconclusive);
    for (target, remaining) in [(45, interval(-30, 0)), (50, interval(-25, 5))] {
        let result = evaluate(capture, now, GlobalDuration::from_nanos(target)).unwrap();
        assert_eq!(result.remaining, remaining);
        assert_eq!(result.expiry, Expiry::Inconclusive);
    }
}

#[test]
fn future_capture_weak_and_unknown_are_not_expiry() {
    let target = GlobalDuration::from_nanos(0);
    let now = time(100, 5);
    let ahead = evaluate(time(106, 3), now, target).unwrap();
    assert_eq!(ahead.age, interval(-14, 2));
    assert_eq!(ahead.remaining, interval(-2, 14));
    assert_eq!(ahead.expiry, Expiry::Inconclusive);
    for quality in [
        Quality::Acquiring,
        Quality::Degraded,
        Quality::Holdover,
        Quality::Recovering,
    ] {
        let weak = Timing { quality, ..now };
        let result = evaluate(time(20, 1), weak, target).unwrap();
        assert_eq!(result.remaining, interval(-86, -74));
        assert_eq!(result.expiry, Expiry::Inconclusive);
    }
    let unknown = Timing {
        absolute_uncertainty_ns: None,
        ..now
    };
    assert_eq!(
        evaluate(time(20, 1), unknown, target),
        Ok(DeadlineEvidence {
            age: None,
            remaining: None,
            expiry: Expiry::Inconclusive,
        })
    );
}

#[test]
fn unrepresentable_results_cannot_authorize_expiry() {
    let zero = GlobalDuration::from_nanos(0);
    assert_eq!(
        evaluate(time(i64::MIN, 0), time(i64::MAX, 0), zero),
        Err(Error::Overflow)
    );
    assert_eq!(
        evaluate(time(0, u64::MAX), time(1, u64::MAX), zero),
        Err(Error::Overflow)
    );
    assert_eq!(
        evaluate(time(0, 1), time(0, 1), GlobalDuration::from_nanos(i64::MAX)),
        Err(Error::Overflow)
    );
    assert_eq!(
        evaluate(time(0, 1), time(10, 1), GlobalDuration::from_nanos(-1)),
        Err(Error::Invalid)
    );
}
