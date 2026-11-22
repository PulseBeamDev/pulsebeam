use super::*;
use crate::deadline::{Expiry, Timing, evaluate};

const ORIGIN: i64 = 100_000_000_000;

pub(super) fn exact_exchange(
    clock: &ClockDomain,
    source: i64,
    global: i64,
    path_ns: u64,
) -> ActiveExchange {
    let turnaround = u64::try_from(global.checked_sub(ORIGIN).unwrap()).unwrap();
    let received = turnaround.checked_add(path_ns).unwrap();
    ActiveExchange {
        binding: clock.binding(),
        source: SourceTime::from_nanos(source),
        sent_at: at(turnaround.checked_sub(path_ns).unwrap()),
        reference: ReferenceSample {
            at: at(received),
            global: GlobalInstant::from_nanos(
                ORIGIN
                    .checked_add(i64::try_from(received).unwrap())
                    .unwrap(),
            ),
            quality: ReferenceQuality::Qualified { uncertainty_ns: 0 },
        },
        asymmetry_bound_ns: Some(0),
    }
}

#[test]
fn exact_exchange_pair_cannot_exceed_configured_drift() {
    let mut clock = domain(61);
    let first = exact_exchange(&clock, 1_000_000_000, 101_000_000_000, 30_000_000);
    clock
        .observe_exchange(first, first.source, first.reference.at)
        .unwrap();
    // 100,000,001 source ns * 1.001 gives at most 100,100,001.001
    // global ns. Both measurement radii are zero, so 100,100,002 is impossible.
    let second = exact_exchange(&clock, 1_100_000_001, 101_100_100_002, 30_000_000);
    let now = second.reference.at.as_nanos();
    let before = data(&clock, second.source.as_nanos(), now);
    let observed = clock.observe_exchange(second, second.source, second.reference.at);
    let after = data(&clock, second.source.as_nanos(), now);
    let expiry = evaluate(
        after.into(),
        Timing {
            instant: second.reference.global,
            absolute_uncertainty_ns: Some(0),
            quality: Quality::Synchronized,
        },
        GlobalDuration::from_nanos(0),
    )
    .unwrap()
    .expiry;
    assert_eq!(
        (observed, after, expiry),
        (Err(Error::OutOfWindow), before, Expiry::Inconclusive)
    );
}

#[test]
fn precise_update_recovers_from_a_wide_center_that_runs_late() {
    let mut clock = domain(62);
    // True turnaround at global 101 s with 10 ms on each path. A
    // +100 ms qualified reference error puts the first midpoint at 101.1 s.
    let equal_paths = exact_exchange(&clock, 1_000_000_000, 101_000_000_000, 10_000_000);
    let wide = ActiveExchange {
        reference: ReferenceSample {
            global: GlobalInstant::from_nanos(101_110_000_000),
            quality: ReferenceQuality::Qualified {
                uncertainty_ns: 100_000_000,
            },
            ..equal_paths.reference
        },
        ..equal_paths
    };
    clock
        .observe_exchange(wide, wide.source, wide.reference.at)
        .unwrap();

    // Then 100 ms in source time at -1,000 ppm gives 99.9 ms in global
    // time. The exact second midpoint precedes the noisy first midpoint,
    // even though the two exchanges do not overlap.
    let precise = exact_exchange(&clock, 1_100_000_000, 101_099_900_000, 30_000_000);
    let now = precise.reference.at.as_nanos();
    let before = data(&clock, precise.source.as_nanos(), now);
    assert_eq!(
        clock.observe_exchange(precise, precise.source, precise.reference.at),
        Ok(())
    );
    let after = data(&clock, precise.source.as_nanos(), now);
    assert_eq!(after.global, before.global);
    assert_eq!(after.quality, Quality::Recovering);
    assert!(
        after.global.as_nanos().abs_diff(101_099_900_000) <= after.absolute_uncertainty_ns.unwrap()
    );
}

#[test]
fn same_fractional_fit_refreshes_at_capacity_after_off_phase_frontier() {
    let mut clock = ClockDomain::new(
        Config {
            history_capacity: 2,
            ..CONFIG
        },
        Binding {
            domain: DomainId(63),
            generation: Generation(7),
        },
        LocalOrigin {
            at: at(0),
            global: GlobalInstant::from_nanos(ORIGIN),
        },
    )
    .unwrap();
    let first = exact_exchange(&clock, 1_000_000_000, 101_000_000_000, 30_000_000);
    clock
        .observe_exchange(first, first.source, first.reference.at)
        .unwrap();
    let next = exact_exchange(&clock, 1_200_000_000, 101_199_800_000, 30_000_000);
    // Install the 999/1000 fit halfway between its integral points. The
    // correction of about 200 us at 9.99 ms/s has settled by the next frontier.
    clock
        .observe_exchange(
            next,
            SourceTime::from_nanos(1_200_000_500),
            next.reference.at,
        )
        .unwrap();

    let refresh = exact_exchange(&clock, 1_400_000_000, 101_399_600_000, 30_000_000);
    let now = refresh.reference.at.as_nanos();
    let frontier = 1_300_000_250;
    let retained = data(&clock, 1_100_000_000, now);
    let before = data(&clock, frontier, now);
    assert_eq!(before.calibration.unwrap().phase_correction_ns, 0);
    assert_eq!(
        clock.observe_exchange(
            refresh,
            SourceTime::from_nanos(frontier),
            refresh.reference.at
        ),
        Ok(()),
        "the same exact fit needs no new projection segment"
    );
    let after = data(&clock, frontier, now);
    assert_eq!(after.global, before.global);
    assert_eq!(after.calibration.unwrap().observation_age_ns, 0);
    assert_eq!(data(&clock, 1_100_000_000, now).global, retained.global);
}

#[test]
fn long_negative_drift_holdover_recovers_at_future_envelope_edge() {
    let mut clock = domain(64);
    for (source, truth) in [
        (1_000_000_000, 101_000_000_000),
        (2_000_000_000, 102_000_000_000),
    ] {
        let e = exact_exchange(&clock, source, truth, 30_000_000);
        clock.observe_exchange(e, e.source, e.reference.at).unwrap();
    }
    // Over 200 source seconds at -1000 ppm, the old estimate goes 200 ms
    // ahead. At receipt (100 ms later) it is exactly at the future envelope.
    let update = exact_exchange(&clock, 202_000_000_000, 301_800_000_000, 100_000_000);
    let now = update.reference.at.as_nanos();
    let held = data(&clock, update.source.as_nanos(), now);
    assert_eq!(held.quality, Quality::Holdover);
    clock
        .observe_exchange(update, update.source, update.reference.at)
        .unwrap();
    let recovering = data(&clock, update.source.as_nanos(), now);
    assert_eq!(recovering.global, held.global);
    assert_eq!(recovering.quality, Quality::Recovering);
    assert!(
        recovering.global.as_nanos().abs_diff(301_800_000_000)
            <= recovering.absolute_uncertainty_ns.unwrap()
    );

    // After 30 more source seconds, independent truth is 331.77 s and the
    // 200 ms correction at 9.99 ms per source second has had time to settle.
    let settled = data(&clock, 232_000_000_000, 231_870_000_000);
    assert_eq!(settled.global.as_nanos(), 331_770_000_000);
}
