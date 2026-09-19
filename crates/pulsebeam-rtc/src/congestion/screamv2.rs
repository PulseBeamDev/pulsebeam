#![allow(
    dead_code,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "the pinned SCReAMv2 profile uses bounded fixed-point arithmetic"
)]

use std::{collections::VecDeque, time::Duration};

const ONE: u64 = 65_536;
const BASE_DELAY_WINDOW: Duration = Duration::from_secs(10);
const BASE_DELAY_SAMPLES_MAX: usize = 4_096;
const COMPETING_HISTORY: usize = 200;
const COMPETING_MEAN_HISTORY: usize = 50;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EcnMark {
    NotEct,
    Ect1,
    Ect0,
    Ce,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FeedbackSample {
    pub(crate) sent_at: Duration,
    pub(crate) received_at: Duration,
    pub(crate) transport_bytes: u32,
    pub(crate) received: bool,
    /// True when this data unit belongs to newly advanced ACK-edge bytes.
    pub(crate) newly_acked: bool,
    /// True only after the RFC 4.2.1.1 reordering timer confirms loss.
    pub(crate) lost: bool,
    pub(crate) receiver_arrival_micros: Option<i64>,
    pub(crate) ecn: Option<EcnMark>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EcnMode {
    Disabled,
    Classic,
    L4s,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ControllerInput<'a> {
    pub(crate) feedback: &'a [FeedbackSample],
    pub(crate) feedback_hold: Duration,
    pub(crate) bytes_in_flight: u64,
    /// The application supplied TARGET_BITRATE_MAX from draft section 4.4.
    pub(crate) target_bitrate_max: u64,
    pub(crate) ecn_mode: EcnMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControllerReason {
    Startup,
    Feedback,
    Delay,
    Loss,
    ClassicEcn,
    L4s,
    PathChanged,
    Policer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Output {
    pub(crate) target_bitrate: u64,
    pub(crate) max_bytes_in_flight: u64,
    pub(crate) pacing_rate: u64,
    pub(crate) reference_window: u64,
    pub(crate) queue_delay_target: Duration,
    pub(crate) queue_delay: Duration,
    pub(crate) smoothed_rtt: Duration,
    pub(crate) delivered_rate: u64,
    pub(crate) policer_detected: bool,
    pub(crate) l4s_enabled: bool,
    pub(crate) reason: ControllerReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Profile {
    // draft-ietf-ccwg-rfc8298bis-screamv2-01 sections 4.2-4.5.
    pub(crate) target_min_bps: u64,
    pub(crate) target_initial_bps: u64,
    pub(crate) initial_rtt: Duration,
    pub(crate) queue_target_low: Duration,
    pub(crate) queue_target_high: Duration,
    pub(crate) min_reference_window: u64,
    pub(crate) mss: u64,
    pub(crate) virtual_rtt: Duration,
    pub(crate) bytes_in_flight_headroom: u64,
    pub(crate) loss_beta: u64,
    pub(crate) classic_ecn_beta: u64,
    pub(crate) post_congestion_rtts: u32,
    pub(crate) multiplicative_increase: u64,
    pub(crate) queue_average_gain: u64,
    pub(crate) jitter_min_max_gain: u64,
    pub(crate) jitter_deviation_gain: u64,
    pub(crate) jitter_deviation_threshold: Duration,
    pub(crate) l4s_attack_gain: u64,
    pub(crate) l4s_decay_gain: u64,
    pub(crate) reference_overhead_min: u64,
    pub(crate) reference_overhead_max: u64,
    pub(crate) pacing_rate_min_bps: u64,
    pub(crate) pacing_headroom: u64,
    pub(crate) relaxed_pacing_max: u64,
    pub(crate) relaxed_pacing_threshold: u64,
    pub(crate) packet_overhead_bytes: u64,
    pub(crate) loss_rate_threshold: u64,
    pub(crate) policer_loss_threshold: u64,
    pub(crate) policer_window_backoff: u64,
    pub(crate) policer_window_lift: u64,
    pub(crate) acknowledged_rate_margin: u64,
    pub(crate) low_target_backoff_scale: u64,
}

pub(crate) const PROFILE: Profile = Profile {
    target_min_bps: 20_000,
    target_initial_bps: 300_000,
    initial_rtt: Duration::from_millis(100),
    queue_target_low: Duration::from_millis(60),
    queue_target_high: Duration::from_millis(400),
    min_reference_window: 3_000,
    mss: 1_000,
    virtual_rtt: Duration::from_millis(25),
    bytes_in_flight_headroom: 3 * ONE / 2,
    loss_beta: 7 * ONE / 10,
    classic_ecn_beta: 4 * ONE / 5,
    post_congestion_rtts: 100,
    multiplicative_increase: ONE / 50,
    queue_average_gain: ONE / 4,
    jitter_min_max_gain: ONE / 16,
    jitter_deviation_gain: ONE / 32,
    jitter_deviation_threshold: Duration::from_millis(10),
    l4s_attack_gain: ONE / 8,
    l4s_decay_gain: ONE / 128,
    reference_overhead_min: 3 * ONE / 2,
    reference_overhead_max: 3 * ONE,
    pacing_rate_min_bps: 50_000,
    pacing_headroom: 3 * ONE / 2,
    relaxed_pacing_max: 4,
    relaxed_pacing_threshold: 4 * ONE / 5,
    packet_overhead_bytes: 20,
    loss_rate_threshold: ONE / 100,
    policer_loss_threshold: ONE / 10,
    policer_window_backoff: 9 * ONE / 10,
    policer_window_lift: 1_001 * ONE / 1_000,
    acknowledged_rate_margin: 4 * ONE / 5,
    low_target_backoff_scale: ONE / 4,
};

pub(crate) struct ScreamV2 {
    mss: u64,
    target_bitrate: u64,
    ref_wnd: u64,
    ref_wnd_i: u64,
    ref_wnd_i_update_allowed: bool,
    max_bytes_in_flight: u64,
    max_bytes_in_flight_prev: u64,
    s_rtt: Duration,
    qdelay_target: Duration,
    qdelay: Duration,
    qdelay_avg: Duration,
    qdelay_max_avg: Duration,
    qdelay_min_avg: Duration,
    qdelay_dev_avg: Duration,
    ref_wnd_delay_scale: u64,
    // Monotonic deque of candidate minima for the 10s base-delay window.
    base_delay_minima: VecDeque<(Duration, i128)>,
    competing_samples: VecDeque<u64>,
    last_qdelay_update: Duration,
    last_ref_wnd_increase: Duration,
    last_congestion_detected: Duration,
    last_reaction_to_congestion: Duration,
    pending_loss: bool,
    pending_ce: bool,
    bytes_newly_acked: u64,
    bytes_newly_acked_ce: u64,
    delivered_rate: u64,
    loss_rate: u64,
    loss_event_rate: u64,
    policer_detected: bool,
    max_policed_ref_wnd: u64,
    l4s_alpha: u64,
    ecn_mode: EcnMode,
    last_l4s_update: Duration,
    data_units_delivered_this_rtt: u64,
    data_units_marked_this_rtt: u64,
    reason: ControllerReason,
}

impl ScreamV2 {
    pub(crate) fn new(target_bitrate_max: u64, path_payload_max: Option<u32>) -> Self {
        let initial = PROFILE.target_initial_bps.min(target_bitrate_max);
        let ref_wnd = PROFILE.min_reference_window.max(div_ceil(
            initial.saturating_mul(micros(PROFILE.initial_rtt)),
            8_000_000,
        ));
        let mss = path_payload_max
            .filter(|maximum| *maximum > 0)
            .map_or(PROFILE.mss, u64::from)
            .min(PROFILE.mss);
        Self {
            mss,
            target_bitrate: initial,
            ref_wnd,
            ref_wnd_i: 1,
            ref_wnd_i_update_allowed: true,
            max_bytes_in_flight: 0,
            max_bytes_in_flight_prev: 0,
            s_rtt: PROFILE.initial_rtt,
            qdelay_target: PROFILE.queue_target_low,
            qdelay: Duration::ZERO,
            qdelay_avg: Duration::ZERO,
            qdelay_max_avg: PROFILE.queue_target_low,
            qdelay_min_avg: Duration::ZERO,
            qdelay_dev_avg: Duration::ZERO,
            ref_wnd_delay_scale: ONE,
            base_delay_minima: VecDeque::with_capacity(BASE_DELAY_SAMPLES_MAX),
            competing_samples: VecDeque::with_capacity(COMPETING_HISTORY),
            last_qdelay_update: Duration::ZERO,
            last_ref_wnd_increase: Duration::ZERO,
            last_congestion_detected: Duration::ZERO,
            last_reaction_to_congestion: Duration::ZERO,
            pending_loss: false,
            pending_ce: false,
            bytes_newly_acked: 0,
            bytes_newly_acked_ce: 0,
            delivered_rate: 0,
            loss_rate: 0,
            loss_event_rate: 0,
            policer_detected: false,
            max_policed_ref_wnd: u64::MAX,
            l4s_alpha: 0,
            ecn_mode: EcnMode::Disabled,
            last_l4s_update: Duration::ZERO,
            data_units_delivered_this_rtt: 0,
            data_units_marked_this_rtt: 0,
            reason: ControllerReason::Startup,
        }
    }

    /// A selected-path replacement invalidates path capacity as well as delay/ECN evidence.
    pub(crate) fn reset_path(&mut self, target_bitrate_max: u64) {
        let path_payload_max = u32::try_from(self.mss).ok();
        *self = Self::new(target_bitrate_max, path_payload_max);
        self.reason = ControllerReason::PathChanged;
    }

    pub(crate) fn update(&mut self, now: Duration, input: ControllerInput<'_>) -> Output {
        self.max_bytes_in_flight = self.max_bytes_in_flight.max(input.bytes_in_flight);
        self.ecn_mode = input.ecn_mode;
        let has_feedback = !input.feedback.is_empty();
        let has_received = input.feedback.iter().any(|sample| sample.received);
        let has_ack_progress = input.feedback.iter().any(|sample| sample.newly_acked);
        self.consume_feedback(now, input);
        // The draft updates queue-delay state from received acknowledgements and
        // reference-window state from ACK-edge progress. Timer/local-loss-only polls
        // must not replay stale delay evidence or consume accumulated ACK credit.
        if has_received {
            self.update_qdelay_filter(now);
        }
        if has_feedback {
            self.reduce_ref_wnd(now, input);
            if has_ack_progress {
                self.increase_ref_wnd(now, input.target_bitrate_max);
            }
            if has_received {
                self.adjust_qdelay_target();
            }
        }
        self.ref_wnd = self.ref_wnd.min(self.max_policed_ref_wnd);
        self.derive_target(input.target_bitrate_max);
        self.snapshot(input.target_bitrate_max)
    }

    fn consume_feedback(&mut self, now: Duration, input: ControllerInput<'_>) {
        if input.feedback.is_empty() {
            return;
        }
        let mut delivered_bytes = 0_u64;
        let mut interval_start = now;
        let mut loss_events = 0_u64;
        let rtt_sample = input
            .feedback
            .iter()
            .filter(|sample| sample.received)
            .max_by_key(|sample| sample.sent_at);
        for sample in input.feedback {
            interval_start = interval_start.min(sample.sent_at);
            if sample.newly_acked {
                self.bytes_newly_acked = self
                    .bytes_newly_acked
                    .saturating_add(u64::from(sample.transport_bytes));
            }
            if input.ecn_mode != EcnMode::Disabled
                && sample.received
                && sample.newly_acked
                && sample.ecn == Some(EcnMark::Ce)
            {
                self.bytes_newly_acked_ce = self
                    .bytes_newly_acked_ce
                    .saturating_add(u64::from(sample.transport_bytes));
            }
            if sample.received {
                delivered_bytes = delivered_bytes.saturating_add(u64::from(sample.transport_bytes));
                self.observe_delay(sample);
                self.data_units_delivered_this_rtt =
                    self.data_units_delivered_this_rtt.saturating_add(1);
                if input.ecn_mode != EcnMode::Disabled && sample.ecn == Some(EcnMark::Ce) {
                    self.data_units_marked_this_rtt =
                        self.data_units_marked_this_rtt.saturating_add(1);
                }
            }
            if sample.lost {
                loss_events = loss_events.saturating_add(1);
            }
        }
        if let Some(sample) = rtt_sample {
            let raw_rtt = sample
                .received_at
                .saturating_sub(sample.sent_at)
                .saturating_sub(input.feedback_hold);
            self.s_rtt = ewma_duration(self.s_rtt, raw_rtt, 1, 8);
        }
        let interval = now
            .saturating_sub(interval_start)
            .max(Duration::from_millis(1));
        let sample_rate = rate(delivered_bytes, interval);
        self.delivered_rate = if self.delivered_rate == 0 {
            sample_rate
        } else {
            ewma(self.delivered_rate, sample_rate, 1, 8)
        };

        // Section 4.5.2 example loss-rate filter. Missing-but-not-yet-lost statuses do not enter it.
        let alpha = ((self.mss.saturating_mul(ONE) / self.ref_wnd.max(1)) / 2).min(ONE / 400);
        let observed = input
            .feedback
            .iter()
            .filter(|sample| sample.received || sample.lost)
            .count() as u64;
        for _ in 0..observed.saturating_sub(loss_events) {
            self.loss_rate = mul_fixed(self.loss_rate, ONE.saturating_sub(alpha));
        }
        for _ in 0..loss_events {
            self.loss_rate = mul_fixed(self.loss_rate, ONE.saturating_sub(alpha))
                .saturating_add(alpha)
                .min(ONE);
        }
        self.update_l4s(now, input.ecn_mode);
        self.reason = ControllerReason::Feedback;
    }

    fn observe_delay(&mut self, sample: &FeedbackSample) {
        let Some(arrival) = sample.receiver_arrival_micros else {
            return;
        };
        let sent = i128::try_from(sample.sent_at.as_micros()).unwrap_or(i128::MAX);
        let relative = i128::from(arrival).saturating_sub(sent);
        while self
            .base_delay_minima
            .front()
            .is_some_and(|(at, _)| sample.received_at.saturating_sub(*at) > BASE_DELAY_WINDOW)
        {
            self.base_delay_minima.pop_front();
        }
        while self
            .base_delay_minima
            .back()
            .is_some_and(|(_, value)| *value >= relative)
        {
            self.base_delay_minima.pop_back();
        }
        if self.base_delay_minima.len() == BASE_DELAY_SAMPLES_MAX {
            self.base_delay_minima.pop_front();
        }
        self.base_delay_minima
            .push_back((sample.received_at, relative));
        let baseline = self
            .base_delay_minima
            .front()
            .map_or(relative, |(_, value)| *value);
        let queue = u64::try_from(relative.saturating_sub(baseline)).unwrap_or(u64::MAX);
        self.qdelay = Duration::from_micros(queue);
        let normalized = queue.saturating_mul(ONE) / micros(PROFILE.queue_target_low).max(1);
        push_bounded(&mut self.competing_samples, normalized, COMPETING_HISTORY);
        self.qdelay_max_avg = self.qdelay_target.min(self.qdelay.max(self.qdelay_max_avg));
        self.qdelay_min_avg = self.qdelay.min(self.qdelay_min_avg);
    }

    fn update_l4s(&mut self, now: Duration, mode: EcnMode) {
        if mode != EcnMode::L4s {
            self.l4s_alpha = 0;
            self.data_units_delivered_this_rtt = 0;
            self.data_units_marked_this_rtt = 0;
            return;
        }
        let interval = Duration::from_millis(10).min(self.s_rtt);
        if now.saturating_sub(self.last_l4s_update) < interval
            || self.data_units_delivered_this_rtt == 0
        {
            return;
        }
        let fraction = self.data_units_marked_this_rtt.saturating_mul(ONE)
            / self.data_units_delivered_this_rtt;
        self.l4s_alpha = if fraction >= self.l4s_alpha {
            mul_fixed(fraction, PROFILE.l4s_attack_gain).saturating_add(mul_fixed(
                self.l4s_alpha,
                ONE.saturating_sub(PROFILE.l4s_attack_gain),
            ))
        } else {
            mul_fixed(self.l4s_alpha, ONE.saturating_sub(PROFILE.l4s_decay_gain))
        };
        self.data_units_delivered_this_rtt = 0;
        self.data_units_marked_this_rtt = 0;
        self.last_l4s_update = now;
    }

    fn update_qdelay_filter(&mut self, now: Duration) {
        let interval = PROFILE.virtual_rtt.min(self.s_rtt);
        if now.saturating_sub(self.last_qdelay_update) < interval {
            return;
        }
        if self.qdelay < self.qdelay_avg {
            self.qdelay_avg = self.qdelay;
        } else {
            self.qdelay_avg = ewma_duration(self.qdelay_avg, self.qdelay, 1, 4);
        }
        // draft-01 specifies REDUCE_JITTER=false by default. Keep the optional
        // state inert in profile v1 instead of silently enabling a local tuning.
        self.ref_wnd_delay_scale = ONE;
        self.last_qdelay_update = now;
    }

    fn reduce_ref_wnd(&mut self, now: Duration, input: ControllerInput<'_>) {
        let current_loss = input.feedback.iter().any(|sample| sample.lost);
        let current_ce = input.ecn_mode != EcnMode::Disabled
            && input
                .feedback
                .iter()
                .any(|sample| sample.received && sample.ecn == Some(EcnMark::Ce));
        if current_loss || current_ce {
            self.last_congestion_detected = now;
        }
        self.pending_loss |= current_loss;
        self.pending_ce |= current_ce;
        let reaction_interval = PROFILE.virtual_rtt.min(self.s_rtt);
        if now.saturating_sub(self.last_reaction_to_congestion) < reaction_interval {
            return;
        }

        let loss_detected = self.pending_loss;
        let data_units_marked = self.pending_ce;
        self.pending_loss = false;
        self.pending_ce = false;
        let virtual_alpha = if self.qdelay_avg > self.qdelay_target / 2 {
            ratio_between(
                self.qdelay_avg.saturating_sub(self.qdelay_target / 2),
                self.qdelay_target / 2,
            )
        } else {
            0
        };
        self.policer_detected = loss_detected
            && self.loss_rate > PROFILE.policer_loss_threshold
            && self.qdelay_avg < self.qdelay_target / 4;
        if self.policer_detected {
            self.max_policed_ref_wnd = mul_fixed(self.ref_wnd, PROFILE.policer_window_backoff);
        }
        let is_loss = loss_detected
            && (self.loss_rate > PROFILE.loss_rate_threshold
                || self.qdelay_avg > self.qdelay_target / 4);
        let is_ce = data_units_marked;
        let is_virtual_ce = !is_loss && !is_ce && virtual_alpha > 0;
        if !(is_loss || is_ce || is_virtual_ce) {
            return;
        }

        let scl = self.inflection_scale();
        if self.ref_wnd_i_update_allowed {
            self.ref_wnd_i = self.ref_wnd;
            self.ref_wnd_i_update_allowed = false;
        }

        if is_loss {
            let mut backoff = ONE.saturating_sub(PROFILE.loss_beta);
            if self.target_bitrate
                < mul_fixed(self.delivered_rate, PROFILE.acknowledged_rate_margin)
            {
                backoff = mul_fixed(backoff, PROFILE.low_target_backoff_scale);
            }
            self.ref_wnd = mul_fixed(self.ref_wnd, ONE.saturating_sub(backoff));
            self.loss_event_rate = ewma(self.loss_event_rate, ONE, 1, 200);
            self.reason = if self.policer_detected {
                ControllerReason::Policer
            } else {
                ControllerReason::Loss
            };
        } else if is_ce {
            match input.ecn_mode {
                EcnMode::Classic => {
                    let mut backoff = ONE.saturating_sub(PROFILE.classic_ecn_beta);
                    if self.target_bitrate
                        < mul_fixed(self.delivered_rate, PROFILE.acknowledged_rate_margin)
                    {
                        backoff = mul_fixed(backoff, PROFILE.low_target_backoff_scale);
                    }
                    self.ref_wnd = mul_fixed(self.ref_wnd, ONE.saturating_sub(backoff));
                    self.reason = ControllerReason::ClassicEcn;
                }
                EcnMode::L4s => {
                    let mut backoff = self.l4s_alpha / 2;
                    let rtt_scale = ONE.max(duration_ratio(self.s_rtt, PROFILE.virtual_rtt));
                    backoff = mul_fixed(backoff, ONE.saturating_mul(ONE) / rtt_scale);
                    if self.qdelay < self.qdelay_target / 4 {
                        backoff = mul_fixed(backoff, scl.max(ONE / 4));
                        backoff = mul_fixed(backoff, self.ref_wnd_delay_scale.max(ONE / 4));
                    }
                    if self.target_bitrate
                        < mul_fixed(self.delivered_rate, PROFILE.acknowledged_rate_margin)
                    {
                        backoff = mul_fixed(backoff, PROFILE.low_target_backoff_scale);
                    }
                    if now.saturating_sub(self.last_reaction_to_congestion)
                        > self.s_rtt.max(PROFILE.virtual_rtt).saturating_mul(100)
                    {
                        self.ref_wnd = self.ref_wnd.min(self.max_bytes_in_flight_prev);
                    }
                    self.ref_wnd = mul_fixed(self.ref_wnd, ONE.saturating_sub(backoff));
                    self.reason = ControllerReason::L4s;
                }
                EcnMode::Disabled => {
                    return;
                }
            }
        } else {
            let mut backoff = virtual_alpha / 2;
            let rtt_scale = ONE.max(duration_ratio(self.s_rtt, PROFILE.virtual_rtt));
            backoff = mul_fixed(backoff, ONE.saturating_mul(ONE) / rtt_scale);
            if self.target_bitrate
                < mul_fixed(self.delivered_rate, PROFILE.acknowledged_rate_margin)
            {
                backoff = mul_fixed(backoff, PROFILE.low_target_backoff_scale);
            }
            self.ref_wnd = mul_fixed(self.ref_wnd, ONE.saturating_sub(backoff));
            self.reason = ControllerReason::Delay;
        }
        self.ref_wnd = self.ref_wnd.max(PROFILE.min_reference_window);
        self.last_congestion_detected = now;
        self.last_reaction_to_congestion = now;
    }

    fn increase_ref_wnd(&mut self, now: Duration, target_bitrate_max: u64) {
        if now.saturating_sub(self.last_ref_wnd_increase) < self.s_rtt {
            return;
        }
        let bytes = self
            .bytes_newly_acked
            .saturating_sub(self.bytes_newly_acked_ce);
        let ref_wnd_ratio = ONE.min(self.mss.saturating_mul(ONE) / self.ref_wnd.max(1));
        let mut increment = mul_fixed(bytes, ref_wnd_ratio);
        increment = mul_fixed(
            increment,
            duration_ratio(self.s_rtt, PROFILE.virtual_rtt).min(ONE),
        );
        let scl = self.inflection_scale();
        increment = mul_fixed(increment, scl.max(ONE / 4));
        increment = mul_fixed(increment, self.ref_wnd_delay_scale.max(ONE / 10));

        let post = duration_ratio(
            now.saturating_sub(self.last_congestion_detected),
            self.s_rtt
                .max(PROFILE.virtual_rtt)
                .saturating_mul(PROFILE.post_congestion_rtts),
        )
        .min(ONE);
        let scale_factor = ONE.saturating_add(mul_fixed(
            self.ref_wnd.saturating_mul(ONE) / self.mss.max(1),
            PROFILE.multiplicative_increase,
        ));
        let multiplicative = if scale_factor > ONE {
            ONE.saturating_add(mul_fixed(
                scale_factor.saturating_sub(ONE),
                mul_fixed(post, scl),
            ))
        } else {
            ONE
        };
        increment = mul_fixed(increment, multiplicative);

        let maximum = self.mss.saturating_add(mul_fixed(
            self.max_bytes_in_flight.max(self.max_bytes_in_flight_prev),
            PROFILE.bytes_in_flight_headroom,
        ));
        let previous = self.ref_wnd;
        let candidate = self.ref_wnd.saturating_add(increment);
        if candidate <= maximum && self.target_bitrate < target_bitrate_max {
            self.ref_wnd = candidate;
        }
        if self.ref_wnd > previous {
            self.ref_wnd_i_update_allowed = true;
        }
        self.max_bytes_in_flight_prev = self.max_bytes_in_flight;
        self.max_bytes_in_flight = 0;
        self.bytes_newly_acked = 0;
        self.bytes_newly_acked_ce = 0;
        self.last_ref_wnd_increase = now;
        self.loss_event_rate = ewma(self.loss_event_rate, 0, 1, 200);
        if self.max_policed_ref_wnd != u64::MAX {
            self.max_policed_ref_wnd =
                mul_fixed(self.max_policed_ref_wnd, PROFILE.policer_window_lift);
        }
    }

    fn inflection_scale(&self) -> u64 {
        let delta = self.ref_wnd.abs_diff(self.ref_wnd_i);
        let ratio = delta.saturating_mul(ONE) / self.ref_wnd_i.max(1);
        let scaled = ratio.saturating_mul(8).min(u64::MAX / 2);
        mul_fixed(scaled, scaled).clamp(ONE / 10, ONE)
    }

    fn adjust_qdelay_target(&mut self) {
        if self.competing_samples.len() < COMPETING_MEAN_HISTORY {
            return;
        }
        let mean_start = self
            .competing_samples
            .len()
            .saturating_sub(COMPETING_MEAN_HISTORY);
        let mean_count = self.competing_samples.len().saturating_sub(mean_start) as u128;
        let mean: u128 = self
            .competing_samples
            .iter()
            .skip(mean_start)
            .map(|value| u128::from(*value))
            .sum::<u128>()
            / mean_count.max(1);
        let variance_count = self.competing_samples.len() as u128;
        let variance_mean = self
            .competing_samples
            .iter()
            .map(|value| u128::from(*value))
            .sum::<u128>()
            / variance_count.max(1);
        let variance = self.competing_samples.iter().fold(0_u128, |sum, value| {
            let delta = i128::from(*value) - i128::try_from(variance_mean).unwrap_or(i128::MAX);
            let squared = delta.unsigned_abs().saturating_mul(delta.unsigned_abs());
            sum.saturating_add(squared)
        }) / variance_count.max(1);
        let candidate_normalized = mean.saturating_add(integer_sqrt(variance));
        let candidate = candidate_normalized
            .saturating_mul(u128::from(micros(PROFILE.queue_target_low)))
            / u128::from(ONE);
        let target = if self.loss_event_rate > mul_ratio(ONE, 2, 1_000) {
            mul_ratio(candidate.min(u128::from(u64::MAX)) as u64, 3, 2)
        } else if variance < u128::from(ONE).saturating_mul(u128::from(ONE)) / 5 {
            candidate.min(u128::from(u64::MAX)) as u64
        } else if candidate < u128::from(micros(PROFILE.queue_target_low)) {
            mul_ratio(micros(self.qdelay_target), 1, 2).max(candidate as u64)
        } else {
            mul_ratio(micros(self.qdelay_target), 9, 10)
        };
        self.qdelay_target =
            duration_from_micros(target).clamp(PROFILE.queue_target_low, PROFILE.queue_target_high);
    }

    fn derive_target(&mut self, target_bitrate_max: u64) {
        if target_bitrate_max == 0 {
            self.target_bitrate = 0;
            return;
        }
        let ref_ratio = ONE.min(self.mss.saturating_mul(ONE) / self.ref_wnd.max(1));
        let small_window = ONE.saturating_sub(ref_ratio.saturating_sub(ONE / 10).min(ONE / 5));
        let overhead =
            self.mss.saturating_mul(ONE) / self.mss.saturating_add(PROFILE.packet_overhead_bytes);
        let raw = self
            .ref_wnd
            .saturating_mul(8_000_000)
            .checked_div(micros(self.s_rtt).max(1))
            .unwrap_or(target_bitrate_max);
        let derived = mul_fixed(mul_fixed(raw, small_window), overhead).saturating_mul(5) / 6;
        self.target_bitrate = derived
            .max(PROFILE.target_min_bps.min(target_bitrate_max))
            .min(target_bitrate_max);
    }

    pub(crate) fn snapshot(&self, target_bitrate_max: u64) -> Output {
        let overhead = PROFILE.reference_overhead_min.saturating_add(mul_fixed(
            PROFILE
                .reference_overhead_max
                .saturating_sub(PROFILE.reference_overhead_min),
            self.ref_wnd_delay_scale,
        ));
        let maximum = mul_fixed(self.ref_wnd, overhead);
        let maximum_target = target_bitrate_max.max(1);
        let nominal = self.target_bitrate.saturating_mul(ONE) / maximum_target;
        let relaxed = nominal
            .saturating_sub(PROFILE.relaxed_pacing_threshold)
            .saturating_mul(5)
            .min(ONE);
        let pacing_scale = ONE
            .saturating_sub(relaxed)
            .max(ONE / PROFILE.relaxed_pacing_max.max(1));
        let pacing = mul_fixed(
            self.target_bitrate.max(PROFILE.pacing_rate_min_bps),
            PROFILE.pacing_headroom,
        )
        .saturating_mul(ONE)
            / pacing_scale.max(1);
        Output {
            target_bitrate: self.target_bitrate,
            max_bytes_in_flight: maximum,
            pacing_rate: pacing,
            reference_window: self.ref_wnd,
            queue_delay_target: self.qdelay_target,
            queue_delay: self.qdelay,
            smoothed_rtt: self.s_rtt,
            delivered_rate: self.delivered_rate,
            policer_detected: self.policer_detected,
            l4s_enabled: self.ecn_mode == EcnMode::L4s,
            reason: self.reason,
        }
    }

    #[cfg(test)]
    fn debug_accumulated_acks(&self) -> (u64, u64) {
        (self.bytes_newly_acked, self.bytes_newly_acked_ce)
    }
}

fn ewma(current: u64, sample: u64, numerator: u64, denominator: u64) -> u64 {
    current
        .saturating_mul(denominator.saturating_sub(numerator))
        .saturating_add(sample.saturating_mul(numerator))
        / denominator.max(1)
}

fn ewma_duration(
    current: Duration,
    sample: Duration,
    numerator: u64,
    denominator: u64,
) -> Duration {
    duration_from_micros(ewma(
        micros(current),
        micros(sample),
        numerator,
        denominator,
    ))
}

fn micros(value: Duration) -> u64 {
    value.as_micros().min(u128::from(u64::MAX)) as u64
}

fn duration_from_micros(value: u64) -> Duration {
    Duration::from_micros(value)
}

fn duration_ratio(value: Duration, denominator: Duration) -> u64 {
    micros(value).saturating_mul(ONE) / micros(denominator).max(1)
}

fn ratio_between(value: Duration, denominator: Duration) -> u64 {
    duration_ratio(value, denominator).min(ONE)
}

fn mul_fixed(value: u64, factor: u64) -> u64 {
    ((u128::from(value) * u128::from(factor)) / u128::from(ONE)).min(u128::from(u64::MAX)) as u64
}

fn mul_ratio(value: u64, numerator: u64, denominator: u64) -> u64 {
    ((u128::from(value) * u128::from(numerator)) / u128::from(denominator.max(1)))
        .min(u128::from(u64::MAX)) as u64
}

fn mul_duration(value: Duration, numerator: u64, denominator: u64) -> Duration {
    duration_from_micros(mul_ratio(micros(value), numerator, denominator))
}

fn div_ceil(value: u64, denominator: u64) -> u64 {
    value.saturating_add(denominator.saturating_sub(1)) / denominator.max(1)
}

fn rate(bytes: u64, interval: Duration) -> u64 {
    bytes.saturating_mul(8_000_000) / micros(interval).max(1)
}

fn push_bounded(values: &mut VecDeque<u64>, value: u64, capacity: usize) {
    if values.len() == capacity {
        values.pop_front();
    }
    values.push_back(value);
}

fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut low = 1_u128;
    let mut high = value.min(u128::from(u64::MAX));
    while low <= high {
        let middle = low + (high - low) / 2;
        match middle.checked_mul(middle).map(|square| square.cmp(&value)) {
            Some(std::cmp::Ordering::Equal) => return middle,
            Some(std::cmp::Ordering::Less) => low = middle + 1,
            _ => high = middle - 1,
        }
    }
    high
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at: u64, received: bool, newly_acked: bool, lost: bool, ce: bool) -> FeedbackSample {
        FeedbackSample {
            sent_at: Duration::from_millis(at.saturating_sub(50)),
            received_at: Duration::from_millis(at),
            transport_bytes: 1_000,
            received,
            newly_acked,
            lost,
            receiver_arrival_micros: received.then_some(
                i64::try_from(at)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000)
                    .saturating_sub(25_000),
            ),
            ecn: ce.then_some(EcnMark::Ce),
        }
    }

    fn input<'a>(feedback: &'a [FeedbackSample], max: u64) -> ControllerInput<'a> {
        ControllerInput {
            feedback,
            feedback_hold: Duration::ZERO,
            bytes_in_flight: 20_000,
            target_bitrate_max: max,
            ecn_mode: EcnMode::Disabled,
        }
    }

    #[test]
    fn inflection_scale_is_q16_and_matches_draft_examples() {
        let mut cc = ScreamV2::new(4_000_000, None);
        cc.ref_wnd_i = 10_000;
        cc.ref_wnd = 11_000;
        assert!((41_000..=43_000).contains(&cc.inflection_scale()));
        cc.ref_wnd = 12_000;
        assert_eq!(cc.inflection_scale(), ONE);
    }

    #[test]
    fn newly_acked_bytes_accumulate_until_the_rtt_window_update() {
        let mut cc = ScreamV2::new(4_000_000, None);
        for at in [25, 50, 75] {
            let feedback = [sample(at, true, true, false, false)];
            cc.update(Duration::from_millis(at), input(&feedback, 4_000_000));
        }
        assert_eq!(cc.debug_accumulated_acks().0, 3_000);
        let feedback = [sample(100, true, true, false, false)];
        cc.update(Duration::from_millis(100), input(&feedback, 4_000_000));
        assert_eq!(cc.debug_accumulated_acks(), (0, 0));
    }

    #[test]
    fn congestion_reaction_is_not_blocked_by_once_per_rtt_growth_cadence() {
        let mut cc = ScreamV2::new(4_000_000, None);
        cc.s_rtt = Duration::from_millis(200);
        cc.ref_wnd = 20_000;
        cc.loss_rate = ONE / 50;
        let loss = [sample(25, false, false, true, false)];
        let before = cc.ref_wnd;
        cc.update(Duration::from_millis(25), input(&loss, 4_000_000));
        assert!(cc.ref_wnd < before);
    }

    #[test]
    fn congestion_signal_survives_the_virtual_rtt_reaction_gate() {
        let mut cc = ScreamV2::new(4_000_000, None);
        cc.s_rtt = Duration::from_millis(200);
        cc.ref_wnd = 20_000;
        cc.loss_rate = ONE / 50;
        cc.last_reaction_to_congestion = Duration::from_millis(1);
        let loss = [sample(10, false, false, true, false)];
        let before = cc.ref_wnd;
        cc.update(Duration::from_millis(10), input(&loss, 4_000_000));
        assert_eq!(cc.ref_wnd, before);
        let clean = [sample(30, true, true, false, false)];
        cc.update(Duration::from_millis(30), input(&clean, 4_000_000));
        assert!(cc.ref_wnd < before);
    }

    #[test]
    fn lingering_l4s_alpha_without_a_fresh_mark_does_not_backoff_again() {
        let mut cc = ScreamV2::new(4_000_000, None);
        cc.ref_wnd = 20_000;
        cc.l4s_alpha = ONE / 4;
        cc.s_rtt = Duration::from_millis(50);
        let feedback = [sample(50, true, true, false, false)];
        let mut values = input(&feedback, 4_000_000);
        values.ecn_mode = EcnMode::L4s;
        let before = cc.ref_wnd;
        cc.update(Duration::from_millis(50), values);
        assert!(cc.ref_wnd >= before);
    }

    #[test]
    fn rtt_filter_consumes_one_sample_per_feedback_batch() {
        let mut cc = ScreamV2::new(4_000_000, None);
        let feedback = [
            FeedbackSample {
                sent_at: Duration::ZERO,
                received_at: Duration::from_millis(200),
                transport_bytes: 1_000,
                received: true,
                newly_acked: true,
                lost: false,
                receiver_arrival_micros: None,
                ecn: None,
            },
            FeedbackSample {
                sent_at: Duration::from_millis(190),
                received_at: Duration::from_millis(200),
                transport_bytes: 1_000,
                received: true,
                newly_acked: true,
                lost: false,
                receiver_arrival_micros: None,
                ecn: None,
            },
        ];
        cc.consume_feedback(Duration::from_millis(200), input(&feedback, 4_000_000));
        assert_eq!(cc.s_rtt, Duration::from_micros(88_750));
    }

    #[test]
    fn path_reset_drops_previous_capacity_state() {
        let mut cc = ScreamV2::new(8_000_000, None);
        cc.ref_wnd = 200_000;
        cc.target_bitrate = 8_000_000;
        cc.s_rtt = Duration::from_millis(350);
        cc.qdelay_target = Duration::from_millis(400);
        cc.bytes_newly_acked = 12_000;
        cc.bytes_newly_acked_ce = 1_000;
        cc.pending_loss = true;
        cc.pending_ce = true;
        cc.reset_path(2_000_000);
        assert_eq!(cc.s_rtt, PROFILE.initial_rtt);
        assert_eq!(cc.qdelay_target, PROFILE.queue_target_low);
        assert!(cc.ref_wnd < 200_000);
        assert!(cc.target_bitrate <= PROFILE.target_initial_bps);
        assert_eq!(cc.debug_accumulated_acks(), (0, 0));
        assert!(!cc.pending_loss && !cc.pending_ce);
        assert_eq!(cc.reason, ControllerReason::PathChanged);
    }

    #[test]
    fn recovered_ce_does_not_cross_ack_windows() {
        let mut cc = ScreamV2::new(4_000_000, None);
        let first = [sample(25, false, true, false, false)];
        cc.consume_feedback(Duration::from_millis(25), input(&first, 4_000_000));
        assert_eq!(cc.debug_accumulated_acks(), (1_000, 0));
        let recovered = [sample(50, true, false, false, true)];
        let mut values = input(&recovered, 4_000_000);
        values.ecn_mode = EcnMode::L4s;
        cc.consume_feedback(Duration::from_millis(50), values);
        assert_eq!(cc.debug_accumulated_acks(), (1_000, 0));
    }

    #[test]
    fn disabled_ecn_does_not_suppress_ack_growth_credit() {
        let mut cc = ScreamV2::new(4_000_000, None);
        let marked = [sample(25, true, true, false, true)];
        cc.consume_feedback(Duration::from_millis(25), input(&marked, 4_000_000));
        assert_eq!(cc.debug_accumulated_acks(), (1_000, 0));
    }

    #[test]
    fn loss_only_feedback_does_not_consume_pending_ack_credit() {
        let mut cc = ScreamV2::new(4_000_000, None);
        let ack = [sample(25, true, true, false, false)];
        cc.update(Duration::from_millis(25), input(&ack, 4_000_000));
        assert_eq!(cc.debug_accumulated_acks().0, 1_000);
        cc.loss_rate = ONE / 50;
        let loss = [sample(100, false, false, true, false)];
        cc.update(Duration::from_millis(100), input(&loss, 4_000_000));
        assert_eq!(cc.debug_accumulated_acks().0, 1_000);
    }

    #[test]
    fn timer_only_poll_does_not_replay_stale_delay_evidence() {
        let mut cc = ScreamV2::new(4_000_000, None);
        cc.ref_wnd = 20_000;
        cc.qdelay = Duration::from_millis(60);
        cc.qdelay_avg = Duration::from_millis(60);
        let before_window = cc.ref_wnd;
        let before_target = cc.qdelay_target;
        cc.update(Duration::from_millis(100), input(&[], 4_000_000));
        assert_eq!(cc.ref_wnd, before_window);
        assert_eq!(cc.qdelay_target, before_target);
    }

    #[test]
    fn inflection_point_updates_once_until_window_grows() {
        let mut cc = ScreamV2::new(4_000_000, None);
        cc.ref_wnd = 20_000;
        cc.loss_rate = ONE / 50;
        let first = [sample(25, false, false, true, false)];
        cc.update(Duration::from_millis(25), input(&first, 4_000_000));
        let inflection = cc.ref_wnd_i;
        let second = [sample(50, false, false, true, false)];
        cc.update(Duration::from_millis(50), input(&second, 4_000_000));
        assert_eq!(cc.ref_wnd_i, inflection);
    }
}
