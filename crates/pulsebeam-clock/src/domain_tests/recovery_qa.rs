#![allow(
    clippy::arithmetic_side_effects,
    reason = "fixed test inputs are in range; overflow is a test failure"
)]

use super::{boundary_witnesses::exact_exchange, *};
use crate::deadline::{DeadlineEvidence, Expiry, Timing, evaluate};

const ORIGIN: i64 = 100_000_000_000;
const PATH: u64 = 30_000_000;
const LAST_SEED: i64 = 1_200_000_000;

fn seeded() -> ClockDomain {
    let mut clock = domain(79);
    for source in [1_000_000_000, LAST_SEED] {
        let e = exact_exchange(&clock, source, ORIGIN + source, PATH);
        clock.observe_exchange(e, e.source, e.reference.at).unwrap();
    }
    clock
}

fn capture(clock: &ClockDomain, source: i64) -> DataCapture {
    DataCapture {
        binding: clock.binding(),
        source: SourceTime::from_nanos(source),
        uncertainty_ns: 0,
    }
}

fn project(clock: &ClockDomain, source: i64, now: MonotonicTime) -> Result<Projection, Error> {
    clock.project_data(Some(capture(clock, source)), receipt(now), now)
}

fn zero_target(projected: Projection, now: MonotonicTime) -> DeadlineEvidence {
    evaluate(
        projected.into(),
        Timing {
            instant: receipt(now).latest_global.unwrap(),
            absolute_uncertainty_ns: Some(0),
            quality: Quality::Synchronized,
        },
        GlobalDuration::from_nanos(0),
    )
    .unwrap()
}

fn recover_at_old_future_limit(extra_source_ns: i64) {
    let mut clock = seeded();
    // After the nominal-rate seed, run at -1000 ppm for 130 source seconds.
    // The old map leads truth by 130 ms, or 100 ms at receipt. An extra
    // microsecond at that rate makes the extrapolation 1 ns too far ahead.
    let span = 130_000_000_000 + extra_source_ns;
    let source = LAST_SEED + span;
    let truth = ORIGIN + source - span / 1_000;
    let e = exact_exchange(&clock, source, truth, PATH);
    let now = e.reference.at;
    let retained_source = source - 1_000_000_000;
    let held = project(&clock, retained_source, now).unwrap();
    assert_eq!(held.quality, Quality::Holdover);
    let held_deadline = zero_target(held, now);
    assert!(held_deadline.remaining.unwrap().latest.as_nanos() < 0);
    assert_eq!(held_deadline.expiry, Expiry::Inconclusive);

    let estimate_lead = ORIGIN + source - e.reference.global.as_nanos();
    assert_eq!(
        estimate_lead,
        i64::try_from(CONFIG.max_future_ns).unwrap() + extra_source_ns / 1_000
    );
    let frontier = project(&clock, source, now);
    if extra_source_ns == 0 {
        assert_eq!(frontier.unwrap().quality, Quality::Holdover);
    } else {
        assert_eq!(frontier, Err(Error::OutOfWindow));
        let mut weak = e;
        weak.reference.quality = ReferenceQuality::Qualified {
            uncertainty_ns: CONFIG.lock_uncertainty_ns + 1,
        };
        assert_eq!(
            clock.observe_exchange(weak, weak.source, now),
            Err(Error::OutOfWindow)
        );
        // A precision refresh cannot install an out-of-window target.
        assert_eq!(
            clock.observe_exchange(e, SourceTime::from_nanos(source + 200_000_000), now),
            Err(Error::OutOfWindow)
        );
        assert_eq!(project(&clock, retained_source, now), Ok(held));
    }
    assert_eq!(
        clock.observe_exchange(e, e.source, now),
        Ok(()),
        "an exact in-window exchange at a legal sealed frontier must recover\n\
         even if the old holdover extrapolation leads receipt by {estimate_lead} ns"
    );
    let recovering = project(&clock, retained_source, now).unwrap();
    assert_eq!(recovering.global, held.global);
    assert_eq!(recovering.quality, Quality::Recovering);
    assert_eq!(zero_target(recovering, now).expiry, Expiry::Inconclusive);
    if let Ok(held_frontier) = frontier {
        assert_eq!(
            project(&clock, source, now).unwrap().global,
            held_frontier.global
        );
    } else {
        assert_eq!(project(&clock, source, now), Err(Error::OutOfWindow));
    }

    // Continue at -1000 ppm for 20 more source seconds, enough for the
    // configured slew to settle, then refresh with a new exact exchange.
    let settled = exact_exchange(
        &clock,
        source + 20_000_000_000,
        truth + 19_980_000_000,
        PATH,
    );
    let now = settled.reference.at;
    let after = project(&clock, settled.source.as_nanos(), now).unwrap();
    assert_eq!(after.global.as_nanos(), truth + 19_980_000_000);
    clock
        .observe_exchange(settled, settled.source, now)
        .unwrap();
    let locked = project(&clock, settled.source.as_nanos(), now).unwrap();
    assert_eq!(locked.global, after.global);
    assert_eq!(locked.quality, Quality::Synchronized);
    assert_eq!(zero_target(locked, now).expiry, Expiry::Definite);
}

#[test]
fn fresh_exchange_recovers_at_old_holdover_future_limit() {
    recover_at_old_future_limit(0);
}

#[test]
fn fresh_exchange_recovers_one_ns_beyond_old_holdover_future_limit() {
    recover_at_old_future_limit(1_000);
}

#[test]
fn aged_same_epoch_bound_survives_wide_refresh_and_rejection() {
    let mut clock = seeded();
    // The seed is older than the lock age but still bounds the same epoch.
    // At 2.4 s, +1.2 ms is the only phase consistent with both seed and
    // this [1.2, 2.2] ms measurement. Use a +0.5 ms reference bias.
    let mut wide = exact_exchange(&clock, 2_400_000_000, 102_401_200_000, PATH);
    wide.reference.global = GlobalInstant::from_nanos(wide.reference.global.as_nanos() + 500_000);
    wide.reference.quality = ReferenceQuality::Qualified {
        uncertainty_ns: 500_000,
    };
    clock
        .observe_exchange(wide, wide.source, wide.reference.at)
        .unwrap();

    // From that single feasible point, the next 200 ms can add at most
    // 200 us of phase. 1.4 ms + 1 ns fits the wide interval in isolation.
    let invalid = exact_exchange(&clock, 2_600_000_000, 102_601_400_001, PATH);
    let now = invalid.reference.at;
    let checkpoints = [LAST_SEED, wide.source.as_nanos(), invalid.source.as_nanos()];
    let before = checkpoints.map(|source| project(&clock, source, now).unwrap());
    assert_eq!(
        clock.observe_exchange(invalid, invalid.source, now),
        Err(Error::OutOfWindow)
    );
    assert_eq!(
        checkpoints.map(|source| project(&clock, source, now).unwrap()),
        before
    );

    // The opposite edge is allowed: the physical rate can reverse from
    // +1000 ppm to -1000 ppm at 2.4 s. Retry at the same evaluation time.
    let truth = 102_601_000_000;
    let valid = exact_exchange(&clock, invalid.source.as_nanos(), truth, PATH);
    clock.observe_exchange(valid, valid.source, now).unwrap();
    let qualified = project(&clock, valid.source.as_nanos(), now).unwrap();
    assert_eq!(qualified.global, before[2].global);
    assert!(
        qualified.global.as_nanos().abs_diff(truth) <= qualified.absolute_uncertainty_ns.unwrap()
    );
    assert_eq!(qualified.quality, Quality::Synchronized);
    assert_eq!(zero_target(qualified, now).expiry, Expiry::Definite);

    let unqualified = clock
        .project_data(
            Some(capture(&clock, valid.source.as_nanos())),
            DataReceipt {
                latest_global: None,
                ..receipt(now)
            },
            now,
        )
        .unwrap();
    assert_eq!(unqualified.global, qualified.global);
    let evidence = zero_target(unqualified, now);
    assert!(evidence.remaining.unwrap().latest.as_nanos() < 0);
    assert_eq!(evidence.expiry, Expiry::Inconclusive);
}
