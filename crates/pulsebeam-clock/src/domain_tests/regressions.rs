use super::*;

#[test]
fn calibration_refreshes_without_duplicate_segments_and_seals_before_first_fit_origin() {
    let mut clock = ClockDomain::new(
        Config {
            history_capacity: 2,
            ..CONFIG
        },
        Binding {
            domain: DomainId(15),
            generation: Generation(0),
        },
        LocalOrigin {
            at: at(0),
            global: GlobalInstant::from_nanos(100_000_000_000),
        },
    )
    .unwrap();
    let first = exchange(&clock, 1_000_000_000, 100_970_000_000, 1_000_000_000);
    clock
        .observe_exchange(first, first.source, first.reference.at)
        .unwrap();
    let frontier = SourceTime::from_nanos(980_000_000);
    let before = data(&clock, frontier.as_nanos(), 1_000_000_000).global;
    let second = exchange(&clock, 1_200_000_000, 101_170_000_000, 1_200_000_000);
    clock
        .observe_exchange(second, frontier, second.reference.at)
        .unwrap();
    assert_eq!(
        data(&clock, frontier.as_nanos(), 1_200_000_000).global,
        before
    );

    let third = exchange(&clock, 1_400_000_000, 101_370_000_000, 1_400_000_000);
    assert_eq!(
        clock.observe_exchange(
            third,
            SourceTime::from_nanos(979_999_999),
            third.reference.at
        ),
        Err(Error::Stale)
    );
    for step in 7_i64..=12 {
        let x = step * 200_000_000;
        let e = exchange(&clock, x, 99_970_000_000 + x, u64::try_from(x).unwrap());
        clock.observe_exchange(e, e.source, e.reference.at).unwrap();
        let projected = data(&clock, frontier.as_nanos(), e.reference.at.as_nanos());
        assert_eq!(projected.calibration.unwrap().observation_age_ns, 0);
        assert_eq!(projected.global, before);
    }
    // A real correction, not an identical continuation, can also start
    // before the initial affine origin.
    let mut fresh = domain(16);
    let first = exchange(&fresh, 1_000_000_000, 100_970_000_000, 1_000_000_000);
    fresh
        .observe_exchange(first, first.source, first.reference.at)
        .unwrap();
    let changed = exchange(&fresh, 1_200_000_000, 101_170_020_000, 1_200_000_000);
    fresh
        .observe_exchange(changed, frontier, changed.reference.at)
        .unwrap();
    assert_eq!(
        data(&fresh, frontier.as_nanos(), 1_200_000_000).global,
        before
    );
    assert_eq!(
        data(&fresh, 970_000_000, 1_200_000_000).global.as_nanos(),
        100_940_000_000
    );
}

#[test]
fn quantized_rtp_capture_interval_is_inside_the_claimed_bound() {
    let mut clock = domain(17);
    for x in [1_000_000_000, 2_000_000_000] {
        let mut e = exchange(
            &clock,
            x,
            100_000_000_000 + x,
            u64::try_from(x).unwrap() + 30_000_000,
        );
        e.asymmetry_bound_ns = Some(0);
        e.reference.quality = ReferenceQuality::Qualified { uncertainty_ns: 0 };
        clock.observe_exchange(e, e.source, e.reference.at).unwrap();
    }
    let audio = clock.bind_track(0, ClockRate::OPUS).unwrap();
    let r = sr(sample(audio, 0), 2_000_000_000, 2_040_000_000);
    clock.observe_sender_report(r, 0, r.received_at).unwrap();
    let projected = clock.project_rtp(r.sample, r.received_at).unwrap();
    assert_eq!(projected.global.as_nanos(), 102_000_000_000);
    for true_capture in [102_000_000_000, 102_000_020_833] {
        assert!(
            projected.global.as_nanos().abs_diff(true_capture)
                <= projected.absolute_uncertainty_ns.unwrap()
        );
    }
}

#[test]
fn noisy_sender_report_pairs_respect_physical_intervals_and_composed_reorder() {
    let mut clock = domain(18);
    let audio = clock.bind_track(0, ClockRate::OPUS).unwrap();
    let first = SenderReport {
        uncertainty_ns: 1_000_000,
        ..sr(sample(audio, 0), 1_000_000_000, 1_000_000_000)
    };
    clock
        .observe_sender_report(first, 0, first.received_at)
        .unwrap();
    let second = SenderReport {
        uncertainty_ns: 1_000_000,
        ..sr(sample(audio, 48_000), 2_002_000_000, 2_000_000_000)
    };
    clock
        .observe_sender_report(second, 0, second.received_at)
        .unwrap();
    let old = clock.project_rtp(first.sample, second.received_at).unwrap();
    let bad = SenderReport {
        uncertainty_ns: 1_000_000,
        ..sr(sample(audio, 96_000), 3_020_000_000, 3_000_000_000)
    };
    assert_eq!(
        clock.observe_sender_report(bad, 96_000, bad.received_at),
        Err(Error::OutOfWindow)
    );
    assert_eq!(
        clock
            .project_rtp(first.sample, bad.received_at)
            .unwrap()
            .global,
        old.global
    );

    let recent = sr(sample(audio, 144_000), 4_004_000_000, 4_030_000_000);
    clock
        .observe_sender_report(recent, 144_000, recent.received_at)
        .unwrap();
    let frontier = clock
        .project_rtp(recent.sample, recent.received_at)
        .unwrap()
        .source
        .unwrap();
    assert!(frontier.as_nanos() - first.source.as_nanos() > 3_000_000_000);
    let e = exchange(&clock, 4_004_000_000, 104_004_000_000, 4_034_000_000);
    clock.observe_exchange(e, frontier, e.reference.at).unwrap();
    assert_eq!(
        clock
            .project_rtp(first.sample, e.reference.at)
            .unwrap()
            .global,
        old.global
    );
    assert_eq!(
        clock.project_rtp(sample(audio, -1), e.reference.at),
        Err(Error::Unresolved)
    );
}
