#![allow(
    clippy::arithmetic_side_effects,
    reason = "fixed test inputs are in range; overflow is a test failure"
)]

use super::{boundary_witnesses::exact_exchange, *};
use crate::deadline::{Expiry, Timing, evaluate};

#[test]
fn wide_fit_and_completed_correction_project_representable_results() {
    for (hz, span, second_tick, second_source, final_tick, expected) in [
        (
            1_000_000_000,
            4_611_686_018_427_387_904_i64,
            4_611_686_018_427_387_904_i128,
            1_i64,
            9_223_372_036_854_775_808_i128,
            4_611_686_018_427_387_906_i64,
        ),
        (
            4_000_000_000,
            6_917_529_027_641_081_856,
            27_670_116_110_564_327_425,
            0,
            55_340_232_221_128_654_850,
            6_917_529_027_641_081_856,
        ),
    ] {
        let mut clock = ClockDomain::new(
            CONFIG,
            Binding {
                domain: DomainId(76),
                generation: Generation(1),
            },
            LocalOrigin {
                at: at(0),
                global: GlobalInstant::from_nanos(-span + 20_000_000),
            },
        )
        .unwrap();
        let track = clock.bind_track(0, ClockRate::new(hz).unwrap()).unwrap();
        let first = sr(sample(track, 0), -span, 0);
        clock.observe_sender_report(first, 0, at(0)).unwrap();
        let second_at = u64::try_from(span + second_source).unwrap();
        let second = sr(sample(track, second_tick), second_source, second_at);
        clock
            .observe_sender_report(second, second_tick, at(second_at))
            .unwrap();
        let now = at(u64::try_from(span).unwrap() * 2);
        let projected = clock.project_rtp(sample(track, final_tick), now).unwrap();
        assert_eq!(projected.source, Some(SourceTime::from_nanos(expected)));
        assert_eq!(projected.global.as_nanos(), expected);
    }
}

#[test]
fn latest_fit_outside_i64_does_not_erase_retained_projection() {
    let binding = Binding {
        domain: DomainId(77),
        generation: Generation(1),
    };
    let mut clock = ClockDomain::new(
        CONFIG,
        binding,
        LocalOrigin {
            at: at(0),
            global: GlobalInstant::from_nanos(i64::MIN + 100_000_000),
        },
    )
    .unwrap();
    let source = SourceTime::from_nanos(0);
    clock
        .observe_passive(
            PassiveReceipt {
                binding,
                source,
                received_at: at(0),
            },
            at(0),
        )
        .unwrap();
    let frontier = SourceTime::from_nanos(100_000_000);
    let received = at(30_000_000);
    let exchange = ActiveExchange {
        binding,
        source: frontier,
        sent_at: at(10_000_000),
        reference: ReferenceSample {
            at: received,
            global: GlobalInstant::from_nanos(i64::MIN + 60_000_000),
            quality: ReferenceQuality::Qualified {
                uncertainty_ns: 60_000_000,
            },
        },
        asymmetry_bound_ns: Some(0),
    };
    clock
        .observe_exchange(exchange, frontier, received)
        .unwrap();
    let result = clock
        .project_data(
            Some(DataCapture {
                binding,
                source,
                uncertainty_ns: 0,
            }),
            DataReceipt {
                at: received,
                latest_global: Some(GlobalInstant::from_nanos(i64::MIN + 130_000_000)),
            },
            received,
        )
        .unwrap();
    assert_eq!(result.global.as_nanos(), i64::MIN + 80_000_000);
    assert_eq!(result.calibration.unwrap().phase_correction_ns, 130_000_000);
    assert!(result.absolute_uncertainty_ns.unwrap() >= 130_000_000);
}

#[test]
fn minimum_tick_can_be_bridged_without_moving_assignment() {
    let mut clock = domain(78);
    let track = clock.bind_track(0, ClockRate::OPUS).unwrap();
    let first = sample(track, i128::MIN);
    let now = at(1_000_000_000);
    clock.bootstrap_track(first, now, now).unwrap();
    let before = clock.project_rtp(first, now).unwrap();
    clock
        .observe_sender_report(sr(first, 1_000_000_000, now.as_nanos()), first.tick, now)
        .unwrap();
    assert_eq!(clock.project_rtp(first, now).unwrap().global, before.global);
}

#[test]
fn fresh_qualified_intervals_without_a_common_clock_cannot_authorize_expiry() {
    // Adjacent pairs can each fit +1000 ppm by choosing different points
    // from B's interval, but a clock cannot use both points for the same B.
    // A to C spans 200 ms: +200 us is possible, +300 us is not.
    for (phase, expected) in [(200_000, Expiry::Definite), (300_000, Expiry::Inconclusive)] {
        let mut clock = domain(65);
        let a = exact_exchange(&clock, 1_000_000_000, 101_000_000_000, 30_000_000);
        let mut b = exact_exchange(&clock, 1_100_000_000, 101_100_150_000, 30_000_000);
        b.reference.quality = ReferenceQuality::Qualified {
            uncertainty_ns: 75_000,
        };
        let c = exact_exchange(&clock, 1_200_000_000, 101_200_000_000 + phase, 30_000_000);
        for e in [a, b] {
            clock.observe_exchange(e, e.source, e.reference.at).unwrap();
        }

        let before = data(&clock, c.source.as_nanos(), c.reference.at.as_nanos());
        let admission = clock.observe_exchange(c, c.source, c.reference.at);
        let capture = data(&clock, c.source.as_nanos(), c.reference.at.as_nanos());
        if phase == 300_000 && admission == Err(Error::OutOfWindow) {
            assert_eq!(
                capture, before,
                "rejected evidence must not change calibration"
            );
            continue;
        }
        assert_eq!(admission, Ok(()));
        let expiry = evaluate(
            capture.into(),
            Timing {
                instant: c.reference.global,
                absolute_uncertainty_ns: Some(0),
                quality: Quality::Synchronized,
            },
            GlobalDuration::from_nanos(20_000_000),
        )
        .unwrap()
        .expiry;
        assert_eq!(
            expiry, expected,
            "C phase={phase} ns, admission={admission:?}, projection={capture:?}"
        );
    }
}

#[test]
fn data_receipt_and_deadline_include_their_exact_uncertainty_edges() {
    let mut clock = domain(66);
    for source in [1_000_000_000, 1_200_000_000] {
        let e = exact_exchange(&clock, source, 100_000_000_000 + source, 30_000_000);
        clock.observe_exchange(e, e.source, e.reference.at).unwrap();
    }
    let now = at(1_230_000_000);
    let capture = DataCapture {
        binding: clock.binding(),
        source: SourceTime::from_nanos(1_230_000_001),
        uncertainty_ns: 0,
    };
    let trusted = receipt(now);
    let projected = clock.project_data(Some(capture), trusted, now).unwrap();
    assert_eq!(projected.quality, Quality::Synchronized);
    assert!(projected.global > trusted.latest_global.unwrap());
    let radius = i64::try_from(projected.absolute_uncertainty_ns.unwrap()).unwrap();
    let earliest = projected.global.as_nanos() - radius;
    for (latest, expected) in [
        (earliest, Ok(projected)),
        (earliest - 1, Err(Error::OutOfWindow)),
    ] {
        assert_eq!(
            clock.project_data(
                Some(capture),
                DataReceipt {
                    latest_global: Some(GlobalInstant::from_nanos(latest)),
                    ..trusted
                },
                now
            ),
            expected
        );
    }
    assert!(
        clock
            .project_data(Some(capture), receipt(at(now.as_nanos() + 1)), now)
            .is_err(),
        "the receipt cannot occur after evaluation"
    );

    let target = 10_000_000;
    let now_radius = 17_u64;
    for (extra, expiry) in [(0, Expiry::Inconclusive), (1, Expiry::Definite)] {
        let evidence = evaluate(
            projected.into(),
            Timing {
                instant: GlobalInstant::from_nanos(
                    projected.global.as_nanos()
                        + radius
                        + target
                        + i64::try_from(now_radius).unwrap()
                        + extra,
                ),
                absolute_uncertainty_ns: Some(now_radius),
                quality: Quality::Synchronized,
            },
            GlobalDuration::from_nanos(target),
        )
        .unwrap();
        assert_eq!(evidence.remaining.unwrap().latest.as_nanos(), -extra);
        assert_eq!(evidence.expiry, expiry);
    }
}

#[test]
fn valid_rtp_reports_are_invariant_to_extreme_extended_tick_origins() {
    for origin in [0, i128::MAX - 4_800, i128::MIN] {
        let mut clock = domain(67);
        let track = clock.bind_track(0, ClockRate::OPUS).unwrap();
        let first = sample(track, origin);
        let second = sample(track, origin + 4_800);
        let now = at(1_130_000_000);
        clock
            .observe_sender_report(
                sr(first, 1_000_000_000, 1_030_000_000),
                first.tick,
                at(1_030_000_000),
            )
            .unwrap();
        let old = clock.project_rtp(first, now).unwrap();
        let frontier = clock.project_rtp(second, now).unwrap();
        // Both reports and the full 100 ms between them are representable.
        assert_eq!(
            clock.observe_sender_report(
                sr(second, 1_100_000_000, now.as_nanos()),
                second.tick,
                now
            ),
            Ok(()),
            "shifting the tick origin to {origin} must not invalidate a nominal-rate report"
        );
        assert_eq!(clock.project_rtp(first, now).unwrap().global, old.global);
        assert_eq!(
            clock.project_rtp(second, now).unwrap().global,
            frontier.global
        );
    }
}

#[test]
fn sender_report_intervals_keep_prior_drift_constraints() {
    // The middle report is compatible with the first only on a smaller
    // interval than its stated uncertainty. The last exact report must
    // be reachable from that intersection, not the entire middle interval.
    for (offset, expected) in [(200_000, Ok(())), (200_001, Err(Error::OutOfWindow))] {
        let mut clock = domain(69);
        let track = clock.bind_track(0, ClockRate::OPUS).unwrap();
        let first = sample(track, 0);
        let middle = sample(track, 4_800);
        let last = sample(track, 9_600);
        let mut b = sr(middle, 1_100_150_000, 1_130_000_000);
        b.uncertainty_ns = 75_000;
        for report in [sr(first, 1_000_000_000, 1_030_000_000), b] {
            clock
                .observe_sender_report(report, report.sample.tick, report.received_at)
                .unwrap();
        }
        let now = at(1_230_000_000);
        let before = clock.project_rtp(last, now).unwrap();
        assert_eq!(
            clock.observe_sender_report(
                sr(last, 1_200_000_000 + offset, now.as_nanos()),
                last.tick,
                now
            ),
            expected
        );
        let after = clock.project_rtp(last, now).unwrap();
        assert_eq!(after.global, before.global);
        if expected.is_err() {
            assert_eq!(after, before);
        }
    }
}

#[test]
fn off_phase_update_preserves_retained_history_and_the_sealed_frontier() {
    let mut clock = domain(68);
    for (source, global) in [
        (1_000_000_000, 101_000_000_000),
        (1_200_000_000, 101_199_800_000),
    ] {
        let e = exact_exchange(&clock, source, global, 30_000_000);
        clock.observe_exchange(e, e.source, e.reference.at).unwrap();
    }
    // Changing the phase of the fractional fit by half a nanosecond
    // can change rounded output away from a frontier where outputs agree.
    let next = exact_exchange(&clock, 1_400_000_500, 101_399_600_500, 30_000_000);
    let frontier = 1_400_000_000;
    let now = next.reference.at.as_nanos();
    let queries = [
        1_100_000_000,
        1_200_000_503,
        frontier - 500,
        frontier - 1,
        frontier,
    ];
    let retained = queries.map(|source| data(&clock, source, now).global);
    clock
        .observe_exchange(next, SourceTime::from_nanos(frontier), next.reference.at)
        .unwrap();
    assert_eq!(
        queries.map(|source| data(&clock, source, now).global),
        retained
    );
    let [.., mut previous] = retained;
    for offset in 1..=64 {
        let capture = data(&clock, frontier + offset * 31, now);
        assert!(
            capture.global >= previous,
            "source offset {} ns",
            offset * 31
        );
        previous = capture.global;
    }
    let measured = data(&clock, next.source.as_nanos(), now);
    assert!(
        measured.global.as_nanos().abs_diff(101_399_600_500)
            <= measured.absolute_uncertainty_ns.unwrap()
    );
}

#[test]
fn exact_drift_fit_and_slew_work_at_signed_and_monotonic_time_edges() {
    const LOCAL: u64 = u64::MAX - 1_000_000_000;
    for (global, source) in [
        (i64::MIN + 1, i64::MAX - 300_000_000),
        (i64::MAX - 300_000_000, i64::MIN),
    ] {
        let mut clock = ClockDomain::new(
            CONFIG,
            Binding {
                domain: DomainId(69),
                generation: Generation(7),
            },
            LocalOrigin {
                at: at(LOCAL),
                global: GlobalInstant::from_nanos(global),
            },
        )
        .unwrap();
        for (source_offset, turnaround) in [(10_000_000, 10_000_000), (210_000_000, 209_800_000)] {
            let e = ActiveExchange {
                binding: clock.binding(),
                source: SourceTime::from_nanos(source + source_offset),
                sent_at: at(LOCAL + turnaround - 1),
                reference: ReferenceSample {
                    at: at(LOCAL + turnaround + 1),
                    global: GlobalInstant::from_nanos(
                        global + i64::try_from(turnaround + 1).unwrap(),
                    ),
                    quality: ReferenceQuality::Qualified { uncertainty_ns: 0 },
                },
                asymmetry_bound_ns: Some(0),
            };
            clock.observe_exchange(e, e.source, e.reference.at).unwrap();
        }
        // 250 ms at -1000 ppm after the first exact turnaround, plus 501 ns
        // to exercise fractional output once the correction has settled.
        let projected = clock
            .project_data(
                Some(DataCapture {
                    binding: clock.binding(),
                    source: SourceTime::from_nanos(source + 260_000_501),
                    uncertainty_ns: 0,
                }),
                DataReceipt {
                    at: at(LOCAL + 270_000_000),
                    latest_global: Some(GlobalInstant::from_nanos(global + 270_000_000)),
                },
                at(LOCAL + 270_000_000),
            )
            .unwrap();
        let truth_floor = global + 259_750_500;
        assert_eq!(projected.global.as_nanos(), truth_floor);
        assert!(projected.absolute_uncertainty_ns.unwrap() > 0);
    }
}
