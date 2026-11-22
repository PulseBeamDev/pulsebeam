use crate::{Error, domain::*, time::*};

mod adversarial;
mod boundary_witnesses;
mod exact_arithmetic_qa;
mod lifecycle;
mod recovery_qa;
mod red_qa;
mod regressions;
mod scenario;

pub(super) const CONFIG: Config = Config {
    track_capacity: 2,
    nominal_delay_ns: 20_000_000,
    max_past_ns: 5_000_000_000,
    max_future_ns: 100_000_000,
    observation_max_age_ns: 1_000_000_000,
    max_drift_ppm: 1_000,
    max_exchange_rtt_ns: 200_000_000,
    min_fit_span_ns: 100_000_000,
    lock_uncertainty_ns: 2_000_000,
    reorder_ns: 3_000_000_000,
    history_capacity: 16,
    slew_ppm: 10_000,
    max_phase_error_ns: 150_000_000,
};

fn at(ns: u64) -> MonotonicTime {
    MonotonicTime::from_nanos(ns)
}

fn domain(id: u64) -> ClockDomain {
    ClockDomain::new(
        CONFIG,
        Binding {
            domain: DomainId(id),
            generation: Generation(7),
        },
        LocalOrigin {
            at: at(0),
            global: GlobalInstant::from_nanos(100_000_000_000),
        },
    )
    .unwrap()
}

fn sample(binding: RtpBinding, tick: i128) -> RtpSample {
    RtpSample { binding, tick }
}

fn sr(sample: RtpSample, ns: i64, received_at: u64) -> SenderReport {
    SenderReport {
        sample,
        source: SourceTime::from_nanos(ns),
        uncertainty_ns: 0,
        received_at: at(received_at),
    }
}

#[test]
fn passive_start_is_read_only_and_has_no_source_alignment_claim() {
    let mut clock = domain(6);
    let audio = clock.bind_track(0, ClockRate::OPUS).unwrap();
    let video = clock.bind_track(1, ClockRate::VIDEO).unwrap();
    let receipt = at(100_000_000);
    let first = sample(audio, 4_294_967_280);
    assert_eq!(clock.project_rtp(first, receipt), Err(Error::Unresolved));
    clock.bootstrap_track(first, receipt, receipt).unwrap();
    clock
        .bootstrap_track(sample(video, 900), at(115_000_000), at(115_000_000))
        .unwrap();
    let latest = sample(audio, 4_294_969_680);
    let later = at(170_000_000);
    let forward = clock.project_rtp(latest, later).unwrap();
    let reorder = clock.project_rtp(first, later).unwrap();
    assert_eq!(forward.global.as_nanos(), 100_130_000_000);
    assert_eq!(reorder.global.as_nanos(), 100_080_000_000);
    assert_eq!(forward, clock.project_rtp(latest, at(180_000_000)).unwrap());
    assert_eq!(reorder.provenance, Provenance::ArrivalEstimate);
    assert_eq!(reorder.source, None);
    assert_eq!(reorder.absolute_uncertainty_ns, None);
    assert_eq!(
        clock.project_rtp(sample(video, 900), later).unwrap().source,
        None
    );
    assert_eq!(clock.bind_track(0, ClockRate::OPUS), Err(Error::Invalid));
    assert_eq!(
        clock.bind_track(2, ClockRate::OPUS),
        Err(Error::OutOfWindow)
    );
}

#[test]
fn reports_establish_relative_timing_without_global_authority() {
    let mut clock = domain(1);
    let audio = clock.bind_track(0, ClockRate::OPUS).unwrap();
    let video = clock.bind_track(1, ClockRate::VIDEO).unwrap();
    let a1 = sr(sample(audio, 0), 10_000_000_000, 1_000_000_000);
    let v1 = sr(sample(video, 9_000), 9_900_000_000, 1_015_000_000);
    clock
        .observe_sender_report(a1, a1.sample.tick, a1.received_at)
        .unwrap();
    clock
        .observe_sender_report(v1, v1.sample.tick, v1.received_at)
        .unwrap();
    let a2 = sr(sample(audio, 48_000), 11_000_500_000, 2_000_000_000);
    clock
        .observe_sender_report(a2, a1.sample.tick, a2.received_at)
        .unwrap();
    let now = at(2_100_000_000);
    let a0 = clock.project_rtp(a1.sample, now).unwrap();
    let v0 = clock.project_rtp(sample(video, 18_000), now).unwrap();
    assert_eq!(a0.source, Some(SourceTime::from_nanos(10_000_000_000)));
    assert_eq!(a0.source, v0.source);
    assert_eq!(a0.global, v0.global);
    assert_eq!(a0.quality, Quality::Acquiring);
    assert_eq!(a0.absolute_uncertainty_ns, None);
    assert_eq!(
        clock.project_rtp(a2.sample, now).unwrap().source,
        Some(a2.source)
    );
    let bad = sr(sample(audio, 96_000), 20_000_000_000, 3_000_000_000);
    assert_eq!(
        clock.observe_sender_report(bad, a2.sample.tick, bad.received_at),
        Err(Error::OutOfWindow)
    );
    assert_eq!(
        clock.project_rtp(a2.sample, now).unwrap().source,
        Some(a2.source)
    );

    let mut other = domain(2);
    let other_audio = other.bind_track(0, ClockRate::OPUS).unwrap();
    assert_eq!(
        other.observe_sender_report(a1, a1.sample.tick, a1.received_at),
        Err(Error::WrongBinding)
    );
    let unrelated = sr(sample(other_audio, 0), 10_000_000_000, 1_050_000_000);
    other
        .observe_sender_report(unrelated, unrelated.sample.tick, unrelated.received_at)
        .unwrap();
    assert_ne!(
        other.project_rtp(unrelated.sample, now).unwrap().global,
        a0.global
    );
    assert_eq!(
        clock.project_rtp(unrelated.sample, now),
        Err(Error::WrongBinding)
    );
}

fn exchange(clock: &ClockDomain, source: i64, center: i64, received: u64) -> ActiveExchange {
    ActiveExchange {
        binding: clock.binding(),
        source: SourceTime::from_nanos(source),
        sent_at: at(received.checked_sub(60_000_000).unwrap()),
        reference: ReferenceSample {
            at: at(received),
            global: GlobalInstant::from_nanos(center.checked_add(30_000_000).unwrap()),
            quality: ReferenceQuality::Qualified {
                uncertainty_ns: 20_000,
            },
        },
        asymmetry_bound_ns: Some(40_000),
    }
}

fn receipt(at: MonotonicTime) -> DataReceipt {
    let global = 100_000_000_000_i64
        .checked_add(i64::try_from(at.as_nanos()).unwrap())
        .unwrap();
    DataReceipt {
        at,
        latest_global: Some(GlobalInstant::from_nanos(global)),
    }
}

fn data(clock: &ClockDomain, source: i64, now: u64) -> Projection {
    clock
        .project_data(
            Some(DataCapture {
                binding: clock.binding(),
                source: SourceTime::from_nanos(source),
                uncertainty_ns: 500,
            }),
            receipt(at(now)),
            at(now),
        )
        .unwrap()
}

#[test]
fn qualified_exchange_estimates_drift_and_bounds_truth() {
    let mut clock = domain(4);
    let mut other = domain(5);
    let a = exchange(&clock, 1_000_000_000, 100_970_000_000, 1_000_000_000);
    let b = exchange(&clock, 2_000_000_000, 101_970_200_000, 2_000_200_000);
    clock
        .observe_passive(
            PassiveReceipt {
                binding: clock.binding(),
                source: a.source,
                received_at: a.reference.at,
            },
            a.reference.at,
        )
        .unwrap();
    let boot = data(&clock, a.source.as_nanos(), 1_000_000_000);
    assert_eq!(boot.global.as_nanos(), 100_980_000_000);
    assert_eq!(
        other.observe_exchange(a, a.source, a.reference.at),
        Err(Error::WrongBinding)
    );
    clock.observe_exchange(a, a.source, a.reference.at).unwrap();
    assert_eq!(
        data(&clock, a.source.as_nanos(), 1_000_000_000).quality,
        Quality::Recovering
    );
    clock.observe_exchange(b, b.source, b.reference.at).unwrap();
    let mapped = data(&clock, 2_200_000_000, 2_210_200_000);
    let truth = 102_170_240_000;
    assert_eq!(mapped.global.as_nanos(), truth);
    assert_eq!(mapped.quality, Quality::Synchronized);
    assert!(mapped.absolute_uncertainty_ns.unwrap() >= 240_000);
    assert_eq!(mapped.calibration.unwrap().fitted_drift_ppm, 200);
    assert_eq!(mapped.calibration.unwrap().evidence, Evidence::Exchange);
    assert_eq!(data(&other, 2_200_000_000, 2_210_200_000).source, None);
    let bad = exchange(&clock, 3_000_000_000, 103_000_000_000, 3_000_000_000);
    for _ in 0..128 {
        assert_eq!(
            clock.observe_exchange(bad, bad.source, bad.reference.at),
            Err(Error::OutOfWindow)
        );
    }
    assert_eq!(data(&clock, 2_200_000_000, 2_210_200_000), mapped);
    assert_eq!(
        clock.observe_exchange(a, a.source, at(2_100_000_000)),
        Err(Error::Stale)
    );
}

#[test]
fn unknown_asymmetry_and_act_cannot_authorize_lock() {
    let mut clock = domain(4);
    let a = ActiveExchange {
        asymmetry_bound_ns: None,
        ..exchange(&clock, 1_000_000_000, 100_970_000_000, 1_000_000_000)
    };
    let b = ActiveExchange {
        asymmetry_bound_ns: None,
        ..exchange(&clock, 2_000_000_000, 101_970_000_000, 2_000_000_000)
    };
    clock
        .observe_act(
            ActHint {
                binding: clock.binding(),
                source: SourceTime::from_nanos(900_000_000),
                global_estimate: GlobalInstant::from_nanos(100_860_000_000),
                observed_at: at(900_000_000),
                conditional_error_ns: 1,
            },
            SourceTime::from_nanos(900_000_000),
            at(900_000_000),
        )
        .unwrap();
    let conditional = data(&clock, 900_000_000, 900_000_000);
    assert_eq!(conditional.calibration.unwrap().evidence, Evidence::Act);
    assert_eq!(conditional.absolute_uncertainty_ns, None);
    clock.observe_exchange(a, a.source, a.reference.at).unwrap();
    clock.observe_exchange(b, b.source, b.reference.at).unwrap();
    let mapped = data(&clock, 2_000_000_000, 2_000_000_000);
    assert_eq!(mapped.quality, Quality::Degraded);
    let true_time_after_asymmetry = 101_999_999_900;
    assert!(
        mapped.global.as_nanos().abs_diff(true_time_after_asymmetry)
            <= mapped.absolute_uncertainty_ns.unwrap()
    );
    let wrong = ActiveExchange {
        asymmetry_bound_ns: Some(u64::MAX),
        ..b
    };
    assert_eq!(
        clock.observe_exchange(wrong, wrong.source, b.reference.at),
        Err(Error::Invalid)
    );
}

#[test]
fn data_reuses_only_its_bound_source_and_otherwise_degrades() {
    let mut clock = domain(3);
    let now = at(1_000_000_000);
    let source = SourceTime::from_nanos(-8_000_000_000);
    let capture = DataCapture {
        binding: clock.binding(),
        source,
        uncertainty_ns: 1_000,
    };
    let fallback = clock
        .project_data(Some(capture), receipt(now), now)
        .unwrap();
    assert_eq!(
        fallback,
        clock.project_data(None, receipt(now), now).unwrap()
    );
    assert_eq!(fallback.source, None);
    clock
        .observe_passive(
            PassiveReceipt {
                binding: clock.binding(),
                source,
                received_at: now,
            },
            now,
        )
        .unwrap();
    let mapped = clock
        .project_data(Some(capture), receipt(now), now)
        .unwrap();
    assert_eq!(mapped.source, Some(source));
    assert_eq!(mapped.provenance, Provenance::DataCapture);
    assert_eq!(mapped.absolute_uncertainty_ns, None);
    assert_eq!(mapped.global.as_nanos(), 100_980_000_000);
    let old = Binding {
        generation: Generation(6),
        ..clock.binding()
    };
    assert_eq!(
        clock.project_data(
            Some(DataCapture {
                binding: old,
                ..capture
            }),
            receipt(now),
            now
        ),
        Err(Error::WrongBinding)
    );
    assert_eq!(
        clock.project_data(None, receipt(at(0)), at(1_000_000_001)),
        Err(Error::Stale)
    );
}
