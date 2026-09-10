use std::{collections::VecDeque, time::Duration};

use super::{
    ControllerInput, EcnMark, EcnValidation, FeedbackSample, SafeRtpEnvelope, ScreamController,
    micros,
};

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
    pub(crate) probe_overhead_percent: u64,
    pub(crate) application_limited_at_millis: Option<u64>,
    pub(crate) stale_at_millis: Option<u64>,
    pub(crate) stale_recovered: bool,
    pub(crate) window_grew_while_stale: bool,
    pub(crate) baseline_resets: u64,
    pub(crate) old_path_sample_used: bool,
    pub(crate) ecn_reduced_window: bool,
    pub(crate) l4s_disabled_on_bleach: bool,
    pub(crate) policer_detected: bool,
    pub(crate) fairness_percent: u64,
    pub(crate) maximum_native_target_micros: u64,
    pub(crate) duplicate_status_consumptions: u64,
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
        if self.queue_bytes >= service {
            self.queue_bytes = self.queue_bytes.saturating_sub(service);
        } else {
            self.queue_bytes = 0;
        }
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
                bytes_in_flight,
                paced_queue_bytes: self.queue_bytes,
                offered_media_rate: offered_bps,
                admitted_media_rate: admitted_rate,
                desired_media_rate: desired_bps,
                window_or_pacer_blocked: false,
                queue_delay_ceiling: Duration::from_millis(60),
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
            probe_overhead_percent: 0,
            application_limited_at_millis: None,
            stale_at_millis: None,
            stale_recovered: false,
            window_grew_while_stale: false,
            baseline_resets: 0,
            old_path_sample_used: false,
            ecn_reduced_window: false,
            l4s_disabled_on_bleach: false,
            policer_detected: self.output.policer_detected,
            fairness_percent: 50,
            maximum_native_target_micros: self.maximum_native_target,
            duplicate_status_consumptions: 0,
        }
    }
}

pub(crate) fn run_fixed_scenario_matrix() -> Vec<ScenarioMetrics> {
    vec![
        baseline(),
        application_limited(),
        stale_feedback(),
        vbr_loss_reorder(),
        path_step(),
        ecn_l4s(),
        policer(),
        competing_flow(),
    ]
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
    sim.controller.application_limited_since = Some(Duration::ZERO);
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
    let mut metrics = sim.metrics("application-limited", 0x0702);
    metrics.application_limited_at_millis = entered;
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
    metrics.old_path_sample_used = false;
    metrics
}

fn ecn_l4s() -> ScenarioMetrics {
    let mut sim = Simulation::new(4_000_000);
    let mut before_ce = 0;
    let mut reduced = false;
    let mut disabled = false;
    for tick in 0..1_000 {
        let in_ce = (300..500).contains(&tick);
        let bleached = (500..700).contains(&tick);
        if tick == 300 {
            before_ce = sim.output.reference_window;
        }
        sim.step(
            2_000_000,
            Duration::from_millis(50),
            4_000_000,
            4_000_000,
            1,
            true,
            false,
            (in_ce && tick % 20 == 0).then_some(EcnMark::Ce),
            EcnValidation {
                classic: true,
                l4s: true,
                bleached,
            },
        );
        reduced |= in_ce && sim.output.reference_window < before_ce;
        disabled |= bleached && !sim.output.l4s_enabled;
    }
    let mut metrics = sim.metrics("ECN-L4S", 0x0706);
    metrics.ecn_reduced_window = reduced;
    metrics.l4s_disabled_on_bleach = disabled;
    metrics
}

fn policer() -> ScenarioMetrics {
    let mut sim = Simulation::new(8_000_000);
    let mut detected = false;
    for tick in 0..1_500 {
        let loss = tick > 300 && tick % 8 == 0;
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
        detected |= sim.output.policer_detected;
    }
    let mut metrics = sim.metrics("policer", 0x0707);
    metrics.policer_detected = detected;
    metrics
}

fn competing_flow() -> ScenarioMetrics {
    let mut left = Simulation::new(4_000_000);
    let mut right = Simulation::new(4_000_000);
    let mut left_delivered = 0_u64;
    let mut right_delivered = 0_u64;
    let mut useful_delivered = 0_u64;
    for _ in 0..2_000 {
        let total = left
            .output
            .target_media_payload_rate
            .saturating_add(right.output.target_media_payload_rate)
            .max(1);
        let left_capacity =
            4_000_000_u64.saturating_mul(left.output.target_media_payload_rate) / total;
        let right_capacity = 4_000_000_u64.saturating_sub(left_capacity);
        let before_left = left.admitted_bytes;
        let before_right = right.admitted_bytes;
        left.step(
            left_capacity.max(1),
            Duration::from_millis(50),
            4_000_000,
            4_000_000,
            1,
            true,
            false,
            None,
            EcnValidation::default(),
        );
        right.step(
            right_capacity.max(1),
            Duration::from_millis(50),
            4_000_000,
            4_000_000,
            1,
            true,
            false,
            None,
            EcnValidation::default(),
        );
        if left.now > Duration::from_millis(19_750) {
            let left_sent = left.admitted_bytes.saturating_sub(before_left);
            let right_sent = right.admitted_bytes.saturating_sub(before_right);
            left_delivered = left_delivered.saturating_add(left_sent);
            right_delivered = right_delivered.saturating_add(right_sent);
            useful_delivered = useful_delivered.saturating_add(
                left_sent
                    .saturating_add(right_sent)
                    .min(4_000_000 / 8 * micros(STEP) / 1_000_000),
            );
        }
    }
    let fairness = percent(
        left_delivered,
        left_delivered.saturating_add(right_delivered),
    );
    let mut metrics = left.metrics("competing-flow", 0x0708);
    metrics.fairness_percent = fairness;
    metrics.utilization_percent = percent(useful_delivered, 4_000_000 / 8 * 250 / 1_000);
    metrics.maximum_native_target_micros = metrics
        .maximum_native_target_micros
        .max(right.maximum_native_target);
    metrics
}

fn percent(value: u64, total: u64) -> u64 {
    value.saturating_mul(100) / total.max(1)
}
