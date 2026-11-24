use super::*;

const GLOBAL_ORIGIN: i64 = 100_000_000_000;
const FRAME_NS: i64 = 100_000_000;
const EARLIEST_RECOVERY_STEP: i64 = 40;
// The trace injects <= 10 ms phase error. At 10,000 ppm, allow 1 s to
// slew, then two 0.5 s exchange intervals to establish a fresh fit.
const RECOVERY_GATE_STEP: i64 = 60;

#[derive(Clone, Copy, Debug)]
struct Publisher {
    id: u64,
    source_origin: i64,
    global_per_source_ppm: i64,
}

impl Publisher {
    fn source(self, step: i64) -> i64 {
        self.source_origin
            .checked_add(step.checked_mul(FRAME_NS).unwrap())
            .unwrap()
    }

    fn truth(self, step: i64) -> i64 {
        let frame_ns = self
            .global_per_source_ppm
            .checked_mul(100)
            .unwrap()
            .checked_add(FRAME_NS)
            .unwrap();
        GLOBAL_ORIGIN
            .checked_add(step.checked_mul(frame_ns).unwrap())
            .unwrap()
    }

    fn tick(step: i64, rate: ClockRate) -> i128 {
        i128::from(step)
            .checked_mul(i128::from(rate.hz() / 10))
            .unwrap()
    }

    fn arrived(self, step: i64, extra: i64) -> MonotonicTime {
        let elapsed = self
            .truth(step)
            .checked_sub(GLOBAL_ORIGIN)
            .unwrap()
            .checked_add(extra)
            .unwrap();
        at(u64::try_from(elapsed).unwrap())
    }

    fn data(self, clock: &ClockDomain, step: i64, now: MonotonicTime) -> Projection {
        clock
            .project_data(
                Some(DataCapture {
                    binding: clock.binding(),
                    source: SourceTime::from_nanos(self.source(step)),
                    uncertainty_ns: 300,
                }),
                receipt(now),
                now,
            )
            .unwrap()
    }

    fn report(self, track: RtpBinding, step: i64, delay: i64) -> SenderReport {
        sr(
            sample(track, Self::tick(step, track.rate)),
            self.source(step),
            self.arrived(step, delay).as_nanos(),
        )
    }

    fn exchange(self, clock: &ClockDomain, step: i64, bias_ns: i64) -> ActiveExchange {
        let reverse_ns = 30_000_000_i64.checked_add(bias_ns).unwrap();
        ActiveExchange {
            asymmetry_bound_ns: Some(200_000),
            ..exchange(
                clock,
                self.source(step),
                self.truth(step).checked_add(bias_ns).unwrap(),
                self.arrived(step, reverse_ns).as_nanos(),
            )
        }
    }
}

#[derive(Default)]
struct Errors {
    absolute_ns: Vec<u64>,
    relative_ns: Vec<u64>,
    bounded_samples: usize,
}

impl Errors {
    fn record(&mut self, estimates: [Projection; 3], truth: i64, replay: (u64, i64)) -> u64 {
        let mut worst = 0;
        for estimate in estimates {
            let error = estimate.global.as_nanos().abs_diff(truth);
            if let Some(bound) = estimate.absolute_uncertainty_ns {
                self.bounded_samples = self.bounded_samples.checked_add(1).unwrap();
                assert!(
                    error <= bound,
                    "replay {replay:?}: {error} > {bound}: {estimate:?}"
                );
            }
            self.absolute_ns.push(error);
            worst = worst.max(error);
        }
        let [a, v, d] = estimates.map(|estimate| estimate.global.as_nanos());
        self.relative_ns
            .extend([a.abs_diff(v), a.abs_diff(d), v.abs_diff(d)]);
        worst
    }

    fn percentiles(values: &mut [u64]) -> [u64; 4] {
        values.sort_unstable();
        [950, 990, 999, 1000].map(|permille| {
            let rank = values.len().checked_mul(permille).unwrap().div_ceil(1000);
            *values.get(rank.checked_sub(1).unwrap()).unwrap()
        })
    }

    fn report(mut self, label: &str, convergence_step: Option<i64>) {
        let absolute = Self::percentiles(&mut self.absolute_ns);
        let relative = Self::percentiles(&mut self.relative_ns);
        println!(
            "{label}: ns [p95, p99, p99.9, max], absolute {absolute:?}, relative {relative:?}; bounds covered {}/{} samples; convergence step {convergence_step:?}",
            self.bounded_samples,
            self.absolute_ns.len(),
        );
    }
}

#[test]
fn two_publishers_converge_through_loss_and_reference_step() {
    let publishers = [
        Publisher {
            id: 91,
            source_origin: 700_000_000_000,
            global_per_source_ppm: 230,
        },
        Publisher {
            id: 92,
            source_origin: -310_000_000_000,
            global_per_source_ppm: -175,
        },
    ];
    let mut worlds = publishers.map(|publisher| {
        let mut clock = domain(publisher.id);
        let audio = clock.bind_track(0, ClockRate::OPUS).unwrap();
        let video = clock.bind_track(1, ClockRate::VIDEO).unwrap();
        (clock, [audio, video], None::<[Projection; 3]>, None)
    });
    let [
        mut startup,
        mut healthy,
        mut held,
        mut stepped,
        mut recovered,
    ] = std::array::from_fn(|_| Errors::default());

    for step in 1..=75 {
        for (world, publisher) in worlds.iter_mut().zip(publishers) {
            let (clock, tracks, previous, convergence) = world;
            let binding = clock.binding();
            let now = publisher.arrived(step, 42_000_000);
            let samples = tracks.map(|track| sample(track, Publisher::tick(step, track.rate)));

            if step == 1 {
                for (track_number, sample) in samples.into_iter().enumerate() {
                    let arrival = publisher.arrived(
                        step,
                        22_000_000 + i64::try_from(track_number).unwrap() * 3_000_000,
                    );
                    clock.bootstrap_track(sample, arrival, now).unwrap();
                }
            }
            if step == 2 || step % 8 == 0 && step != 32 {
                for track in *tracks {
                    let delay = 20_000_000 + step.rem_euclid(7) * 50_000;
                    let report = publisher.report(track, step, delay);
                    clock
                        .observe_sender_report(report, report.sample.tick, now)
                        .unwrap();
                }
            }
            if step == 23 {
                let mut bad = publisher.report(tracks[0], step, 20_000_000);
                bad.source = SourceTime::from_nanos(publisher.source(step) + 5_000_000_000);
                assert_eq!(
                    clock.observe_sender_report(bad, bad.sample.tick, now),
                    Err(Error::OutOfWindow)
                );
            }

            match step {
                26 => clock
                    .set_reference_state(binding, now, ReferenceState::Lost)
                    .unwrap(),
                38 => clock
                    .set_reference_state(binding, now, ReferenceState::Stepped)
                    .unwrap(),
                39 => {
                    let mut bad = publisher.exchange(clock, step, 0);
                    bad.reference.global =
                        GlobalInstant::from_nanos(publisher.truth(step) + 500_000_000);
                    assert_eq!(
                        clock.observe_exchange(bad, bad.source, now),
                        Err(Error::Unresolved)
                    );
                    clock
                        .set_reference_state(binding, now, ReferenceState::Usable)
                        .unwrap();
                }
                _ => {}
            }
            if step == 3 {
                let source = SourceTime::from_nanos(publisher.source(step));
                let hint = ActHint {
                    binding,
                    source,
                    global_estimate: GlobalInstant::from_nanos(publisher.truth(step) + 5_000_000),
                    observed_at: now,
                    conditional_error_ns: 6_000_000,
                };
                clock.observe_act(hint, source, now).unwrap();
            }
            if step % 5 == 0 && !(26..EARLIEST_RECOVERY_STEP).contains(&step) {
                // Vary actual path asymmetry deterministically, without
                // feeding the simulated true capture instant to the estimator.
                let jitter = (step.rem_euclid(3) - 1) * 60_000;
                let exchange = publisher.exchange(clock, step, jitter);
                clock
                    .observe_exchange(exchange, exchange.source, now)
                    .unwrap();
            }
            let [audio, video] = samples.map(|sample| clock.project_rtp(sample, now).unwrap());
            let data_stamp = publisher.data(clock, step, now);
            let estimates = [audio, video, data_stamp];
            if let Some(previous) = previous.replace(estimates) {
                for (before, after) in previous.into_iter().zip(estimates) {
                    assert!(
                        after.global > before.global,
                        "replay {:?} step {step}: {before:?} {after:?}",
                        publisher.id
                    );
                }
                // One-frame late, through both domain and per-track corrections.
                for (track, first) in tracks.iter().zip(previous) {
                    let late = sample(*track, Publisher::tick(step - 1, track.rate));
                    assert_eq!(
                        clock.project_rtp(late, now).unwrap().rtp,
                        first.rtp,
                        "replay {:?} step {step}",
                        publisher.id
                    );
                }
            }
            assert_eq!(clock.project_rtp(samples[0], now).unwrap(), audio);

            let replay = (publisher.id, step);
            let truth = publisher.truth(step);
            if step == 1 {
                assert!(
                    estimates
                        .iter()
                        .all(|estimate| estimate.quality == Quality::Degraded)
                );
                assert!(estimates.iter().all(|estimate| estimate.source.is_none()
                    && estimate.absolute_uncertainty_ns.is_none()));
                // Arrival-only startup need not estimate true capture.
                startup.record(estimates, truth, replay);
            } else if step < 26 {
                healthy.record(estimates, truth, replay);
            } else if step < 38 {
                assert!(
                    estimates
                        .iter()
                        .all(|estimate| estimate.quality == Quality::Holdover)
                );
                held.record(estimates, truth, replay);
            } else if step < EARLIEST_RECOVERY_STEP {
                assert!(
                    estimates
                        .iter()
                        .all(|estimate| estimate.absolute_uncertainty_ns.is_none())
                );
                stepped.record(estimates, truth, replay);
            } else {
                let error = recovered.record(estimates, truth, replay);
                let converged = error <= 1_000_000
                    && estimates
                        .iter()
                        .all(|estimate| estimate.quality == Quality::Synchronized);
                if converged {
                    convergence.get_or_insert(step);
                } else {
                    *convergence = None;
                }
                if step >= RECOVERY_GATE_STEP {
                    assert!(
                        converged,
                        "clean recovery {replay:?} exceeded declared horizon/error: {estimates:?}"
                    );
                }
            }
        }
        let ((first, ..), (second, tracks, ..)) = (&worlds[0], &worlds[1]);
        let foreign = sample(tracks[0], Publisher::tick(step, tracks[0].rate));
        assert_ne!(first.binding(), second.binding());
        assert_eq!(
            first.project_rtp(foreign, publishers[0].arrived(step, 42_000_000)),
            Err(Error::WrongBinding)
        );
    }

    for (clock, tracks, _, convergence) in &mut worlds {
        let previous_generation = clock.binding();
        let reset_at = at(8_000_000_000);
        let replacement = Binding {
            generation: Generation(8),
            ..previous_generation
        };
        let old = *tracks;
        clock.reset_generation(replacement, reset_at).unwrap();
        assert_eq!(
            clock.project_rtp(sample(old[0], 0), reset_at),
            Err(Error::WrongBinding)
        );
        let fresh = sample(clock.bind_track(0, ClockRate::OPUS).unwrap(), 0);
        clock.bootstrap_track(fresh, reset_at, reset_at).unwrap();
        assert_eq!(
            clock.project_rtp(fresh, reset_at).unwrap().quality,
            Quality::Degraded
        );
        println!(
            "domain {:?} first sustained recovery at step {convergence:?}",
            replacement.domain
        );
    }
    startup.report("arrival-only", None);
    healthy.report("acquiring/calibrated", None);
    held.report("holdover", None);
    stepped.report("stepped", None);
    recovered.report(
        "recovery",
        worlds.iter().filter_map(|(_, _, _, step)| *step).max(),
    );
}

#[test]
fn predicted_capture_across_multiple_virtual_wraps_at_each_rate() {
    let publisher = Publisher {
        id: 21,
        source_origin: 2_000_000_000,
        global_per_source_ppm: 250,
    };
    let mut clock = domain(publisher.id);
    let tracks = [
        clock.bind_track(0, ClockRate::OPUS).unwrap(),
        clock.bind_track(1, ClockRate::VIDEO).unwrap(),
    ];
    let now = publisher.arrived(1, 42_000_000);
    for track in tracks {
        let report = publisher.report(track, 1, 20_000_000);
        clock
            .observe_sender_report(report, report.sample.tick, now)
            .unwrap();
    }
    for step in [5, 10] {
        let exchange = publisher.exchange(&clock, step, 0);
        clock
            .observe_exchange(
                exchange,
                exchange.source,
                publisher.arrived(step, 42_000_000),
            )
            .unwrap();
    }

    let window = UnwrapWindow::new(9_000, 9_000, 1_000_000_000).unwrap();
    let mut wrap_counts = [0; 2];
    for step in [480_000, 900_000, 1_000_000, 1_800_000, 1_900_000] {
        let mono = publisher.arrived(step, 40_000_000);
        for ((count, track), rate) in wrap_counts
            .iter_mut()
            .zip(tracks)
            .zip([48_000_i128, 90_000])
        {
            let ext = Publisher::tick(step, track.rate);
            let anchor = UnwrapAnchor::new(
                track,
                ext - rate / 10,
                publisher.arrived(step - 1, 40_000_000),
            );
            let wire = RtpTimestamp(u32::try_from(ext.rem_euclid(4_294_967_296)).unwrap());
            let ext = anchor.unwrap(track, wire, mono, window).unwrap();
            let projected = clock.project_rtp(sample(track, ext), mono).unwrap();
            assert_eq!(projected.quality, Quality::Holdover);
            let truth = publisher.truth(step);
            assert_eq!(projected.global.as_nanos(), truth);
            assert_eq!(projected.binding, clock.binding());
            let expected = (i128::from(truth) * rate)
                .div_euclid(1_000_000_000)
                .rem_euclid(4_294_967_296);
            assert_eq!(i128::from(projected.rtp.0), expected);
            *count = ext.div_euclid(4_294_967_296);
        }
    }
    assert_eq!(wrap_counts, [2, 3]);
}
