use super::*;
use crate::deadline::{Expiry, Timing, evaluate};

#[test]
fn capture_after_receipt_cannot_authorize_definite_expiry() {
    let mut clock = domain(50);
    for (source, received) in [
        (1_000_000_000, 1_030_000_000),
        (1_200_000_000, 1_230_000_000),
    ] {
        let e = exchange(&clock, source, 100_000_000_000 + source, received);
        clock.observe_exchange(e, e.source, e.reference.at).unwrap();
    }
    let receipt = receipt(at(1_270_000_000));
    let now = at(1_950_000_000);
    let evaluation_time = Timing {
        instant: GlobalInstant::from_nanos(101_950_000_000),
        absolute_uncertainty_ns: Some(0),
        quality: Quality::Synchronized,
    };
    let target = GlobalDuration::from_nanos(100_000_000);
    let capture = |source| {
        clock.project_data(
            Some(DataCapture {
                binding: clock.binding(),
                source: SourceTime::from_nanos(source),
                uncertainty_ns: 0,
            }),
            receipt,
            now,
        )
    };
    let honest = capture(1_250_000_000).unwrap();
    assert_eq!(
        evaluate(honest.into(), evaluation_time, target)
            .unwrap()
            .expiry,
        Expiry::Definite
    );

    // This timestamp is 430 ms after the receiver already had the data.
    assert_eq!(capture(1_700_000_000), Err(Error::OutOfWindow));
    let unqualified = clock
        .project_data(
            Some(DataCapture {
                binding: clock.binding(),
                source: SourceTime::from_nanos(1_250_000_000),
                uncertainty_ns: 0,
            }),
            DataReceipt {
                latest_global: None,
                ..receipt
            },
            now,
        )
        .unwrap();
    assert_eq!(unqualified.global, honest.global);
    assert_eq!(
        evaluate(unqualified.into(), evaluation_time, target)
            .unwrap()
            .expiry,
        Expiry::Inconclusive
    );
}

#[test]
fn uncertain_act_bootstrap_does_not_permanently_block_qualified_evidence() {
    let mut clock = domain(51);
    let frontier = SourceTime::from_nanos(1_000_000_000);
    let observed_at = at(1_030_000_000);
    let hint = ActHint {
        binding: clock.binding(),
        source: frontier,
        global_estimate: GlobalInstant::from_nanos(100_750_000_000),
        observed_at,
        conditional_error_ns: 250_000_000,
    };
    clock.observe_act(hint, frontier, observed_at).unwrap();
    let assigned = data(&clock, frontier.as_nanos(), observed_at.as_nanos());

    let results = [
        (1_200_000_000, 1_230_000_000),
        (1_400_000_000, 1_430_000_000),
    ]
    .map(|(source, received)| {
        let e = exchange(&clock, source, 100_000_000_000 + source, received);
        let result = clock.observe_exchange(e, frontier, e.reference.at);
        assert_eq!(
            data(&clock, frontier.as_nanos(), received).global,
            assigned.global
        );
        result
    });
    assert_eq!(
        results,
        [Ok(()), Ok(())],
        "well-bounded exchanges must be usable without a new incarnation"
    );
    let current = data(&clock, 1_400_000_000, 1_430_000_000);
    assert_eq!(current.calibration.unwrap().evidence, Evidence::Exchange);
    assert_eq!(current.quality, Quality::Recovering);
    // A 250 ms phase correction at 10,000 ppm takes 25 seconds. Feed
    // fresh evidence while the active projection moves without a step.
    let mut previous = current;
    for seconds in 2_i64..=28 {
        let source = seconds.checked_mul(1_000_000_000).unwrap();
        let current_truth = 100_000_000_000_i64.checked_add(source).unwrap();
        let now = u64::try_from(source.checked_add(30_000_000).unwrap()).unwrap();
        let exchange = exchange(&clock, source, current_truth, now);
        let before = data(&clock, source, now);
        clock
            .observe_exchange(exchange, exchange.source, exchange.reference.at)
            .unwrap();
        let projected = data(&clock, source, now);
        assert_eq!(projected.global, before.global);
        assert!(projected.global > previous.global);
        assert!(
            projected.global.as_nanos().abs_diff(current_truth)
                <= projected.absolute_uncertainty_ns.unwrap()
        );
        previous = projected;
    }
    assert_eq!(previous.quality, Quality::Synchronized);
    assert_eq!(previous.global.as_nanos(), 128_000_000_000);
}

#[test]
fn expired_wide_exchange_does_not_veto_precise_recovery() {
    let mut clock = domain(55);
    // 190 ms outbound, 10 ms inbound and 95 ms of reference error.
    // The midpoint is 185 ms early, inside the 200 ms combined bound.
    let wide = ActiveExchange {
        binding: clock.binding(),
        source: SourceTime::from_nanos(1_000_000_000),
        sent_at: at(810_000_000),
        reference: ReferenceSample {
            at: at(1_010_000_000),
            global: GlobalInstant::from_nanos(100_915_000_000),
            quality: ReferenceQuality::Qualified {
                uncertainty_ns: 100_000_000,
            },
        },
        asymmetry_bound_ns: None,
    };
    clock
        .observe_exchange(wide, wide.source, wide.reference.at)
        .unwrap();
    let held = data(&clock, 2_200_000_000, 2_230_000_000);
    assert_eq!(held.quality, Quality::Holdover);
    assert!(
        held.global.as_nanos().abs_diff(102_200_000_000) <= held.absolute_uncertainty_ns.unwrap()
    );

    let recovery = [
        (2_200_000_000, 102_200_000_000, 2_230_000_000),
        (2_400_000_000, 102_400_000_000, 2_430_000_000),
    ]
    .map(|(source, global, received)| {
        let before = data(&clock, source, received);
        let fresh = exchange(&clock, source, global, received);
        let result = clock.observe_exchange(fresh, fresh.source, fresh.reference.at);
        assert_eq!(data(&clock, source, received).global, before.global);
        result
    });
    assert_eq!(
        recovery,
        [Ok(()), Ok(())],
        "fresh observations agree with the old bound and need only a continuous slew"
    );
}

#[test]
fn converged_slew_can_refresh_evidence_without_evicting_reorder_history() {
    let mut clock = ClockDomain::new(
        Config {
            history_capacity: 2,
            ..CONFIG
        },
        Binding {
            domain: DomainId(56),
            generation: Generation(7),
        },
        LocalOrigin {
            at: at(0),
            global: GlobalInstant::from_nanos(100_000_000_000),
        },
    )
    .unwrap();
    let hint = ActHint {
        binding: clock.binding(),
        source: SourceTime::from_nanos(1_000_000_000),
        global_estimate: GlobalInstant::from_nanos(100_999_000_000),
        observed_at: at(1_030_000_000),
        conditional_error_ns: 1_000_000,
    };
    clock
        .observe_act(hint, hint.source, hint.observed_at)
        .unwrap();
    let first = exchange(&clock, 1_200_000_000, 101_200_000_000, 1_230_000_000);
    clock
        .observe_exchange(first, first.source, first.reference.at)
        .unwrap();

    let refresh = exchange(&clock, 1_400_000_000, 101_400_000_000, 1_430_000_000);
    let old = data(&clock, 1_100_000_000, 1_430_000_000);
    let current = data(&clock, 1_400_000_000, 1_430_000_000);
    assert_eq!(current.global.as_nanos(), 101_400_000_000);
    assert_eq!(current.calibration.unwrap().phase_correction_ns, 0);
    clock
        .observe_exchange(refresh, refresh.source, refresh.reference.at)
        .expect("the corrected mapping already agrees with the refresh");
    let updated = data(&clock, 1_400_000_000, 1_430_000_000);
    assert_eq!(updated.global, current.global);
    assert_eq!(updated.quality, Quality::Synchronized);
    assert_eq!(
        data(&clock, 1_100_000_000, 1_430_000_000).global,
        old.global
    );
}

#[test]
fn exchange_cannot_seed_calibration_across_generation_reset() {
    let mut clock = domain(52);
    let fresh = Binding {
        generation: Generation(8),
        ..clock.binding()
    };
    clock.reset_generation(fresh, at(1_020_000_000)).unwrap();
    let straddling = exchange(&clock, 1_000_000_000, 101_000_000_000, 1_030_000_000);
    assert_eq!(
        clock.observe_exchange(straddling, straddling.source, straddling.reference.at),
        Err(Error::Stale),
    );
    assert_eq!(
        data(&clock, straddling.source.as_nanos(), 1_030_000_000).source,
        None
    );
    let e = exchange(&clock, 1_200_000_000, 101_200_000_000, 1_230_000_000);
    clock.observe_exchange(e, e.source, e.reference.at).unwrap();
}

#[test]
fn extreme_report_tick_cannot_replace_a_working_track_model() {
    let mut clock = domain(54);
    let track = clock.bind_track(0, ClockRate::OPUS).unwrap();
    let first = sr(sample(track, 0), 1_000_000_000, 1_000_000_000);
    clock
        .observe_sender_report(first, first.sample.tick, first.received_at)
        .unwrap();
    let now = at(1_400_000_000);
    let before = clock.project_rtp(first.sample, now).unwrap();
    for tick in [i128::MIN, i128::MAX] {
        let extreme = sr(sample(track, tick), 1_200_000_000, 1_200_000_000);
        assert!(clock.observe_sender_report(extreme, 0, now).is_err());
        assert!(clock.project_rtp(extreme.sample, now).is_err());
        assert_eq!(clock.project_rtp(first.sample, now).unwrap(), before);
    }
    let valid = sr(sample(track, 9_600), 1_200_000_000, 1_200_000_000);
    clock.observe_sender_report(valid, 0, now).unwrap();
    assert_eq!(
        clock.project_rtp(valid.sample, now).unwrap().source,
        Some(valid.source)
    );
}

#[test]
fn repeated_loss_fences_reordered_reference_restoration() {
    let mut clock = domain(53);
    let binding = clock.binding();
    clock
        .set_reference_state(binding, at(1_000_000_000), ReferenceState::Lost)
        .unwrap();
    clock
        .set_reference_state(binding, at(1_200_000_000), ReferenceState::Lost)
        .unwrap();
    assert_eq!(
        clock.set_reference_state(binding, at(1_100_000_000), ReferenceState::Usable),
        Err(Error::Stale),
        "an older restoration cannot override a later loss observation"
    );
    assert_eq!(
        clock.set_reference_state(binding, at(1_200_000_000), ReferenceState::Usable),
        Err(Error::Stale)
    );
    clock
        .set_reference_state(binding, at(1_300_000_000), ReferenceState::Usable)
        .unwrap();
}
