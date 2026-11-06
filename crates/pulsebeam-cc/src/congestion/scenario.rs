use std::{collections::VecDeque, time::Duration};

use super::{
    ControllerInput, EcnMark, EcnValidation, FeedbackSample, SafeRtpEnvelope, ScreamController,
};

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

const STEP: Duration = Duration::from_millis(10);
const FEEDBACK_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug)]
pub(crate) struct ScenarioMetrics {
    pub(crate) name: &'static str,
    pub(crate) seed: u64,
    pub(crate) utilization_percent: u64,
    pub(crate) admitted_percent: u64,
    pub(crate) p99_queue_micros: u64,
    pub(crate) effective_target_micros: u64,
    pub(crate) application_limited_at_millis: Option<u64>,
    pub(crate) stale_at_millis: Option<u64>,
    pub(crate) stale_recovered: bool,
    pub(crate) window_grew_while_stale: bool,
    pub(crate) baseline_resets: u64,
    pub(crate) ecn_reduced_window: bool,
    pub(crate) invalidated_l4s_mode_disabled: bool,
    pub(crate) loss_reduced_window: bool,
    pub(crate) maximum_native_target_micros: u64,
}

struct Packet {
    epoch: u64,
    sent_at: Duration,
    arrival_at: Duration,
    bytes: u32,
    receiver_arrival_micros: i64,
    received: bool,
    ecn: Option<EcnMark>,
}

struct Simulation {
    controller: ScreamController,
    output: SafeRtpEnvelope,
    now: Duration,
    next_feedback: Duration,
    queue_bytes: u64,
    pending: VecDeque<Packet>,
    delivered_bytes: u64,
    offered_bytes: u64,
    admitted_bytes: u64,
    queue_samples: Vec<u64>,
    maximum_native_target: u64,
}

impl Simulation {
    fn new(desired: u64) -> Self {
        let controller = ScreamController::new(desired, None);
        let output = controller.snapshot(desired, 0, 0);
        Self {
            controller,
            output,
            now: Duration::ZERO,
            next_feedback: FEEDBACK_INTERVAL,
            queue_bytes: 0,
            pending: VecDeque::new(),
            delivered_bytes: 0,
            offered_bytes: 0,
            admitted_bytes: 0,
            queue_samples: Vec::new(),
            maximum_native_target: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn step(
        &mut self,
        bottleneck_bps: u64,
        rtt: Duration,
        offered_bps: u64,
        desired_bps: u64,
        epoch: u64,
        feedback_enabled: bool,
        loss: bool,
        ecn: Option<EcnMark>,
        validation: EcnValidation,
    ) {
        let step_micros = micros(STEP);
        let offered = offered_bps.saturating_mul(step_micros) / 8_000_000;
        let admitted_rate = offered_bps.min(self.output.target_media_payload_rate);
        let sent = admitted_rate.saturating_mul(step_micros) / 8_000_000;
        let service = bottleneck_bps.saturating_mul(step_micros) / 8_000_000;
        self.offered_bytes = self.offered_bytes.saturating_add(offered);
        self.admitted_bytes = self.admitted_bytes.saturating_add(sent);
        self.queue_bytes = self
            .queue_bytes
            .saturating_add(sent)
            .saturating_sub(service);
        let queue_delay = Duration::from_micros(
            self.queue_bytes.saturating_mul(8_000_000) / bottleneck_bps.max(1),
        );
        self.queue_samples.push(micros(queue_delay));
        if sent > 0 {
            let arrival_at = self.now.saturating_add(rtt).saturating_add(queue_delay);
            self.pending.push_back(Packet {
                epoch,
                sent_at: self.now,
                arrival_at,
                bytes: u32::try_from(sent).unwrap_or(u32::MAX),
                receiver_arrival_micros: i64::try_from(
                    micros(self.now)
                        .saturating_add(micros(rtt) / 2)
                        .saturating_add(micros(queue_delay)),
                )
                .unwrap_or(i64::MAX),
                received: !loss,
                ecn,
            });
            self.controller.note_send(self.now);
        }
        // Match the production boundary: preserve committed flight before
        // feedback retires it. Link service was already subtracted above.
        self.controller.observe_bytes_in_flight(
            self.pending
                .iter()
                .map(|packet| u64::from(packet.bytes))
                .sum(),
        );
        self.now = self.now.saturating_add(STEP);
        let mut samples = Vec::new();
        if feedback_enabled && self.now >= self.next_feedback {
            while self
                .pending
                .front()
                .is_some_and(|packet| packet.arrival_at <= self.now)
            {
                let packet = self.pending.pop_front().expect("front checked");
                if packet.epoch != epoch {
                    continue;
                }
                if packet.received {
                    self.delivered_bytes =
                        self.delivered_bytes.saturating_add(u64::from(packet.bytes));
                }
                samples.push(FeedbackSample {
                    sent_at: packet.sent_at,
                    received_at: self.now,
                    transport_bytes: packet.bytes,
                    received: packet.received,
                    // Covered sequence-space gaps advance SCReAM's ACK edge too;
                    // confirmed loss is a separate signal.
                    newly_acked: true,
                    lost: !packet.received,
                    recovered: false,
                    receiver_arrival_micros: packet
                        .received
                        .then_some(packet.receiver_arrival_micros),
                    ecn: packet.ecn,
                });
            }
            self.next_feedback = self.next_feedback.saturating_add(FEEDBACK_INTERVAL);
        }
        let bytes_in_flight = self
            .pending
            .iter()
            .map(|packet| u64::from(packet.bytes))
            .sum();
        self.output = self.controller.update(
            self.now,
            ControllerInput {
                path_epoch: Some(epoch),
                path_available: true,
                feedback: &samples,
                feedback_hold: Duration::ZERO,
                fresh_network_feedback: !samples.is_empty(),
                bytes_in_flight,
                paced_queue_bytes: self.queue_bytes,
                desired_media_rate: desired_bps,
                ecn: validation,
            },
        );
        self.maximum_native_target = self
            .maximum_native_target
            .max(micros(self.output.native_queue_delay_target));
    }

    fn metrics(self, name: &'static str, seed: u64) -> ScenarioMetrics {
        let mut queue = self.queue_samples;
        queue.sort_unstable();
        let index = queue.len().saturating_mul(99) / 100;
        ScenarioMetrics {
            name,
            seed,
            utilization_percent: percent(
                self.delivered_bytes,
                2_000_000 / 8 * micros(self.now) / 1_000_000,
            ),
            admitted_percent: percent(self.admitted_bytes, self.offered_bytes),
            p99_queue_micros: queue
                .get(index.min(queue.len().saturating_sub(1)))
                .copied()
                .unwrap_or(0),
            effective_target_micros: micros(self.output.effective_queue_delay_target),
            application_limited_at_millis: None,
            stale_at_millis: None,
            stale_recovered: false,
            window_grew_while_stale: false,
            baseline_resets: 0,
            ecn_reduced_window: false,
            invalidated_l4s_mode_disabled: false,
            loss_reduced_window: false,
            maximum_native_target_micros: self.maximum_native_target,
        }
    }
}

fn run_fixed_scenario_matrix() -> Vec<ScenarioMetrics> {
    vec![
        baseline(),
        application_limited(),
        stale_feedback(),
        vbr_loss_reorder(),
        path_step(),
        validated_ecn(),
        persistent_loss(),
    ]
}

#[test]
fn fixed_scenario_matrix() {
    for metrics in run_fixed_scenario_matrix() {
        eprintln!(
            "case={} seed={:#06x} metrics={metrics:?} excluded=first-five-rtts,path-steps,probe-clusters,keyframe",
            metrics.name, metrics.seed
        );
        assert!(metrics.maximum_native_target_micros <= 400_000);
        match metrics.seed {
            0x0701 => {
                assert!(metrics.utilization_percent >= 85);
                assert!(metrics.p99_queue_micros <= metrics.effective_target_micros + 10_000);
            }
            0x0702 => {
                // Native ALR needs a reference window established by real
                // service, followed by flight and receive-rate evidence.
                assert!(
                    metrics
                        .application_limited_at_millis
                        .is_some_and(|at| (2_000..=2_400).contains(&at))
                );
                assert!(metrics.admitted_percent >= 95);
            }
            0x0703 => {
                assert!(metrics.stale_at_millis.is_some());
                assert!(!metrics.window_grew_while_stale);
                assert!(metrics.stale_recovered);
            }
            0x0704 => {
                assert!(metrics.p99_queue_micros <= metrics.effective_target_micros + 10_000);
            }
            0x0705 => {
                assert_eq!(metrics.baseline_resets, 1);
            }
            0x0706 => {
                assert!(metrics.ecn_reduced_window);
                assert!(metrics.invalidated_l4s_mode_disabled);
            }
            0x0707 => {
                assert!(metrics.loss_reduced_window);
                assert!(metrics.p99_queue_micros <= metrics.effective_target_micros + 10_000);
            }
            _ => panic!("unreviewed scenario seed"),
        }
    }
}

fn baseline() -> ScenarioMetrics {
    let mut sim = Simulation::new(4_000_000);
    for _ in 0..1_000 {
        sim.step(
            2_000_000,
            Duration::from_millis(50),
            4_000_000,
            4_000_000,
            1,
            true,
            false,
            None,
            EcnValidation::default(),
        );
    }
    sim.metrics("baseline", 0x0701)
}

fn application_limited() -> ScenarioMetrics {
    let mut sim = Simulation::new(2_000_000);
    for _ in 0..200 {
        sim.step(
            2_000_000,
            Duration::from_millis(50),
            4_000_000,
            2_000_000,
            1,
            true,
            false,
            None,
            EcnValidation::default(),
        );
    }
    let offered_before = sim.offered_bytes;
    let admitted_before = sim.admitted_bytes;
    let mut entered = None;
    for _ in 0..1_000 {
        sim.step(
            2_000_000,
            Duration::from_millis(50),
            64_000,
            2_000_000,
            1,
            true,
            false,
            None,
            EcnValidation::default(),
        );
        if sim.output.application_limited && entered.is_none() {
            entered = Some(micros(sim.now) / 1_000);
        }
    }
    let admitted_percent = percent(
        sim.admitted_bytes.saturating_sub(admitted_before),
        sim.offered_bytes.saturating_sub(offered_before),
    );
    let mut metrics = sim.metrics("application-limited", 0x0702);
    metrics.application_limited_at_millis = entered;
    metrics.admitted_percent = admitted_percent;
    metrics
}

fn stale_feedback() -> ScenarioMetrics {
    let mut sim = Simulation::new(4_000_000);
    let mut stale_at = None;
    let mut stale_window = None;
    let mut grew = false;
    let mut recovered = false;
    for _ in 0..1_000 {
        let feedback = sim.now < Duration::from_secs(3) || sim.now >= Duration::from_secs(6);
        sim.step(
            2_000_000,
            Duration::from_millis(100),
            4_000_000,
            4_000_000,
            1,
            feedback,
            false,
            None,
            EcnValidation::default(),
        );
        if sim.output.feedback_stale {
            stale_at.get_or_insert_with(|| micros(sim.now) / 1_000);
            let reference = *stale_window.get_or_insert(sim.output.reference_window);
            grew |= sim.output.reference_window > reference;
        } else if stale_at.is_some() && sim.now >= Duration::from_secs(6) {
            recovered = true;
        }
    }
    let mut metrics = sim.metrics("stale-feedback", 0x0703);
    metrics.stale_at_millis = stale_at;
    metrics.stale_recovered = recovered;
    metrics.window_grew_while_stale = grew;
    metrics
}

fn vbr_loss_reorder() -> ScenarioMetrics {
    let mut sim = Simulation::new(4_000_000);
    for tick in 0..1_500 {
        let keyframe = (500..=550).contains(&tick);
        let offered = if keyframe { 40_000_000 } else { 1_000_000 };
        let loss = tick % 100 == 0;
        sim.step(
            4_000_000,
            Duration::from_millis(80),
            offered,
            4_000_000,
            1,
            true,
            loss,
            None,
            EcnValidation::default(),
        );
        if tick % 50 == 0 && sim.pending.len() >= 2 {
            let last = sim.pending.len() - 1;
            let previous = last - 1;
            let later = sim.pending[last].arrival_at;
            sim.pending[last].arrival_at = sim.pending[previous].arrival_at;
            sim.pending[previous].arrival_at = later;
        }
    }
    sim.metrics("VBR-loss-reorder", 0x0704)
}

fn path_step() -> ScenarioMetrics {
    let mut sim = Simulation::new(4_000_000);
    let mut resets = 0;
    for _ in 0..1_500 {
        let rtt = if sim.now < Duration::from_secs(5) {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(200)
        };
        let epoch = if sim.now < Duration::from_secs(10) {
            1
        } else {
            2
        };
        let previous = sim.controller.path_epoch;
        sim.step(
            2_000_000,
            rtt,
            4_000_000,
            4_000_000,
            epoch,
            true,
            false,
            None,
            EcnValidation::default(),
        );
        if previous.is_some() && sim.controller.path_epoch != previous {
            resets += 1;
        }
    }
    let mut metrics = sim.metrics("RTT-path-step", 0x0705);
    metrics.baseline_resets = resets;
    metrics
}

fn validated_ecn() -> ScenarioMetrics {
    let mut sim = Simulation::new(4_000_000);
    let mut reduced = false;
    let mut disabled = false;
    for tick in 0..1_000 {
        let in_ce = (300..500).contains(&tick);
        let bleached = (500..700).contains(&tick);
        let before_ce = sim.output.reference_window;
        sim.step(
            2_000_000,
            Duration::from_millis(50),
            4_000_000,
            4_000_000,
            1,
            true,
            false,
            in_ce.then_some(EcnMark::Ce),
            EcnValidation {
                classic: true,
                l4s: true,
                bleached,
            },
        );
        reduced |= matches!(
            sim.output.reason,
            super::ControllerReason::L4s | super::ControllerReason::ClassicEcn
        ) && sim.output.reference_window < before_ce;
        disabled |= bleached && !sim.output.l4s_enabled;
    }
    let mut metrics = sim.metrics("validated-ECN-modes", 0x0706);
    metrics.ecn_reduced_window = reduced;
    // Invalidation is a supplied adapter fact, not a native bleach detector.
    metrics.invalidated_l4s_mode_disabled = disabled;
    metrics
}

fn persistent_loss() -> ScenarioMetrics {
    let mut sim = Simulation::new(8_000_000);
    let mut reduced = false;
    for tick in 0..1_500 {
        let loss = tick > 300 && tick % 8 == 0;
        let before_loss = sim.output.reference_window;
        sim.step(
            2_000_000,
            Duration::from_millis(50),
            8_000_000,
            8_000_000,
            1,
            true,
            loss,
            None,
            EcnValidation::default(),
        );
        reduced |= sim.output.reason == super::ControllerReason::Loss
            && sim.output.reference_window < before_loss;
    }
    let mut metrics = sim.metrics("persistent-loss", 0x0707);
    metrics.loss_reduced_window = reduced;
    metrics
}

fn percent(value: u64, total: u64) -> u64 {
    value.saturating_mul(100) / total.max(1)
}
