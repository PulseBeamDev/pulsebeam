use super::*;

#[test]
fn track_switch_is_continuous_and_keeps_fallback_reordering() {
    let mut clock = domain(12);
    let audio = clock.bind_track(0, ClockRate::OPUS).unwrap();
    let first = sample(audio, 0);
    let frontier = sample(audio, 4_800);
    clock
        .bootstrap_track(first, at(100_000_000), at(100_000_000))
        .unwrap();
    let now = at(200_000_000);
    let before = clock.project_rtp(frontier, now).unwrap();
    let old = clock.project_rtp(first, now).unwrap();
    let report = sr(frontier, 160_000_000, at(180_000_000).as_nanos());
    clock
        .observe_sender_report(report, frontier.tick, now)
        .unwrap();
    assert_eq!(
        clock.project_rtp(frontier, now).unwrap().global,
        before.global
    );
    assert_eq!(clock.project_rtp(first, now).unwrap(), old);
    let mut last = before;
    for n in 1_i128..=40 {
        let tick = 4_800 + n * 2_400;
        let elapsed = u64::try_from(n).unwrap() * 50_000_000;
        let item = clock
            .project_rtp(sample(audio, tick), at(200_000_000 + elapsed))
            .unwrap();
        let cadence = item.global.duration_since(last.global).unwrap().as_nanos();
        assert!((49_500_000..=50_000_000).contains(&cadence));
        last = item;
    }
    assert_eq!(last.source, Some(SourceTime::from_nanos(2_160_000_000)));
    assert_eq!(last.global.as_nanos(), 102_160_000_000);
}

#[test]
fn reference_loss_step_and_recovery_never_step_projection() {
    let mut clock = domain(13);
    let binding = clock.binding();
    let a = exchange(&clock, 1_000_000_000, 100_970_000_000, 1_000_000_000);
    let b = exchange(&clock, 2_000_000_000, 101_970_200_000, 2_000_200_000);
    clock.observe_exchange(a, a.source, a.reference.at).unwrap();
    clock.observe_exchange(b, b.source, b.reference.at).unwrap();
    let live = data(&clock, 2_200_000_000, 2_210_200_000);
    assert_eq!(live.quality, Quality::Synchronized);
    clock
        .set_reference_state(binding, at(2_500_000_000), ReferenceState::Lost)
        .unwrap();
    let held = data(&clock, 3_200_000_000, 3_210_200_000);
    assert_eq!(held.quality, Quality::Holdover);
    assert!(held.absolute_uncertainty_ns.unwrap() > live.absolute_uncertainty_ns.unwrap());
    assert_eq!(
        held.global.duration_since(live.global).unwrap().as_nanos(),
        1_000_200_000
    );
    clock
        .set_reference_state(binding, at(3_250_000_000), ReferenceState::Stepped)
        .unwrap();
    let c = exchange(&clock, 4_000_000_000, 103_975_600_000, 4_000_600_000);
    assert_eq!(
        clock.observe_exchange(c, c.source, c.reference.at),
        Err(Error::Unresolved)
    );
    let stepped = data(&clock, 4_000_000_000, 4_000_600_000);
    assert_eq!(stepped.quality, Quality::Degraded);
    assert_eq!(stepped.absolute_uncertainty_ns, None);
    assert_eq!(stepped.global.as_nanos(), 103_970_600_000);

    clock
        .set_reference_state(binding, at(3_500_000_000), ReferenceState::Usable)
        .unwrap();
    let reordered = data(&clock, 3_000_000_000, 4_000_600_000);
    clock.observe_exchange(c, c.source, c.reference.at).unwrap();
    assert_eq!(
        data(&clock, 3_000_000_000, 4_000_600_000).global,
        reordered.global
    );
    let frontier = data(&clock, 4_000_000_000, 4_000_600_000);
    assert_eq!(frontier.global, stepped.global);
    assert_eq!(frontier.quality, Quality::Recovering);
    assert_eq!(
        frontier.calibration.unwrap().phase_correction_ns,
        -5_000_000
    );
    let d = exchange(&clock, 4_800_000_000, 104_775_760_000, 4_800_760_000);
    clock.observe_exchange(d, d.source, d.reference.at).unwrap();
    let recovered = data(&clock, 4_900_000_000, 4_910_760_000);
    assert_eq!(recovered.quality, Quality::Synchronized);
    assert_eq!(recovered.global.as_nanos(), 104_875_780_000);
}

#[test]
fn history_has_a_cap_and_reset_fences_old_incarnations() {
    let mut clock = ClockDomain::new(
        Config {
            history_capacity: 2,
            reorder_ns: 500_000_000,
            ..CONFIG
        },
        Binding {
            domain: DomainId(14),
            generation: Generation(0),
        },
        LocalOrigin {
            at: at(0),
            global: GlobalInstant::from_nanos(100_000_000_000),
        },
    )
    .unwrap();
    let old = clock.binding();
    let a = exchange(&clock, 1_000_000_000, 100_970_000_000, 1_000_000_000);
    let b = exchange(&clock, 1_200_000_000, 101_170_020_000, 1_200_000_000);
    let c = exchange(&clock, 1_400_000_000, 101_370_080_000, 1_400_000_000);
    clock.observe_exchange(a, a.source, a.reference.at).unwrap();
    clock.observe_exchange(b, b.source, b.reference.at).unwrap();
    assert_eq!(
        clock.observe_exchange(c, c.source, c.reference.at),
        Err(Error::Capacity)
    );
    assert_eq!(
        data(&clock, 1_200_000_000, 1_400_000_000)
            .calibration
            .unwrap()
            .observation_age_ns,
        200_000_000
    );
    let later = exchange(&clock, 1_800_000_000, 101_770_080_000, 1_800_000_000);
    clock
        .observe_exchange(later, later.source, later.reference.at)
        .unwrap();
    let expired = DataCapture {
        binding: old,
        source: a.source,
        uncertainty_ns: 0,
    };
    assert_eq!(
        clock.project_data(
            Some(expired),
            receipt(later.reference.at),
            later.reference.at
        ),
        Err(Error::Unresolved)
    );
    let track = clock.bind_track(0, ClockRate::OPUS).unwrap();
    let next = Binding {
        generation: Generation(1),
        ..old
    };
    assert_eq!(
        clock.reset_generation(old, later.reference.at),
        Err(Error::WrongBinding)
    );
    clock.reset_generation(next, later.reference.at).unwrap();
    assert_eq!(
        clock.project_rtp(sample(track, 0), later.reference.at),
        Err(Error::WrongBinding)
    );
    assert_eq!(
        clock.project_data(
            Some(expired),
            receipt(later.reference.at),
            later.reference.at
        ),
        Err(Error::WrongBinding)
    );
    assert_eq!(
        clock.observe_exchange(later, later.source, later.reference.at),
        Err(Error::WrongBinding)
    );
    let fresh = clock.bind_track(0, ClockRate::OPUS).unwrap();
    let fresh = sample(fresh, 0);
    clock
        .bootstrap_track(fresh, later.reference.at, later.reference.at)
        .unwrap();
    assert_eq!(
        clock
            .project_rtp(fresh, later.reference.at)
            .unwrap()
            .binding,
        next
    );
}
