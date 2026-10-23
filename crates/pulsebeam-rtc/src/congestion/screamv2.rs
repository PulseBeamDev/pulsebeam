// Copyright 2025 The WebRTC Project Authors. All rights reserved.
// Rust port of modules/congestion_controller/scream at libwebrtc
// 326fd06bb4d93900ed6947205b94cefd2413afca. See screamv2/LICENSE and PATENTS.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    reason = "the upstream SCReAM equations use bounded floating-point unit ratios; external counts and conversions saturate"
)]

use std::time::Duration;

mod delay;
mod feedback;
mod loss;
mod parameters;

use delay::DelayController;
use feedback::Feedback;
use loss::LossEstimator;
pub(super) use parameters::{DEFAULT as PARAMETERS, Parameters};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EcnMark {
    NotEct,
    Ect1,
    Ect0,
    Ce,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LossOrigin {
    Reordering,
    HistoryExpiry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FeedbackSample {
    pub(crate) sent_at: Duration,
    pub(crate) received_at: Duration,
    pub(crate) transport_bytes: u32,
    pub(crate) received: bool,
    pub(crate) newly_acked: bool,
    /// First network loss report, distinct from final transport-history expiry.
    pub(crate) lost: bool,
    pub(crate) recovered: bool,
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
    /// Receiver hold after the newest receipt, when the feedback format supplies it.
    pub(crate) feedback_hold: Duration,
    /// Adapter validation fence, not an application-limited classification.
    pub(crate) growth_eligible: bool,
    pub(crate) bytes_in_flight: u64,
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
    pub(crate) feedback_hold: Duration,
    pub(crate) delivered_rate: u64,
    pub(crate) application_limited: bool,
    pub(crate) last_window_decrease: Option<Duration>,
    pub(crate) last_congestion_reaction: Option<Duration>,
    pub(crate) policer_detected: bool,
    pub(crate) l4s_enabled: bool,
    pub(crate) reason: ControllerReason,
}

pub(crate) struct ScreamV2 {
    params: Parameters,
    target_bitrate: u64,
    ref_window: u64,
    ref_window_i: u64,
    ref_window_lowest: u64,
    last_ref_window_i_update: Option<Duration>,
    l4s_alpha: f64,
    loss: LossEstimator,
    feedback_hold: Duration,
    last_feedback_time: Option<Duration>,
    feedback_interval: Duration,
    last_flight_update: Option<Duration>,
    max_flight_this_rtt: u64,
    max_flight_prev_rtt: u64,
    allow_initial_ref_window_clamping: bool,
    received_rate: Option<u64>,
    accumulated_received_bytes: u64,
    last_window_receive_time: Option<i64>,
    last_congestion_reaction: Option<Duration>,
    last_window_decrease: Option<Duration>,
    drain_queue_start: Option<Duration>,
    queue_delay_avg_prev: Option<Duration>,
    data_in_flight_prev: u64,
    last_queue_delay_avg_check: Option<Duration>,
    delay: DelayController,
    first_feedback_processed: bool,
    application_limited: bool,
    ecn_mode: EcnMode,
    reason: ControllerReason,
}

impl ScreamV2 {
    pub(crate) fn new(target_bitrate_max: u64, _path_payload_max: Option<u32>) -> Self {
        Self::with_start(target_bitrate_max.min(300_000), PARAMETERS)
    }

    fn with_start(start: u64, params: Parameters) -> Self {
        Self {
            params,
            target_bitrate: start,
            ref_window: params.min_ref_window,
            ref_window_i: 1,
            ref_window_lowest: 0,
            last_ref_window_i_update: None,
            l4s_alpha: 0.0,
            loss: LossEstimator::default(),
            feedback_hold: Duration::ZERO,
            last_feedback_time: None,
            feedback_interval: Duration::ZERO,
            last_flight_update: None,
            max_flight_this_rtt: 0,
            max_flight_prev_rtt: 0,
            allow_initial_ref_window_clamping: params.allow_initial_ref_window_clamping,
            received_rate: Some(0),
            accumulated_received_bytes: 0,
            last_window_receive_time: None,
            last_congestion_reaction: None,
            last_window_decrease: None,
            drain_queue_start: None,
            queue_delay_avg_prev: None,
            data_in_flight_prev: 0,
            last_queue_delay_avg_check: None,
            delay: DelayController::new(params),
            first_feedback_processed: false,
            application_limited: false,
            ecn_mode: EcnMode::Disabled,
            reason: ControllerReason::Startup,
        }
    }

    pub(crate) fn reset_path(&mut self, target_bitrate_max: u64) {
        // Native route changes without restart retain the old target, not its
        // RTT/window evidence. Source and queue changes never call this method.
        self.reset_route(target_bitrate_max, None);
    }

    fn reset_route(&mut self, target_bitrate_max: u64, restart: Option<u64>) {
        let start = restart
            .map_or(self.target_bitrate, |requested| {
                if self.params.safe_reset_on_route_change {
                    requested.min(self.target_bitrate)
                } else {
                    requested
                }
            })
            .min(target_bitrate_max);
        *self = Self::with_start(start, self.params);
        self.reason = ControllerReason::PathChanged;
    }

    #[cfg(test)]
    pub(crate) fn test_flight_peak(&self) -> u64 {
        self.max_flight_this_rtt.max(self.max_flight_prev_rtt)
    }

    pub(crate) fn observe_bytes_in_flight(&mut self, bytes: u64) {
        self.max_flight_this_rtt = self.max_flight_this_rtt.max(bytes);
    }

    pub(crate) fn update(&mut self, now: Duration, input: ControllerInput<'_>) -> Output {
        if input.feedback.is_empty() || !input.growth_eligible {
            return self.snapshot(input.target_bitrate_max);
        }
        let now = input
            .feedback
            .iter()
            .filter(|sample| sample.received)
            .map(|sample| sample.received_at)
            .max()
            .unwrap_or(now)
            .min(now);
        let feedback = Feedback::parse(now, input, self.params);
        if feedback.num_received == 0 {
            return self.snapshot(input.target_bitrate_max);
        }
        self.ecn_mode = input.ecn_mode;
        self.observe_bytes_in_flight(input.bytes_in_flight);
        self.update_receive_rate(&feedback);
        if self.params.enable_alr {
            self.application_limited = self.max_allowed_ref_window() < self.ref_window
                && self.received_rate.is_some_and(|received| {
                    (received as f64) < self.params.alr_threshold * self.target_bitrate as f64
                });
        }
        if let Some(metrics) = feedback.delay {
            self.delay
                .update(now, metrics, self.application_limited, self.params);
            if !self.application_limited {
                if self.feedback_hold.is_zero() && self.params.feedback_hold_time_avg_g > 0.0 {
                    self.feedback_hold = metrics.feedback_hold;
                }
                self.feedback_hold = ewma_duration(
                    self.feedback_hold,
                    metrics.feedback_hold,
                    self.params.feedback_hold_time_avg_g,
                );
            }
        }
        if !self.first_feedback_processed {
            self.ref_window = self.params.min_ref_window.max(rate_bytes(
                self.target_bitrate,
                self.delay.rtt.saturating_add(self.feedback_hold),
            ));
            self.first_feedback_processed = true;
        }
        if let Some(last) = self.last_feedback_time {
            let delta = now.saturating_sub(last);
            if !delta.is_zero() {
                self.feedback_interval = if self.feedback_interval.is_zero() {
                    delta
                } else {
                    ewma_duration(
                        self.feedback_interval,
                        delta,
                        self.params.feedback_interval_avg_g,
                    )
                };
            }
        }
        self.last_feedback_time = Some(now);
        let steps_per_rtt = if self.params.scale_l4s_alpha_with_rtt {
            (self.delay.rtt.as_secs_f64() / self.reaction_interval().as_secs_f64()).max(1.0)
        } else {
            1.0
        };
        let gain = self.params.l4s_avg_g / steps_per_rtt;
        self.l4s_alpha = (gain * feedback.num_ce as f64 / feedback.num_received as f64
            + (1.0 - gain) * self.l4s_alpha)
            .clamp(0.0, 1.0);
        self.reason = ControllerReason::Feedback;
        self.update_ref_window(now, input.bytes_in_flight, &feedback);
        self.update_target_rate(now, input.target_bitrate_max);
        if elapsed(now, self.last_flight_update) >= self.params.virtual_rtt.max(self.delay.rtt) {
            self.last_flight_update = Some(now);
            self.max_flight_prev_rtt = self.max_flight_this_rtt;
            self.max_flight_this_rtt = 0;
        }
        self.snapshot(input.target_bitrate_max)
    }

    fn reaction_interval(&self) -> Duration {
        if self.params.use_feedback_interval_for_virtual_rtt {
            self.params.virtual_rtt.max(self.feedback_interval)
        } else {
            self.params.virtual_rtt
        }
    }

    fn scale_backoff(&self, mut backoff: f64, flight: u64) -> f64 {
        if self.params.use_spec_backoff
            && self.received_rate.is_some_and(|received| {
                (self.target_bitrate as f64) < self.params.acked_bitrate_margin * received as f64
            })
        {
            if self.params.inhibit_backoff_if_data_in_flight_decreases
                && flight < self.data_in_flight_prev
            {
                backoff = 0.0;
            } else {
                backoff *= self.params.backoff_scale_low_target_rate;
            }
        }
        backoff / (self.delay.rtt.as_secs_f64() / self.reaction_interval().as_secs_f64()).max(1.0)
    }

    fn update_ref_window(&mut self, now: Duration, flight: u64, feedback: &Feedback) {
        let ce = feedback.num_ce > 0;
        let mut loss = self.loss.update(
            now,
            feedback.num_lost,
            feedback.num_recovered,
            self.delay.rtt,
            self.params,
        );
        let queue_detected = self.delay.is_queue_delay_detected(self.params);
        if loss && !self.loss.congested() && !queue_detected {
            loss = false;
        }
        let since_reaction = elapsed(now, self.last_congestion_reaction);
        let reaction_interval = self.delay.rtt.min(self.reaction_interval());
        let virtual_ce = if self.params.use_spec_backoff {
            since_reaction >= reaction_interval
                && !loss
                && !ce
                && queue_detected
                && self.queue_delay_avg_prev.is_none_or(|previous| {
                    self.delay
                        .queue_delay
                        .is_some_and(|delay| delay >= previous)
                })
        } else {
            queue_detected
        };
        let previous_window = self.ref_window;
        if (virtual_ce || ce || loss) && since_reaction >= reaction_interval {
            self.last_congestion_reaction = Some(now);
            let mut backoff;
            if loss {
                backoff = 1.0 - self.params.beta_loss;
                self.reason = ControllerReason::Loss;
            } else if ce {
                backoff = self.scale_backoff(self.l4s_alpha / 2.0, flight);
                if !queue_detected {
                    backoff *= self.inflection_scale().max(0.25);
                    backoff *= self.delay.min_delay_scale(false, self.params).max(0.1);
                }
                if since_reaction
                    > self.params.virtual_rtt.max(self.delay.rtt).saturating_mul(
                        self.params
                            .number_of_rtts_between_reset_ref_window_i_on_congestion,
                    )
                    && since_reaction != Duration::MAX
                    && self.max_flight_prev_rtt > 0
                {
                    self.ref_window = self
                        .max_flight_prev_rtt
                        .clamp(self.params.min_ref_window, self.ref_window);
                }
                self.reason = if self.ecn_mode == EcnMode::Classic {
                    ControllerReason::ClassicEcn
                } else {
                    ControllerReason::L4s
                };
            } else {
                backoff = self.scale_backoff(self.delay.virtual_alpha(self.params) / 2.0, flight);
                self.reason = ControllerReason::Delay;
            }
            if self.allow_initial_ref_window_clamping {
                if let Some(received) = self.received_rate {
                    let clamped =
                        rate_bytes(received, self.delay.rtt.saturating_add(self.feedback_hold))
                            .clamp(self.params.min_ref_window, self.ref_window);
                    backoff = backoff.max(1.0 - clamped as f64 / self.ref_window as f64);
                    self.allow_initial_ref_window_clamping = false;
                }
            }
            self.ref_window = scale(self.ref_window, 1.0 - backoff);
        }
        if self.params.use_spec_backoff
            && (self.last_congestion_reaction == Some(now)
                || elapsed(now, self.last_queue_delay_avg_check) >= reaction_interval)
        {
            self.queue_delay_avg_prev = self.delay.queue_delay;
            self.data_in_flight_prev = flight;
            self.last_queue_delay_avg_check = Some(now);
        }
        if (!ce && self.loss.congestion_level < 0.01 && !virtual_ce)
            || self.last_congestion_reaction == Some(now)
        {
            let mut increase_scale = self.mss_ratio();
            let rtt_with_hold = self.delay.rtt.saturating_add(self.feedback_hold);
            if rtt_with_hold < self.params.virtual_rtt {
                increase_scale *=
                    rtt_with_hold.as_secs_f64() / self.params.virtual_rtt.as_secs_f64();
            }
            increase_scale *= self.inflection_scale().max(0.25);
            let min_delay_scale = self.delay.min_delay_scale(false, self.params);
            let latency_scale = self.delay.latency_difference_scale(self.params);
            increase_scale *= if self.params.use_min_delay_scale_on_increase {
                min_delay_scale.min(latency_scale)
            } else {
                min_delay_scale * latency_scale
            };
            let post_congestion_scale = (elapsed(now, self.last_congestion_reaction).as_secs_f64()
                / self
                    .params
                    .virtual_rtt
                    .max(self.delay.rtt)
                    .saturating_mul(self.params.post_congestion_delay_rtts)
                    .as_secs_f64())
            .clamp(0.0, 1.0);
            let multiplicative = self.params.multiplicative_increase_factor
                * self.ref_window as f64
                / self.params.max_segment_size as f64;
            increase_scale *= 1.0 + multiplicative * post_congestion_scale;
            let maximum = self.max_allowed_ref_window();
            if self.ref_window < maximum {
                self.ref_window = self
                    .ref_window
                    .saturating_add(scale(feedback.acked_not_marked, increase_scale))
                    .clamp(self.params.min_ref_window, maximum);
            }
        }
        if previous_window > self.ref_window {
            self.last_window_decrease = Some(now);
            if previous_window as f64 > self.ref_window_lowest as f64 * 1.05
                || elapsed(now, self.last_ref_window_i_update)
                    > self
                        .params
                        .virtual_rtt
                        .max(self.delay.rtt)
                        .saturating_mul(self.params.ref_window_i_update_max_hold_rtts)
            {
                self.ref_window_i = previous_window;
                self.last_ref_window_i_update = Some(now);
            }
            self.ref_window_lowest = self.ref_window;
        }
    }

    fn max_allowed_ref_window(&self) -> u64 {
        self.params
            .min_ref_window
            .max(self.params.max_segment_size.saturating_add(scale(
                self.max_flight_this_rtt.max(self.max_flight_prev_rtt),
                self.params.bytes_in_flight_head_room,
            )))
    }

    fn mss_ratio(&self) -> f64 {
        (self.params.max_segment_size as f64 / self.ref_window.max(1) as f64).min(1.0)
    }

    fn inflection_scale(&self) -> f64 {
        let difference = self.ref_window.abs_diff(self.ref_window_i) as f64;
        (self.params.backoff_scale_factor_close_to_ref_window_i * difference
            / self.ref_window_i.max(1) as f64)
            .powi(2)
            .clamp(0.1, 1.0)
    }

    fn update_target_rate(&mut self, now: Duration, maximum: u64) {
        let non_zero_rtt = self.delay.rtt.max(Duration::from_millis(1));
        let rate = rate(
            self.ref_window,
            non_zero_rtt.saturating_add(self.feedback_hold),
        );
        let mut target = scale(rate, 1.0 - (self.mss_ratio() - 0.1).clamp(0.0, 0.2));
        if !self.delay.is_queue_drained_in_time(now, self.params) {
            let started = *self.drain_queue_start.get_or_insert(now);
            if now.saturating_sub(started)
                < Duration::from_millis(100)
                    .max(non_zero_rtt.saturating_mul(self.params.queue_delay_drain_rtts))
            {
                target = scale(target, 0.5);
            } else {
                self.delay.reset_queue_delay();
            }
        } else {
            self.drain_queue_start = None;
        }
        self.target_bitrate = target.min(maximum);
    }

    fn update_receive_rate(&mut self, feedback: &Feedback) {
        self.accumulated_received_bytes = self
            .accumulated_received_bytes
            .saturating_add(feedback.received);
        let Some(delay) = feedback.delay else {
            return;
        };
        if let Some(last) = self.last_window_receive_time {
            let interval = i128::from(delay.last_packet_receive_time) - i128::from(last);
            if interval >= micros_i128(self.params.received_rate_window) {
                self.received_rate = Some(rate(
                    self.accumulated_received_bytes,
                    duration_micros(interval),
                ));
                self.accumulated_received_bytes = 0;
                self.last_window_receive_time = Some(delay.last_packet_receive_time);
            }
        } else {
            self.last_window_receive_time = Some(delay.last_packet_receive_time);
            self.received_rate = None;
            self.accumulated_received_bytes = 0;
        }
    }

    pub(crate) fn snapshot(&self, target_bitrate_max: u64) -> Output {
        let target = self.target_bitrate.min(target_bitrate_max);
        let overhead = self.params.ref_window_overhead_min
            + (self.params.ref_window_overhead_max - self.params.ref_window_overhead_min)
                * self.delay.min_delay_scale(true, self.params);
        let pacing = scale(target, self.params.pacing_factor).max(
            self.received_rate.map_or(0, |received| {
                scale(received, self.params.pacing_rate_received_factor)
            }),
        );
        Output {
            target_bitrate: target,
            max_bytes_in_flight: scale(self.ref_window, overhead).max(
                self.params
                    .max_segment_size
                    .saturating_mul(u64::from(self.params.min_max_data_in_flight_mss)),
            ),
            pacing_rate: pacing,
            reference_window: self.ref_window,
            queue_delay_target: self.params.queue_delay_target,
            queue_delay: self.delay.queue_delay.unwrap_or_default(),
            smoothed_rtt: self.delay.rtt,
            feedback_hold: self.feedback_hold,
            delivered_rate: self.received_rate.unwrap_or_default(),
            application_limited: self.application_limited,
            last_window_decrease: self.last_window_decrease,
            last_congestion_reaction: self.last_congestion_reaction,
            policer_detected: false,
            l4s_enabled: self.ecn_mode == EcnMode::L4s,
            reason: self.reason,
        }
    }
}

fn elapsed(now: Duration, previous: Option<Duration>) -> Duration {
    previous.map_or(Duration::MAX, |previous| now.saturating_sub(previous))
}

fn micros_i128(value: Duration) -> i128 {
    i128::try_from(value.as_micros()).unwrap_or(i128::MAX)
}

fn duration_micros(value: i128) -> Duration {
    Duration::from_micros(u64::try_from(value.max(0)).unwrap_or(u64::MAX))
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "nonnegative native unit conversion rounds and saturates at the integer boundary"
)]
fn scale(value: u64, factor: f64) -> u64 {
    (value as f64 * factor.max(0.0)).round() as u64
}

fn ewma_duration(current: Duration, sample: Duration, gain: f64) -> Duration {
    Duration::from_micros(
        scale(
            u64::try_from(current.as_micros()).unwrap_or(u64::MAX),
            1.0 - gain,
        )
        .saturating_add(scale(
            u64::try_from(sample.as_micros()).unwrap_or(u64::MAX),
            gain,
        )),
    )
}

fn rate(bytes: u64, interval: Duration) -> u64 {
    u64::try_from(u128::from(bytes).saturating_mul(8_000_000) / interval.as_micros().max(1))
        .unwrap_or(u64::MAX)
}

fn rate_bytes(bps: u64, interval: Duration) -> u64 {
    u64::try_from(u128::from(bps).saturating_mul(interval.as_micros()) / 8_000_000)
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn sample(sent_ms: u64, arrival_us: i64) -> FeedbackSample {
        FeedbackSample {
            sent_at: Duration::from_millis(sent_ms),
            received_at: Duration::from_millis(sent_ms.saturating_add(100)),
            transport_bytes: 1_000,
            received: true,
            newly_acked: true,
            lost: false,
            recovered: false,
            receiver_arrival_micros: Some(arrival_us),
            ecn: None,
        }
    }

    pub(super) fn input(feedback: &[FeedbackSample]) -> ControllerInput<'_> {
        ControllerInput {
            feedback,
            feedback_hold: Duration::ZERO,
            growth_eligible: true,
            bytes_in_flight: 0,
            target_bitrate_max: 100_000_000,
            ecn_mode: EcnMode::Disabled,
        }
    }

    #[test]
    fn upstream_initial_window_mss_and_pacing_defaults() {
        let core = ScreamV2::new(100_000_000, Some(300));
        let output = core.snapshot(100_000_000);
        assert_eq!(output.reference_window, 1_000);
        assert_eq!(output.max_bytes_in_flight, 5_120);
        assert_eq!(output.target_bitrate, 300_000);
        assert_eq!(output.pacing_rate, 330_000);
        assert_eq!(output.smoothed_rtt, Duration::ZERO);
    }

    #[test]
    fn pre_feedback_flight_peak_survives_ack_retirement_and_timer_polls() {
        let mut core = ScreamV2::new(100_000_000, None);
        core.observe_bytes_in_flight(12_000);
        core.update(Duration::from_millis(100), input(&[sample(0, 50_000)]));
        assert_eq!(core.test_flight_peak(), 12_000);
        let before = core.snapshot(100_000_000);
        core.update(Duration::from_secs(1), input(&[]));
        assert_eq!(core.snapshot(100_000_000), before);
    }

    #[test]
    fn upstream_receiver_time_window_not_send_to_feedback_age() {
        let mut core = ScreamV2::new(100_000_000, None);
        core.update(Duration::from_millis(100), input(&[sample(0, -900_000)]));
        assert_eq!(core.received_rate, None);
        core.update(Duration::from_millis(200), input(&[sample(100, -800_000)]));
        assert_eq!(core.received_rate, Some(80_000));
        core.update(Duration::from_millis(250), input(&[sample(150, -750_000)]));
        assert_eq!(core.received_rate, Some(80_000));
        core.update(Duration::from_millis(300), input(&[sample(200, -700_000)]));
        assert_eq!(core.received_rate, Some(160_000));
    }

    #[test]
    fn upstream_alr_needs_flight_headroom_and_finite_low_received_rate() {
        let mut core = ScreamV2::new(100_000_000, None);
        core.update(Duration::from_millis(100), input(&[sample(0, 50_000)]));
        assert!(!core.application_limited);
        core.ref_window = 30_000;
        core.target_bitrate = 2_000_000;
        core.update(Duration::from_millis(200), input(&[sample(100, 150_000)]));
        assert!(core.application_limited);
        core.observe_bytes_in_flight(40_000);
        core.update(Duration::from_millis(225), input(&[sample(125, 175_000)]));
        assert!(!core.application_limited);
    }

    #[test]
    fn upstream_received_rate_pacing_floor_and_safe_route_reset() {
        let mut core = ScreamV2::new(100_000_000, None);
        core.target_bitrate = 500_000;
        core.received_rate = Some(2_000_000);
        core.ref_window = 50_000;
        assert_eq!(core.snapshot(100_000_000).pacing_rate, 1_600_000);
        core.reset_path(100_000_000);
        let output = core.snapshot(100_000_000);
        assert_eq!(output.target_bitrate, 500_000);
        assert_eq!(output.reference_window, 1_000);
        assert_eq!(core.received_rate, Some(0));
        assert_eq!(output.reason, ControllerReason::PathChanged);
    }

    #[test]
    fn ce_credit_and_lingering_alpha_do_not_replay_a_congestion_event() {
        let mut core = ScreamV2::new(100_000_000, None);
        core.observe_bytes_in_flight(30_000);
        let data = [FeedbackSample {
            ecn: Some(EcnMark::Ce),
            ..sample(0, 50_000)
        }];
        let mut args = input(&data);
        args.ecn_mode = EcnMode::L4s;
        core.update(Duration::from_millis(100), args);
        assert_eq!(core.l4s_alpha, PARAMETERS.l4s_avg_g / 4.0);
        let reaction = core.last_congestion_reaction;
        core.update(Duration::from_millis(101), input(&[sample(1, 51_000)]));
        assert_eq!(core.last_congestion_reaction, reaction);
        assert!(core.l4s_alpha > 0.0);
    }

    #[test]
    fn extreme_accepted_timestamps_sizes_and_feedback_without_arrival_are_bounded() {
        let mut core = ScreamV2::new(u64::MAX, None);
        let data = [FeedbackSample {
            transport_bytes: u32::MAX,
            receiver_arrival_micros: Some(i64::MIN),
            ..sample(0, 0)
        }];
        let mut args = input(&data);
        args.bytes_in_flight = u64::MAX;
        args.feedback_hold = Duration::MAX;
        core.update(Duration::MAX, args);
        let before = core.delay.queue_delay;
        core.update(
            Duration::MAX,
            input(&[FeedbackSample {
                receiver_arrival_micros: None,
                ..sample(1, 0)
            }]),
        );
        assert_eq!(core.delay.queue_delay, before);
        assert!(core.snapshot(100_000_000).target_bitrate <= 100_000_000);
    }
}
