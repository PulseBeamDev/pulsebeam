#![allow(
    dead_code,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "Plan 11 consumes this private fixed-point controller; arithmetic is saturating or bounded"
)]

use std::{collections::VecDeque, time::Duration};

const ONE: u64 = 65_536;
const MAX_DELAY_SAMPLES: usize = 4_096;
const COMPETING_HISTORY: usize = 200;

#[cfg(test)]
#[path = "congestion/scenario.rs"]
pub(crate) mod scenario;

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
    pub(crate) receiver_arrival_micros: Option<i64>,
    pub(crate) ecn: Option<EcnMark>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct EcnValidation {
    pub(crate) classic: bool,
    pub(crate) l4s: bool,
    pub(crate) bleached: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ControllerInput<'a> {
    pub(crate) path_epoch: Option<u64>,
    pub(crate) path_available: bool,
    pub(crate) feedback: &'a [FeedbackSample],
    pub(crate) feedback_hold: Duration,
    pub(crate) bytes_in_flight: u64,
    pub(crate) paced_queue_bytes: u64,
    pub(crate) offered_media_rate: u64,
    pub(crate) admitted_media_rate: u64,
    pub(crate) desired_media_rate: u64,
    pub(crate) window_or_pacer_blocked: bool,
    pub(crate) queue_delay_ceiling: Duration,
    pub(crate) ecn: EcnValidation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControllerReason {
    Startup,
    Feedback,
    Delay,
    Loss,
    ClassicEcn,
    L4s,
    ApplicationLimited,
    FeedbackStale,
    PathChanged,
    Policer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SafeRtpEnvelope {
    pub(crate) target_media_payload_rate: u64,
    pub(crate) max_rtp_bytes_in_flight: u64,
    pub(crate) pacing_transport_rate: u64,
    pub(crate) reference_window: u64,
    pub(crate) native_queue_delay_target: Duration,
    pub(crate) effective_queue_delay_target: Duration,
    pub(crate) queue_delay: Duration,
    pub(crate) queue_delay_confidence: u16,
    pub(crate) smoothed_rtt: Duration,
    pub(crate) feedback_hold: Duration,
    pub(crate) delivered_rtp_transport_rate: u64,
    pub(crate) application_limited: bool,
    pub(crate) feedback_stale: bool,
    pub(crate) policer_detected: bool,
    pub(crate) l4s_enabled: bool,
    pub(crate) probe_permitted: bool,
    pub(crate) reason: ControllerReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CongestionProfileV1 {
    // draft-ietf-ccwg-rfc8298bis-screamv2-01 sections 4.2-4.5.
    pub(crate) target_min_bps: u64,
    pub(crate) target_max_bps: u64,
    pub(crate) target_initial_bps: u64,
    pub(crate) initial_rtt: Duration,
    pub(crate) queue_target_low: Duration,
    pub(crate) queue_target_high: Duration,
    pub(crate) min_reference_window: u64,
    pub(crate) draft_mss: u64,
    pub(crate) application_limited_threshold_percent: u64,
    pub(crate) application_limited_interval: Duration,
    pub(crate) confidence_half_life: Duration,
    pub(crate) virtual_rtt: Duration,
    pub(crate) bytes_in_flight_headroom: u64,
    pub(crate) loss_backoff: u64,
    pub(crate) classic_ecn_backoff: u64,
    pub(crate) post_congestion_rtts: u64,
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
    pub(crate) sent_history_entries: usize,
    pub(crate) sent_history_age: Duration,
    pub(crate) ack_reordering_packets: u64,
    pub(crate) feedback_statuses: usize,
    pub(crate) expirations_per_poll: usize,
    pub(crate) payload_efficiency_initial: u64,
    pub(crate) payload_efficiency_min: u64,
    pub(crate) payload_efficiency_max: u64,
    pub(crate) payload_efficiency_time_constant: Duration,
    pub(crate) paced_packets: usize,
    pub(crate) paced_transport_bytes: u64,
    pub(crate) pacer_horizon_max: Duration,
    pub(crate) rtx_age_max: Duration,
    pub(crate) control_rate_min_bps: u64,
    pub(crate) allocation_hysteresis_bps: u64,
    pub(crate) allocation_hysteresis_percent: u64,
    pub(crate) allocation_confirmation_rounds: u8,
    pub(crate) upward_relaxation_percent: u64,
    pub(crate) fixed_range_relaxation_percent: u64,
    pub(crate) frame_dependencies_max: usize,
    pub(crate) probe_history: usize,
    pub(crate) probe_unmet_percent: u64,
    pub(crate) probe_unmet_bps: u64,
    pub(crate) probe_duration: Duration,
    pub(crate) probe_packets_min: usize,
    pub(crate) probe_packets_max: usize,
    pub(crate) probe_transport_bytes_max: u64,
    pub(crate) probe_interval_min: Duration,
    pub(crate) probe_overhead_percent: u64,
    pub(crate) probe_success_percent: u64,
    pub(crate) probe_queue_growth_abort: Duration,
    pub(crate) probe_loss_abort_percent: u64,
    pub(crate) urgent_queue_ceiling: Duration,
    pub(crate) quality_queue_ceiling: Duration,
    pub(crate) urgent_playout_knee: Duration,
    pub(crate) quality_playout_saturation: Duration,
    pub(crate) urgent_utilization: u64,
    pub(crate) quality_utilization: u64,
    pub(crate) urgent_pacer_horizon: Duration,
    pub(crate) quality_pacer_horizon: Duration,
    pub(crate) rtx_allowance_max: Duration,
    pub(crate) urgent_probe_impact: Duration,
    pub(crate) quality_probe_impact: Duration,
    pub(crate) audio_processing_reserve: Duration,
    pub(crate) video_processing_reserve: Duration,
    pub(crate) synchronized_clock_reserve: Duration,
    pub(crate) provisional_clock_reserve: Duration,
    pub(crate) unverified_clock_reserve: Duration,
    pub(crate) discontinuous_clock_reserve: Duration,
    pub(crate) network_uncertainty_min: Duration,
    pub(crate) network_uncertainty_max: Duration,
    pub(crate) asap_audio_age: Duration,
    pub(crate) asap_video_age: Duration,
}

const PROFILE: CongestionProfileV1 = CongestionProfileV1 {
    target_min_bps: 20_000,
    target_max_bps: 100_000_000,
    target_initial_bps: 300_000,
    initial_rtt: Duration::from_millis(100),
    queue_target_low: Duration::from_millis(60),
    queue_target_high: Duration::from_millis(400),
    min_reference_window: 3_000,
    draft_mss: 1_000,
    application_limited_threshold_percent: 85,
    application_limited_interval: Duration::from_millis(200),
    confidence_half_life: Duration::from_secs(5),
    virtual_rtt: Duration::from_millis(25),
    bytes_in_flight_headroom: 3 * ONE / 2,
    loss_backoff: 7 * ONE / 10,
    classic_ecn_backoff: 4 * ONE / 5,
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
    sent_history_entries: 32_768,
    sent_history_age: Duration::from_secs(5),
    ack_reordering_packets: 4_096,
    feedback_statuses: 8_192,
    expirations_per_poll: 256,
    payload_efficiency_initial: 58_982,
    payload_efficiency_min: ONE / 2,
    payload_efficiency_max: 64_881,
    payload_efficiency_time_constant: Duration::from_secs(1),
    paced_packets: 8_192,
    paced_transport_bytes: 8 * 1024 * 1024,
    pacer_horizon_max: Duration::from_millis(100),
    rtx_age_max: Duration::from_secs(2),
    control_rate_min_bps: 16_000,
    allocation_hysteresis_bps: 25_000,
    allocation_hysteresis_percent: 10,
    allocation_confirmation_rounds: 2,
    upward_relaxation_percent: 10,
    fixed_range_relaxation_percent: 5,
    frame_dependencies_max: 8,
    probe_history: 64,
    probe_unmet_percent: 20,
    probe_unmet_bps: 50_000,
    probe_duration: Duration::from_millis(20),
    probe_packets_min: 5,
    probe_packets_max: 32,
    probe_transport_bytes_max: 48 * 1024,
    probe_interval_min: Duration::from_secs(1),
    probe_overhead_percent: 5,
    probe_success_percent: 80,
    probe_queue_growth_abort: Duration::from_millis(10),
    probe_loss_abort_percent: 5,
    urgent_queue_ceiling: Duration::from_millis(15),
    quality_queue_ceiling: Duration::from_millis(60),
    urgent_playout_knee: Duration::from_millis(75),
    quality_playout_saturation: Duration::from_millis(500),
    urgent_utilization: 52_429,
    quality_utilization: 62_259,
    urgent_pacer_horizon: Duration::from_millis(15),
    quality_pacer_horizon: Duration::from_millis(80),
    rtx_allowance_max: Duration::from_millis(50),
    urgent_probe_impact: Duration::from_millis(5),
    quality_probe_impact: Duration::from_millis(20),
    audio_processing_reserve: Duration::from_millis(10),
    video_processing_reserve: Duration::from_millis(25),
    synchronized_clock_reserve: Duration::from_millis(5),
    provisional_clock_reserve: Duration::from_millis(25),
    unverified_clock_reserve: Duration::from_millis(50),
    discontinuous_clock_reserve: Duration::from_millis(75),
    network_uncertainty_min: Duration::from_millis(5),
    network_uncertainty_max: Duration::from_millis(50),
    asap_audio_age: Duration::from_millis(100),
    asap_video_age: Duration::from_millis(150),
};

pub(crate) const fn profile() -> &'static CongestionProfileV1 {
    &PROFILE
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SenderOperatingPoint {
    pub(crate) queue_delay_ceiling: Duration,
    pub(crate) allocation_utilization: u64,
    pub(crate) pacer_horizon: Duration,
    pub(crate) rtx_extra_allowance: Duration,
    pub(crate) probe_queue_impact: Duration,
    pub(crate) governed_demand: u64,
}

pub(crate) struct LatencyGovernor;

impl LatencyGovernor {
    pub(crate) fn operating_point(
        playout_max_ticks: u16,
        desired_media_rate: u64,
    ) -> SenderOperatingPoint {
        let playout = Duration::from_millis(u64::from(playout_max_ticks).saturating_mul(10));
        let numerator = if playout_max_ticks == 0 || playout <= PROFILE.urgent_playout_knee {
            0
        } else {
            micros(playout.saturating_sub(PROFILE.urgent_playout_knee)).min(micros(
                PROFILE
                    .quality_playout_saturation
                    .saturating_sub(PROFILE.urgent_playout_knee),
            ))
        };
        let denominator = micros(
            PROFILE
                .quality_playout_saturation
                .saturating_sub(PROFILE.urgent_playout_knee),
        );
        let queue_delay_ceiling = lerp_duration_even(
            PROFILE.urgent_queue_ceiling,
            PROFILE.quality_queue_ceiling,
            numerator,
            denominator,
        );
        let allocation_utilization = lerp_even(
            PROFILE.urgent_utilization,
            PROFILE.quality_utilization,
            numerator,
            denominator,
        );
        SenderOperatingPoint {
            queue_delay_ceiling,
            allocation_utilization,
            pacer_horizon: lerp_duration_even(
                PROFILE.urgent_pacer_horizon,
                PROFILE.quality_pacer_horizon,
                numerator,
                denominator,
            ),
            rtx_extra_allowance: lerp_duration_even(
                Duration::ZERO,
                PROFILE.rtx_allowance_max,
                numerator,
                denominator,
            ),
            probe_queue_impact: lerp_duration_even(
                PROFILE.urgent_probe_impact,
                PROFILE.quality_probe_impact,
                numerator,
                denominator,
            ),
            governed_demand: mul_fixed(desired_media_rate, allocation_utilization),
        }
    }

    pub(crate) fn strictest_queue_ceiling<'a>(
        active: impl IntoIterator<Item = &'a SenderOperatingPoint>,
    ) -> Option<Duration> {
        active
            .into_iter()
            .map(|point| point.queue_delay_ceiling)
            .min()
    }
}

pub(crate) struct ScreamController {
    mss: u64,
    target_rate: u64,
    reference_window: u64,
    reference_inflection: u64,
    max_bytes_in_flight: u64,
    max_bytes_in_flight_previous: u64,
    smoothed_rtt: Duration,
    native_queue_target: Duration,
    effective_queue_target: Duration,
    queue_delay: Duration,
    queue_delay_average: Duration,
    queue_delay_max_average: Duration,
    queue_delay_min_average: Duration,
    queue_delay_deviation: Duration,
    delay_scale: u64,
    path_epoch: Option<u64>,
    path_available: bool,
    path_baseline_micros: Option<i128>,
    delay_samples: VecDeque<(Duration, i128)>,
    competing_samples: VecDeque<u64>,
    last_delay_update: Duration,
    last_window_update: Duration,
    last_congestion: Duration,
    last_reaction: Duration,
    last_feedback: Option<Duration>,
    last_send: Option<Duration>,
    application_limited_since: Option<Duration>,
    application_limited: bool,
    feedback_stale: bool,
    confidence: u16,
    confidence_decay_started: Option<(Duration, u16)>,
    delivered_rate: u64,
    loss_rate: u64,
    loss_event_rate: u64,
    policer_detected: bool,
    max_policed_window: u64,
    l4s_alpha: u64,
    l4s_enabled: bool,
    last_l4s_update: Duration,
    delivered_packets_l4s: u64,
    marked_packets_l4s: u64,
    feedback_hold: Duration,
    reason: ControllerReason,
}

impl ScreamController {
    pub(crate) fn new(desired_media_rate: u64, path_payload_max: Option<u32>) -> Self {
        let initial = PROFILE
            .target_initial_bps
            .min(desired_media_rate)
            .min(PROFILE.target_max_bps);
        let reference_window = PROFILE.min_reference_window.max(div_ceil(
            initial.saturating_mul(PROFILE.initial_rtt.as_micros() as u64),
            8_000_000,
        ));
        let mss = path_payload_max
            .filter(|maximum| *maximum > 0)
            .map_or(PROFILE.draft_mss, u64::from)
            .min(PROFILE.draft_mss);
        Self {
            mss,
            target_rate: initial,
            reference_window,
            // draft-ietf-ccwg-rfc8298bis-screamv2-01 section 4.2.2
            // initializes the inflection point to one byte.
            reference_inflection: 1,
            max_bytes_in_flight: 0,
            max_bytes_in_flight_previous: 0,
            smoothed_rtt: PROFILE.initial_rtt,
            native_queue_target: PROFILE.queue_target_low,
            effective_queue_target: PROFILE.queue_target_low,
            queue_delay: Duration::ZERO,
            queue_delay_average: Duration::ZERO,
            queue_delay_max_average: PROFILE.queue_target_low,
            queue_delay_min_average: Duration::ZERO,
            queue_delay_deviation: Duration::ZERO,
            delay_scale: ONE,
            path_epoch: None,
            path_available: false,
            path_baseline_micros: None,
            delay_samples: VecDeque::with_capacity(MAX_DELAY_SAMPLES),
            competing_samples: VecDeque::with_capacity(COMPETING_HISTORY),
            last_delay_update: Duration::ZERO,
            last_window_update: Duration::ZERO,
            last_congestion: Duration::ZERO,
            last_reaction: Duration::ZERO,
            last_feedback: None,
            last_send: None,
            application_limited_since: None,
            application_limited: false,
            feedback_stale: false,
            confidence: u16::MAX,
            confidence_decay_started: None,
            delivered_rate: 0,
            loss_rate: 0,
            loss_event_rate: 0,
            policer_detected: false,
            max_policed_window: u64::MAX,
            l4s_alpha: 0,
            l4s_enabled: false,
            last_l4s_update: Duration::ZERO,
            delivered_packets_l4s: 0,
            marked_packets_l4s: 0,
            feedback_hold: Duration::ZERO,
            reason: ControllerReason::Startup,
        }
    }

    pub(crate) fn note_send(&mut self, at: Duration) {
        self.last_send = Some(at);
    }

    pub(crate) fn update(&mut self, now: Duration, input: ControllerInput<'_>) -> SafeRtpEnvelope {
        self.update_path(input.path_epoch, input.path_available);
        self.effective_queue_target = self.native_queue_target.min(input.queue_delay_ceiling);
        self.max_bytes_in_flight = self.max_bytes_in_flight.max(input.bytes_in_flight);
        self.feedback_hold = input.feedback_hold;
        self.update_application_limited(now, &input);
        self.update_feedback(now, &input);
        self.update_staleness(now);
        self.update_confidence(now, !input.feedback.is_empty());
        self.update_window(now, &input);
        self.derive_target(input.desired_media_rate);
        self.effective_queue_target = self.native_queue_target.min(input.queue_delay_ceiling);
        self.snapshot(
            input.desired_media_rate,
            input.bytes_in_flight,
            input.paced_queue_bytes,
        )
    }

    fn update_path(&mut self, epoch: Option<u64>, available: bool) {
        if self.path_epoch == epoch && self.path_available == available {
            return;
        }
        self.path_epoch = epoch;
        self.path_available = available;
        self.path_baseline_micros = None;
        self.delay_samples.clear();
        self.competing_samples.clear();
        self.queue_delay = Duration::ZERO;
        self.queue_delay_average = Duration::ZERO;
        self.queue_delay_max_average = self.native_queue_target;
        self.queue_delay_min_average = Duration::ZERO;
        self.queue_delay_deviation = Duration::ZERO;
        self.delay_scale = ONE;
        self.last_feedback = None;
        self.feedback_stale = false;
        self.l4s_enabled = false;
        self.l4s_alpha = 0;
        self.reason = ControllerReason::PathChanged;
    }

    fn update_application_limited(&mut self, now: Duration, input: &ControllerInput<'_>) {
        let credible = self.target_rate.max(input.admitted_media_rate);
        let below = input.offered_media_rate.saturating_mul(100)
            < credible.saturating_mul(PROFILE.application_limited_threshold_percent)
            && !input.window_or_pacer_blocked;
        if below {
            let since = *self.application_limited_since.get_or_insert(now);
            if now.saturating_sub(since) >= PROFILE.application_limited_interval {
                self.application_limited = true;
                self.reason = ControllerReason::ApplicationLimited;
            }
        } else {
            self.application_limited_since = None;
            self.application_limited = false;
        }
    }

    fn update_feedback(&mut self, now: Duration, input: &ControllerInput<'_>) {
        if input.feedback.is_empty() {
            return;
        }
        self.last_feedback = Some(now);
        self.feedback_stale = false;
        let mut delivered_bytes = 0_u64;
        let mut interval_start = now;
        for sample in input.feedback {
            interval_start = interval_start.min(sample.sent_at);
            let alpha =
                ((self.mss.saturating_mul(ONE) / self.reference_window.max(1)) / 2).min(ONE / 400);
            self.loss_rate = mul_fixed(self.loss_rate, ONE.saturating_sub(alpha));
            if sample.received {
                delivered_bytes = delivered_bytes.saturating_add(u64::from(sample.transport_bytes));
                let raw_rtt = sample
                    .received_at
                    .saturating_sub(sample.sent_at)
                    .saturating_sub(input.feedback_hold);
                self.smoothed_rtt = ewma_duration(self.smoothed_rtt, raw_rtt, 1, 8);
                self.observe_delay(sample);
                self.delivered_packets_l4s = self.delivered_packets_l4s.saturating_add(1);
                if sample.ecn == Some(EcnMark::Ce) {
                    self.marked_packets_l4s = self.marked_packets_l4s.saturating_add(1);
                }
            } else {
                self.loss_rate = self.loss_rate.saturating_add(alpha).min(ONE);
            }
        }
        let interval = now
            .saturating_sub(interval_start)
            .max(Duration::from_millis(1));
        let sample_rate = rate(delivered_bytes, interval);
        self.delivered_rate = if self.delivered_rate == 0 {
            sample_rate
        } else {
            (self
                .delivered_rate
                .saturating_mul(7)
                .saturating_add(sample_rate))
                / 8
        };
        self.l4s_enabled = input.ecn.l4s && !input.ecn.bleached;
        if input.ecn.bleached {
            self.l4s_alpha = 0;
            self.delivered_packets_l4s = 0;
            self.marked_packets_l4s = 0;
        }
        self.update_l4s(now);
        self.reason = ControllerReason::Feedback;
    }

    fn observe_delay(&mut self, sample: &FeedbackSample) {
        let Some(arrival) = sample.receiver_arrival_micros else {
            return;
        };
        let sent = i128::try_from(sample.sent_at.as_micros()).unwrap_or(i128::MAX);
        let relative = i128::from(arrival).saturating_sub(sent);
        while self
            .delay_samples
            .front()
            .is_some_and(|(at, _)| sample.received_at.saturating_sub(*at) > Duration::from_secs(10))
        {
            self.delay_samples.pop_front();
        }
        if self.delay_samples.len() == MAX_DELAY_SAMPLES {
            self.delay_samples.pop_front();
        }
        self.delay_samples.push_back((sample.received_at, relative));
        let baseline = self
            .delay_samples
            .iter()
            .map(|(_, value)| *value)
            .min()
            .unwrap_or(relative);
        self.path_baseline_micros = Some(baseline);
        let queue = u64::try_from(relative.saturating_sub(baseline)).unwrap_or(u64::MAX);
        self.queue_delay = Duration::from_micros(queue);
        let normalized = queue.saturating_mul(ONE) / micros(PROFILE.queue_target_low);
        push_bounded(&mut self.competing_samples, normalized, COMPETING_HISTORY);
        self.queue_delay_max_average = self.queue_delay.max(self.queue_delay_max_average);
        self.queue_delay_min_average = self.queue_delay.min(self.queue_delay_min_average);
    }

    fn update_l4s(&mut self, now: Duration) {
        let interval = Duration::from_millis(10).min(self.smoothed_rtt);
        if now.saturating_sub(self.last_l4s_update) < interval || self.delivered_packets_l4s == 0 {
            return;
        }
        let fraction = self.marked_packets_l4s.saturating_mul(ONE) / self.delivered_packets_l4s;
        self.l4s_alpha = if fraction >= self.l4s_alpha {
            (fraction.saturating_add(self.l4s_alpha.saturating_mul(7))) / 8
        } else {
            self.l4s_alpha.saturating_mul(127) / 128
        };
        self.delivered_packets_l4s = 0;
        self.marked_packets_l4s = 0;
        self.last_l4s_update = now;
    }

    fn update_staleness(&mut self, now: Duration) {
        let threshold = self
            .smoothed_rtt
            .saturating_mul(3)
            .clamp(Duration::from_millis(500), Duration::from_secs(2));
        let reference = self.last_feedback.or(self.last_send);
        self.feedback_stale = self.path_available
            && reference.is_some_and(|at| now.saturating_sub(at) >= threshold)
            && self.last_send.is_some();
        if self.feedback_stale {
            self.reason = ControllerReason::FeedbackStale;
        }
    }

    fn update_confidence(&mut self, now: Duration, has_feedback: bool) {
        let decaying = self.feedback_stale || self.application_limited;
        if decaying {
            let (started, base) = *self
                .confidence_decay_started
                .get_or_insert((now, self.confidence));
            self.confidence = half_life(base, now.saturating_sub(started));
        } else {
            self.confidence_decay_started = None;
            if has_feedback {
                self.confidence = self
                    .confidence
                    .saturating_add((u16::MAX - self.confidence) / 8 + 1);
            }
        }
    }

    fn update_window(&mut self, now: Duration, input: &ControllerInput<'_>) {
        if now.saturating_sub(self.last_delay_update)
            >= self.smoothed_rtt.min(Duration::from_millis(25))
        {
            self.update_delay_filter();
            self.last_delay_update = now;
        }
        if now.saturating_sub(self.last_window_update) < self.smoothed_rtt
            || input.feedback.is_empty()
        {
            return;
        }
        let loss = self.loss_rate > ONE / 100
            || (self.loss_rate > 0 && self.queue_delay_average > self.effective_queue_target / 4);
        self.policer_detected =
            self.loss_rate > ONE / 10 && self.queue_delay_average < self.effective_queue_target / 4;
        if self.policer_detected {
            self.max_policed_window = mul_ratio(self.reference_window, 9, 10);
        } else if self.max_policed_window != u64::MAX {
            self.max_policed_window = mul_ratio(self.max_policed_window, 1_001, 1_000);
        }
        let classic_ce = input.ecn.classic
            && !input.ecn.l4s
            && !input.ecn.bleached
            && input
                .feedback
                .iter()
                .any(|sample| sample.ecn == Some(EcnMark::Ce));
        let l4s_ce = self.l4s_enabled && self.l4s_alpha > 0;
        let virtual_alpha = if self.queue_delay_average > self.effective_queue_target / 2 {
            ratio_between(
                self.queue_delay_average
                    .saturating_sub(self.effective_queue_target / 2),
                self.effective_queue_target / 2,
            )
        } else {
            0
        };
        let reaction_interval = self.smoothed_rtt.min(Duration::from_millis(25));
        let may_react = now.saturating_sub(self.last_reaction) >= reaction_interval;
        let congested = may_react && (loss || classic_ce || l4s_ce || virtual_alpha > 0);
        if congested {
            self.reference_inflection = self.reference_window;
            if loss {
                let backoff = if self.target_rate < mul_ratio(self.delivered_rate, 4, 5) {
                    3 * ONE / 40
                } else {
                    3 * ONE / 10
                };
                self.reference_window =
                    mul_fixed(self.reference_window, ONE.saturating_sub(backoff));
                self.loss_event_rate = ewma(self.loss_event_rate, ONE, 1, 200);
                self.reason = if self.policer_detected {
                    ControllerReason::Policer
                } else {
                    ControllerReason::Loss
                };
            } else if classic_ce {
                let backoff = if self.target_rate < mul_ratio(self.delivered_rate, 4, 5) {
                    ONE / 20
                } else {
                    ONE / 5
                };
                self.reference_window =
                    mul_fixed(self.reference_window, ONE.saturating_sub(backoff));
                self.reason = ControllerReason::ClassicEcn;
            } else {
                let alpha = if l4s_ce {
                    self.l4s_alpha
                } else {
                    virtual_alpha
                };
                let rtt_scale =
                    ONE.max(duration_ratio(self.smoothed_rtt, Duration::from_millis(25)));
                let mut backoff = (alpha / 2).saturating_mul(ONE) / rtt_scale;
                if self.target_rate < mul_ratio(self.delivered_rate, 4, 5) {
                    backoff /= 4;
                }
                self.reference_window =
                    mul_fixed(self.reference_window, ONE.saturating_sub(backoff));
                self.reason = if l4s_ce {
                    ControllerReason::L4s
                } else {
                    ControllerReason::Delay
                };
            }
            self.reference_window = self.reference_window.max(PROFILE.min_reference_window);
            self.last_congestion = now;
            self.last_reaction = now;
        } else {
            self.loss_event_rate = ewma(self.loss_event_rate, 0, 1, 200);
        }
        if !self.feedback_stale && !self.application_limited && !classic_ce && !l4s_ce {
            self.grow_window(now, input);
        }
        self.reference_window = self.reference_window.min(self.max_policed_window);
        self.adjust_native_target();
        self.max_bytes_in_flight_previous = self.max_bytes_in_flight;
        self.max_bytes_in_flight = input.bytes_in_flight;
        self.last_window_update = now;
    }

    fn update_delay_filter(&mut self) {
        if self.queue_delay < self.queue_delay_average {
            self.queue_delay_average = self.queue_delay;
        } else {
            self.queue_delay_average =
                ewma_duration(self.queue_delay_average, self.queue_delay, 1, 4);
        }
        self.queue_delay_max_average = mul_duration(self.queue_delay_max_average, 15, 16);
        self.queue_delay_min_average = duration_from_micros(
            (micros(self.queue_delay_min_average).saturating_mul(15)
                + micros(self.queue_delay_max_average))
                / 16,
        );
        let spread = self
            .queue_delay_max_average
            .saturating_sub(self.queue_delay_min_average);
        self.queue_delay_deviation = ewma_duration(self.queue_delay_deviation, spread, 1, 32);
        self.delay_scale = ONE.saturating_sub(
            micros(self.queue_delay_deviation)
                .saturating_mul(ONE)
                .checked_div(10_000)
                .unwrap_or(ONE)
                .min(ONE),
        );
    }

    fn grow_window(&mut self, now: Duration, input: &ControllerInput<'_>) {
        let newly_acked: u64 = input
            .feedback
            .iter()
            .map(|sample| u64::from(sample.transport_bytes))
            .sum();
        let marked: u64 = input
            .feedback
            .iter()
            .filter(|sample| sample.received && sample.ecn == Some(EcnMark::Ce))
            .map(|sample| u64::from(sample.transport_bytes))
            .sum();
        let ref_ratio = ONE.min(self.mss.saturating_mul(ONE) / self.reference_window.max(1));
        let mut increment = mul_fixed(newly_acked.saturating_sub(marked), ref_ratio);
        increment = mul_fixed(
            increment,
            duration_ratio(self.smoothed_rtt, Duration::from_millis(25)).min(ONE),
        );
        let distance = self.reference_window.abs_diff(self.reference_inflection);
        let scaled = distance.saturating_mul(8) / self.reference_inflection.max(1);
        let inflection_scale = mul_ratio(scaled, scaled, ONE).clamp(ONE / 10, ONE);
        increment = mul_fixed(increment, inflection_scale);
        increment = mul_fixed(increment, self.delay_scale.max(ONE / 10));
        let post = duration_ratio(
            now.saturating_sub(self.last_congestion),
            self.smoothed_rtt
                .max(Duration::from_millis(25))
                .saturating_mul(100),
        )
        .min(ONE);
        let multiplicative = ONE.saturating_add(mul_fixed(
            mul_fixed(
                self.reference_window.saturating_mul(ONE) / self.mss.max(1),
                ONE / 50,
            ),
            post,
        ));
        increment = mul_fixed(increment, multiplicative.max(ONE));
        let maximum = self.mss.saturating_add(mul_ratio(
            self.max_bytes_in_flight
                .max(self.max_bytes_in_flight_previous),
            3,
            2,
        ));
        let candidate = self.reference_window.saturating_add(increment);
        if candidate <= maximum
            && self.target_rate < input.desired_media_rate.min(PROFILE.target_max_bps)
        {
            self.reference_window = candidate;
        }
    }

    fn adjust_native_target(&mut self) {
        if self.competing_samples.len() < 50 {
            return;
        }
        let count = self.competing_samples.len() as u128;
        let sum: u128 = self
            .competing_samples
            .iter()
            .map(|value| u128::from(*value))
            .sum();
        let average = sum / count;
        let variance = self
            .competing_samples
            .iter()
            .map(|value| {
                let delta = i128::from(*value) - i128::try_from(average).unwrap_or(i128::MAX);
                delta.unsigned_abs().saturating_mul(delta.unsigned_abs())
            })
            .sum::<u128>()
            / count;
        let candidate_normalized = average.saturating_add(integer_sqrt(variance));
        let candidate = candidate_normalized
            .saturating_mul(u128::from(micros(PROFILE.queue_target_low)))
            / u128::from(ONE);
        let target = if self.loss_event_rate > mul_ratio(ONE, 2, 1_000) {
            mul_ratio(candidate.min(u128::from(u64::MAX)) as u64, 3, 2)
        } else if variance < u128::from(ONE).saturating_mul(u128::from(ONE)) / 5 {
            candidate.min(u128::from(u64::MAX)) as u64
        } else if candidate < u128::from(micros(PROFILE.queue_target_low)) {
            mul_ratio(micros(self.native_queue_target), 1, 2).max(candidate as u64)
        } else {
            mul_ratio(micros(self.native_queue_target), 9, 10)
        };
        self.native_queue_target =
            duration_from_micros(target).clamp(PROFILE.queue_target_low, PROFILE.queue_target_high);
    }

    fn derive_target(&mut self, desired: u64) {
        if desired == 0 {
            self.target_rate = 0;
            return;
        }
        let ref_ratio = ONE.min(self.mss.saturating_mul(ONE) / self.reference_window.max(1));
        let small_window = ONE.saturating_sub(ref_ratio.saturating_sub(ONE / 10).min(ONE / 5));
        let overhead = self.mss.saturating_mul(ONE) / self.mss.saturating_add(20);
        let raw = self
            .reference_window
            .saturating_mul(8_000_000)
            .checked_div(micros(self.smoothed_rtt).max(1))
            .unwrap_or(PROFILE.target_max_bps);
        let derived = mul_fixed(mul_fixed(raw, small_window), overhead).saturating_mul(5) / 6;
        self.target_rate = derived
            .clamp(PROFILE.target_min_bps, PROFILE.target_max_bps)
            .min(desired);
    }

    fn snapshot(
        &self,
        desired: u64,
        bytes_in_flight: u64,
        paced_queue_bytes: u64,
    ) -> SafeRtpEnvelope {
        let overhead = (3 * ONE / 2).saturating_add(mul_fixed(3 * ONE / 2, self.delay_scale));
        let maximum = mul_fixed(self.reference_window, overhead);
        // draft-ietf-ccwg-rfc8298bis-screamv2-01 section 4.3.2.
        let maximum_target = desired.clamp(1, PROFILE.target_max_bps);
        let nominal = self.target_rate.saturating_mul(ONE) / maximum_target;
        let relaxed = nominal
            .saturating_sub(mul_ratio(ONE, 4, 5))
            .saturating_mul(5)
            .min(ONE);
        let pacing_scale = ONE.saturating_sub(relaxed).max(ONE / 4);
        let pacing = self
            .target_rate
            .max(50_000)
            .saturating_mul(3)
            .saturating_mul(ONE)
            / 2
            / pacing_scale;
        let unmet = desired > self.target_rate.saturating_mul(6) / 5
            && desired.saturating_sub(self.target_rate) >= 50_000;
        SafeRtpEnvelope {
            target_media_payload_rate: self.target_rate,
            max_rtp_bytes_in_flight: maximum,
            pacing_transport_rate: pacing,
            reference_window: self.reference_window,
            native_queue_delay_target: self.native_queue_target,
            effective_queue_delay_target: self.effective_queue_target,
            queue_delay: self.queue_delay,
            queue_delay_confidence: self.confidence,
            smoothed_rtt: self.smoothed_rtt,
            feedback_hold: self.feedback_hold,
            delivered_rtp_transport_rate: self.delivered_rate,
            application_limited: self.application_limited,
            feedback_stale: self.feedback_stale,
            policer_detected: self.policer_detected,
            l4s_enabled: self.l4s_enabled,
            probe_permitted: self.path_available
                && !self.feedback_stale
                && !self.application_limited
                && unmet
                && bytes_in_flight.saturating_add(paced_queue_bytes) < maximum,
            reason: self.reason,
        }
    }
}

fn ewma(current: u64, sample: u64, numerator: u64, denominator: u64) -> u64 {
    current
        .saturating_mul(denominator.saturating_sub(numerator))
        .saturating_add(sample.saturating_mul(numerator))
        / denominator
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

fn half_life(base: u16, elapsed: Duration) -> u16 {
    let half_life = micros(PROFILE.confidence_half_life);
    let halves = micros(elapsed) / half_life;
    let remainder = micros(elapsed) % half_life;
    let shifted = u64::from(base) >> halves.min(15);
    let value = shifted.saturating_mul(half_life.saturating_mul(2).saturating_sub(remainder))
        / half_life.saturating_mul(2);
    value.min(u64::from(u16::MAX)) as u16
}

fn lerp_duration_even(low: Duration, high: Duration, numerator: u64, denominator: u64) -> Duration {
    duration_from_micros(lerp_even(micros(low), micros(high), numerator, denominator))
}

fn lerp_even(low: u64, high: u64, numerator: u64, denominator: u64) -> u64 {
    let denominator = denominator.max(1);
    let product = u128::from(high.saturating_sub(low)) * u128::from(numerator);
    let divisor = u128::from(denominator);
    let quotient = product / divisor;
    let remainder = product % divisor;
    let round_up = remainder.saturating_mul(2) > divisor
        || (remainder.saturating_mul(2) == divisor && quotient % 2 == 1);
    low.saturating_add(
        u64::try_from(quotient.saturating_add(u128::from(round_up))).unwrap_or(u64::MAX),
    )
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
    use proptest::prelude::*;

    fn input<'a>(feedback: &'a [FeedbackSample]) -> ControllerInput<'a> {
        ControllerInput {
            path_epoch: Some(1),
            path_available: true,
            feedback,
            feedback_hold: Duration::ZERO,
            bytes_in_flight: 3_000,
            paced_queue_bytes: 0,
            offered_media_rate: 4_000_000,
            admitted_media_rate: 300_000,
            desired_media_rate: 4_000_000,
            window_or_pacer_blocked: false,
            queue_delay_ceiling: Duration::from_millis(60),
            ecn: EcnValidation::default(),
        }
    }

    #[test]
    fn scream_profile_has_exact_startup_values() {
        let controller = ScreamController::new(4_000_000, None);
        let output = controller.snapshot(4_000_000, 0, 0);
        assert_eq!(output.target_media_payload_rate, 300_000);
        assert_eq!(output.reference_window, 3_750);
        assert_eq!(controller.mss, 1_000);
        assert_eq!(output.smoothed_rtt, Duration::from_millis(100));
    }

    #[test]
    fn stale_and_application_limited_never_grow_the_window() {
        let mut controller = ScreamController::new(4_000_000, None);
        controller.note_send(Duration::ZERO);
        let initial = controller.reference_window;
        let mut low = input(&[]);
        low.offered_media_rate = 64_000;
        controller.update(Duration::ZERO, low);
        let output = controller.update(Duration::from_millis(200), low);
        assert!(output.application_limited);
        assert_eq!(output.reference_window, initial);
        assert!(output.target_media_payload_rate <= 300_000);
        let stale = controller.update(Duration::from_millis(500), input(&[]));
        assert!(stale.feedback_stale);
        assert_eq!(stale.reference_window, initial);
        assert!(stale.target_media_payload_rate <= output.target_media_payload_rate);
    }

    #[test]
    fn selected_path_replacement_resets_only_path_evidence() {
        let mut controller = ScreamController::new(2_000_000, None);
        controller.path_epoch = Some(1);
        controller.path_available = true;
        controller.reference_window = 9_000;
        controller.path_baseline_micros = Some(50_000);
        let mut changed = input(&[]);
        changed.path_epoch = Some(2);
        let output = controller.update(Duration::from_secs(1), changed);
        assert_eq!(controller.path_baseline_micros, None);
        assert_eq!(output.reference_window, 9_000);
        assert_eq!(output.reason, ControllerReason::PathChanged);
    }

    #[test]
    fn latency_governor_is_monotonic_for_every_wire_tick() {
        let mut previous = LatencyGovernor::operating_point(0, 1_000_000);
        for tick in 1..=4_095 {
            let current = LatencyGovernor::operating_point(tick, 1_000_000);
            assert!(current.queue_delay_ceiling >= previous.queue_delay_ceiling);
            assert!(current.allocation_utilization >= previous.allocation_utilization);
            assert!(current.pacer_horizon >= previous.pacer_horizon);
            assert!(current.rtx_extra_allowance >= previous.rtx_extra_allowance);
            assert!(current.probe_queue_impact >= previous.probe_queue_impact);
            assert!(current.governed_demand >= previous.governed_demand);
            previous = current;
        }
        assert_eq!(previous.queue_delay_ceiling, Duration::from_millis(60));
        assert_eq!(previous.pacer_horizon, Duration::from_millis(80));
    }

    #[test]
    fn strictest_active_sender_controls_the_shared_ceiling() {
        let urgent = LatencyGovernor::operating_point(0, 1_000_000);
        let quality = LatencyGovernor::operating_point(50, 1_000_000);
        assert_eq!(
            LatencyGovernor::strictest_queue_ceiling([&quality, &urgent]),
            Some(Duration::from_millis(15))
        );
        assert_eq!(
            LatencyGovernor::strictest_queue_ceiling([&quality]),
            Some(Duration::from_millis(60))
        );
    }

    #[test]
    fn confidence_has_an_exact_five_second_half_life() {
        assert_eq!(half_life(u16::MAX, Duration::from_secs(5)), 32_767);
        assert_eq!(half_life(u16::MAX, Duration::from_secs(10)), 16_383);
    }

    #[test]
    fn policy_demand_changes_do_not_reset_network_state() {
        let mut controller = ScreamController::new(4_000_000, None);
        controller.reference_window = 12_345;
        controller.native_queue_target = Duration::from_millis(123);
        let mut changed = input(&[]);
        changed.desired_media_rate = 500_000;
        changed.queue_delay_ceiling = Duration::from_millis(30);
        let output = controller.update(Duration::ZERO, changed);
        assert_eq!(output.reference_window, 12_345);
        assert_eq!(output.native_queue_delay_target, Duration::from_millis(123));
        assert_eq!(
            output.effective_queue_delay_target,
            Duration::from_millis(30)
        );
    }

    #[test]
    fn scream_validated_ecn_only_and_bleaching_disables_l4s() {
        let sample = FeedbackSample {
            sent_at: Duration::ZERO,
            received_at: Duration::from_millis(50),
            transport_bytes: 1_000,
            received: true,
            receiver_arrival_micros: Some(25_000),
            ecn: Some(EcnMark::Ce),
        };
        let mut controller = ScreamController::new(2_000_000, None);
        let samples = [sample];
        let mut unvalidated = input(&samples);
        let before = controller.reference_window;
        controller.update(Duration::from_millis(100), unvalidated);
        assert!(controller.reference_window >= before);
        unvalidated.ecn = EcnValidation {
            classic: false,
            l4s: true,
            bleached: false,
        };
        let before_l4s = controller.reference_window;
        let l4s = controller.update(Duration::from_millis(200), unvalidated);
        assert!(l4s.l4s_enabled);
        assert!(l4s.reference_window <= before_l4s);
        unvalidated.ecn.bleached = true;
        let bleached = controller.update(Duration::from_millis(300), unvalidated);
        assert!(!bleached.l4s_enabled);
    }

    proptest! {
        #[test]
        fn governor_can_only_tighten(native_ms in 60_u64..=400, ceiling_ms in 0_u64..=500) {
            let mut controller = ScreamController::new(2_000_000, None);
            controller.native_queue_target = Duration::from_millis(native_ms);
            let mut values = input(&[]);
            values.queue_delay_ceiling = Duration::from_millis(ceiling_ms);
            let output = controller.update(Duration::ZERO, values);
            prop_assert!(output.effective_queue_delay_target <= output.native_queue_delay_target);
        }

        #[test]
        fn fixed_trace_is_deterministic(delays in prop::collection::vec(0_u64..100_000, 1..128)) {
            fn run(delays: &[u64]) -> SafeRtpEnvelope {
                let mut controller = ScreamController::new(2_000_000, None);
                let mut now = Duration::ZERO;
                let mut output = controller.snapshot(2_000_000, 0, 0);
                for delay in delays {
                    now += Duration::from_millis(50);
                    let sample = FeedbackSample {
                        sent_at: now.saturating_sub(Duration::from_millis(50)),
                        received_at: now,
                        transport_bytes: 1_000,
                        received: true,
                        receiver_arrival_micros: Some(
                            i64::try_from(micros(now).saturating_add(*delay)).unwrap_or(i64::MAX),
                        ),
                        ecn: None,
                    };
                    output = controller.update(now, input(&[sample]));
                }
                output
            }
            prop_assert_eq!(run(&delays), run(&delays));
        }

        #[test]
        fn scream_state_stays_bounded_for_extreme_inputs(
            offered in any::<u64>(),
            admitted in any::<u64>(),
            desired in any::<u64>(),
            bytes_in_flight in any::<u64>(),
        ) {
            let mut controller = ScreamController::new(desired, Some(u32::MAX));
            let output = controller.update(
                Duration::MAX,
                ControllerInput {
                    path_epoch: Some(u64::MAX),
                    path_available: true,
                    feedback: &[],
                    feedback_hold: Duration::MAX,
                    bytes_in_flight,
                    paced_queue_bytes: u64::MAX,
                    offered_media_rate: offered,
                    admitted_media_rate: admitted,
                    desired_media_rate: desired,
                    window_or_pacer_blocked: false,
                    queue_delay_ceiling: Duration::MAX,
                    ecn: EcnValidation::default(),
                },
            );
            prop_assert!(output.target_media_payload_rate <= PROFILE.target_max_bps.min(desired));
            prop_assert!(output.native_queue_delay_target <= PROFILE.queue_target_high);
            prop_assert!(output.effective_queue_delay_target <= output.native_queue_delay_target);
        }
    }
}
