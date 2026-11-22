use super::*;
use crate::deadline::{Expiry, Timing, evaluate};

const ORIGIN: i64 = 100_000_000_000;

fn with_history(capacity: usize, reorder_ns: u64) -> ClockDomain {
    ClockDomain::new(
        Config {
            history_capacity: capacity,
            reorder_ns,
            ..CONFIG
        },
        Binding {
            domain: DomainId(57),
            generation: Generation(7),
        },
        LocalOrigin {
            at: at(0),
            global: GlobalInstant::from_nanos(ORIGIN),
        },
    )
    .unwrap()
}

#[test]
fn wide_to_narrow_admission_accounts_for_compatible_drift() {
    let mut clock = domain(58);
    // Turnaround at 101 s, path delays 199.9 and 0.1 ms, reference
    // error -100 ms. The resulting estimate is 199.9 ms early.
    let wide = ActiveExchange {
        binding: clock.binding(),
        source: SourceTime::from_nanos(1_000_000_000),
        sent_at: at(800_100_000),
        reference: ReferenceSample {
            at: at(1_000_100_000),
            global: GlobalInstant::from_nanos(100_900_100_000),
            quality: ReferenceQuality::Qualified {
                uncertainty_ns: 100_000_000,
            },
        },
        asymmetry_bound_ns: None,
    };
    clock
        .observe_exchange(wide, wide.source, wide.reference.at)
        .unwrap();

    // The true source/global slope is +1000 ppm. A second capture 1.2 s
    // later in source time is 1.2012 s later in global time.
    let source = 2_200_000_000;
    let now = 2_231_200_000;
    let truth = 102_201_200_000;
    let held = data(&clock, source, now);
    assert_eq!(held.quality, Quality::Holdover);
    assert!(held.global.as_nanos().abs_diff(truth) <= held.absolute_uncertainty_ns.unwrap());

    let inconsistent = exchange(&clock, source, 102_205_200_000, now);
    assert_eq!(
        clock.observe_exchange(inconsistent, inconsistent.source, inconsistent.reference.at),
        Err(Error::OutOfWindow)
    );
    assert_eq!(data(&clock, source, now), held);

    let precise = exchange(&clock, source, truth, now);
    clock
        .observe_exchange(precise, precise.source, precise.reference.at)
        .expect("the precise exchange agrees with the old bound and allowed drift");
    let recovering = data(&clock, source, now);
    assert_eq!(recovering.global, held.global);
    assert_eq!(recovering.quality, Quality::Recovering);
    assert!(
        recovering.global.as_nanos().abs_diff(truth) <= recovering.absolute_uncertainty_ns.unwrap()
    );
}

#[test]
fn expired_precise_fit_recovers_after_long_holdover_at_drift_limit() {
    let mut clock = domain(59);
    for (source, truth, received) in [
        (1_000_000_000, 101_000_000_000, 1_030_000_000),
        (2_000_000_000, 102_000_000_000, 2_030_000_000),
    ] {
        let update = exchange(&clock, source, truth, received);
        clock
            .observe_exchange(update, update.source, update.reference.at)
            .unwrap();
    }
    assert_eq!(
        data(&clock, 2_000_000_000, 2_030_000_000).quality,
        Quality::Synchronized
    );

    // Then 200 seconds at +1,000 ppm. The 200 ms discrepancy is
    // physically plausible but exceeds the 150 ms steady-state limit.
    let source = 202_000_000_000;
    let truth = 302_200_000_000;
    let now = 202_230_000_000;
    let stale = data(&clock, source, now);
    assert_eq!(stale.quality, Quality::Holdover);
    let update = exchange(&clock, source, truth, now);
    clock
        .observe_exchange(update, update.source, update.reference.at)
        .unwrap();
    let recovering = data(&clock, source, now);
    assert_eq!(recovering.global, stale.global);
    assert_eq!(recovering.quality, Quality::Recovering);
    assert!(
        recovering.global.as_nanos().abs_diff(truth) <= recovering.absolute_uncertainty_ns.unwrap()
    );
}

fn slewing_with_full_history() -> ClockDomain {
    let mut clock = with_history(2, CONFIG.reorder_ns);
    for (source, center, received) in [
        (1_000_000_000, 101_000_000_000, 1_030_000_000),
        (1_200_000_000, 101_199_800_000, 1_229_800_000),
    ] {
        let e = exchange(&clock, source, center, received);
        clock.observe_exchange(e, e.source, e.reference.at).unwrap();
    }
    clock
}

#[test]
fn rounded_agreement_does_not_hide_an_unfinished_slew_at_capacity() {
    let mut clock = slewing_with_full_history();
    let refresh = exchange(&clock, 1_400_000_000, 101_399_600_000, 1_429_600_000);
    let now = refresh.reference.at.as_nanos();
    // The 200 us gap closes after 20,020,020.02 ns. At this first
    // frontier the rounded lines agree, but not at the next tick.
    let frontier = 1_220_020_000;
    let before = data(&clock, frontier, now);
    assert_eq!(before.global.as_nanos(), 101_219_799_980);
    assert_eq!(
        clock.observe_exchange(
            refresh,
            SourceTime::from_nanos(frontier),
            refresh.reference.at
        ),
        Err(Error::Capacity)
    );
    assert_eq!(data(&clock, frontier, now), before);
    let mut last = before.global;
    for source in 1_220_020_001..=1_220_020_025 {
        let current = data(&clock, source, now).global;
        assert!(current.as_nanos().abs_diff(last.as_nanos()) <= 1);
        assert!(current >= last);
        last = current;
    }
}

#[test]
fn identical_fractional_fit_refreshes_after_slew_at_capacity() {
    // Both frontiers are past the crossing. The first is on an integral
    // point of the 999/1000 mapping; the second is between integral points.
    for frontier in [1_220_021_000, 1_220_020_022] {
        let mut clock = slewing_with_full_history();
        let refresh = exchange(&clock, 1_400_000_000, 101_399_600_000, 1_429_600_000);
        let now = refresh.reference.at.as_nanos();
        let current = data(&clock, frontier, now);
        let retained = data(&clock, 1_100_000_000, now);
        assert_eq!(current.calibration.unwrap().phase_correction_ns, 0);
        let result = clock.observe_exchange(
            refresh,
            SourceTime::from_nanos(frontier),
            refresh.reference.at,
        );
        assert_eq!(result, Ok(()), "unchanged mapping at frontier {frontier}");
        assert_eq!(data(&clock, 1_100_000_000, now).global, retained.global);
        let updated = data(&clock, frontier, now);
        assert_eq!(updated.global, current.global);
        assert_eq!(updated.calibration.unwrap().observation_age_ns, 0);
        // The unchanged map floors these adjacent values to the same ns.
        assert_eq!(
            data(&clock, 1_220_022_001, now).global,
            data(&clock, 1_220_022_000, now).global
        );
    }
}

#[test]
fn maximum_history_preserves_reorder_during_rejection_and_pruning() {
    let mut clock = with_history(32, 4_000_000_000);
    for step in 0_i64..32 {
        let source = 1_000_000_000_i64
            .checked_add(step.checked_mul(100_000_000).unwrap())
            .unwrap();
        let bias = if step % 2 == 0 { 0 } else { 20_000 };
        let global = ORIGIN
            .checked_add(source)
            .unwrap()
            .checked_add(bias)
            .unwrap();
        let now = u64::try_from(source)
            .unwrap()
            .checked_add(30_000_000)
            .unwrap();
        let e = exchange(&clock, source, global, now);
        clock.observe_exchange(e, e.source, e.reference.at).unwrap();
    }

    let full = exchange(&clock, 4_200_000_000, 104_200_000_000, 4_230_000_000);
    let now = full.reference.at.as_nanos();
    let checkpoints = [1_000_000_000, 1_300_000_000, 4_200_000_000];
    let before = checkpoints.map(|source| data(&clock, source, now));
    assert_eq!(
        clock.observe_exchange(full, full.source, full.reference.at),
        Err(Error::Capacity)
    );
    assert_eq!(checkpoints.map(|source| data(&clock, source, now)), before);

    let later = exchange(&clock, 5_300_000_000, 105_300_000_000, 5_330_000_000);
    clock
        .observe_exchange(later, later.source, later.reference.at)
        .unwrap();
    let [_, retained, recent] = before;
    assert_eq!(
        data(&clock, 1_300_000_000, 5_330_000_000).global,
        retained.global
    );
    assert_eq!(
        data(&clock, 4_200_000_000, 5_330_000_000).global,
        recent.global
    );
}

#[test]
fn projected_capture_bounds_compose_with_deadlines_through_holdover() {
    let mut clock = domain(59);
    for (source, center, received) in [
        (1_000_000_000, 101_000_000_000, 1_030_000_000),
        (2_000_000_000, 102_001_000_000, 2_031_000_000),
    ] {
        let e = exchange(&clock, source, center, received);
        clock.observe_exchange(e, e.source, e.reference.at).unwrap();
    }
    let audio = clock.bind_track(0, ClockRate::OPUS).unwrap();
    let report = SenderReport {
        uncertainty_ns: 500,
        ..sr(sample(audio, 0), 1_999_999_500, 2_040_000_000)
    };
    clock
        .observe_sender_report(report, 0, report.received_at)
        .unwrap();

    // Tick 24,000 at +1000 ppm is 500.5 ms from the report's true
    // boundary, and the sample can be 20,854 ns into that tick.
    // Applying the independent +1000 ppm source/global drift gives truth.
    let truth = 102_502_021_374_i64;
    for (now, expected_quality, zero_target_expiry) in [
        (2_600_000_000, Quality::Synchronized, Expiry::Definite),
        (3_100_000_000, Quality::Holdover, Expiry::Inconclusive),
    ] {
        let rtp = clock.project_rtp(sample(audio, 24_000), at(now)).unwrap();
        let data = clock
            .project_data(
                Some(DataCapture {
                    binding: clock.binding(),
                    source: SourceTime::from_nanos(2_500_000_000),
                    uncertainty_ns: 520_854,
                }),
                receipt(at(2_550_000_000)),
                at(now),
            )
            .unwrap();
        let now_center = ORIGIN.checked_add(i64::try_from(now).unwrap()).unwrap();
        let local_now = Timing {
            instant: GlobalInstant::from_nanos(now_center),
            absolute_uncertainty_ns: Some(3_000),
            quality: Quality::Synchronized,
        };
        for projected in [rtp, data] {
            assert_eq!(projected.quality, expected_quality);
            assert!(
                projected.global.as_nanos().abs_diff(truth)
                    <= projected.absolute_uncertainty_ns.unwrap()
            );
            for error in [-3_000_i64, 3_000] {
                let age = now_center
                    .checked_add(error)
                    .unwrap()
                    .checked_sub(truth)
                    .unwrap();
                let target = GlobalDuration::from_nanos(age);
                let result = evaluate(projected.into(), local_now, target).unwrap();
                let age = result.age.unwrap();
                let remaining = result.remaining.unwrap();
                assert!(age.earliest <= target && target <= age.latest);
                assert!(remaining.earliest.as_nanos() <= 0 && remaining.latest.as_nanos() >= 0);
                assert_eq!(result.expiry, Expiry::Inconclusive);
            }
            assert_eq!(
                evaluate(projected.into(), local_now, GlobalDuration::from_nanos(0))
                    .unwrap()
                    .expiry,
                zero_target_expiry
            );
        }
    }
}
