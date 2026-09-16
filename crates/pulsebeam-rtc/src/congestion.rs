#![allow(
    dead_code,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "the PulseBeam adapter uses bounded fixed-point policy interpolation"
)]

use std::time::Duration;

mod screamv2;

pub(crate) use screamv2::{EcnMark, FeedbackSample};

const ONE: u64 = 65_536;
const TARGET_BITRATE_MAX: u64 = 100_000_000;

#[cfg(test)]
#[path = "congestion/scenario.rs"]
pub(crate) mod scenario;

/// PulseBeam-side validation of ECN capability. This selects an RFC-defined SCReAMv2
/// mode; it does not tune the congestion-control algorithm.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct EcnValidation {
    pub(crate) classic: bool,
    pub(crate) l4s: bool,
    pub(crate) bleached: bool,
}

impl EcnValidation {
    fn mode(self) -> screamv2::EcnMode {
        if self.bleached {
            screamv2::EcnMode::Disabled
        } else if self.l4s {
            screamv2::EcnMode::L4s
        } else if self.classic {
            screamv2::EcnMode::Classic
        } else {
            screamv2::EcnMode::Disabled
        }
    }
}

/// Adapter inputs owned by PulseBeam. `desired_media_rate` maps to the draft's
/// application-controlled TARGET_BITRATE_MAX. Other PulseBeam policy fields may govern
/// scheduling/probing outside the isolated SCReAMv2 core but MUST NOT change its window,
/// qdelay target, gains, pacing equations, or feedback semantics.
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
    /// Legacy PulseBeam policy seam. It is intentionally NOT passed to SCReAMv2.
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

impl From<screamv2::ControllerReason> for ControllerReason {
    fn from(value: screamv2::ControllerReason) -> Self {
        match value {
            screamv2::ControllerReason::Startup => Self::Startup,
            screamv2::ControllerReason::Feedback => Self::Feedback,
            screamv2::ControllerReason::Delay => Self::Delay,
            screamv2::ControllerReason::Loss => Self::Loss,
            screamv2::ControllerReason::ClassicEcn => Self::ClassicEcn,
            screamv2::ControllerReason::L4s => Self::L4s,
            screamv2::ControllerReason::PathChanged => Self::PathChanged,
            screamv2::ControllerReason::Policer => Self::Policer,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SafeRtpEnvelope {
    pub(crate) target_media_payload_rate: u64,
    pub(crate) max_rtp_bytes_in_flight: u64,
    pub(crate) pacing_transport_rate: u64,
    pub(crate) reference_window: u64,
    pub(crate) native_queue_delay_target: Duration,
    /// Kept for the existing stats surface. It is exactly the native SCReAMv2 target;
    /// PulseBeam policy no longer overrides it.
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

/// PulseBeam latency policy. These values govern media usefulness, allocation, pacer
/// horizon, repair, and probing outside SCReAMv2. In particular, queue_delay_ceiling is
/// NOT a congestion-control input.
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
        let urgent_playout_knee = Duration::from_millis(75);
        let quality_playout_saturation = Duration::from_millis(500);
        let playout = Duration::from_millis(u64::from(playout_max_ticks).saturating_mul(10));
        let numerator = if playout_max_ticks == 0 || playout <= urgent_playout_knee {
            0
        } else {
            micros(playout.saturating_sub(urgent_playout_knee)).min(micros(
                quality_playout_saturation.saturating_sub(urgent_playout_knee),
            ))
        };
        let denominator = micros(quality_playout_saturation.saturating_sub(urgent_playout_knee));
        let allocation_utilization = lerp_even(52_429, 62_259, numerator, denominator);
        SenderOperatingPoint {
            queue_delay_ceiling: lerp_duration_even(
                Duration::from_millis(15),
                Duration::from_millis(60),
                numerator,
                denominator,
            ),
            allocation_utilization,
            pacer_horizon: lerp_duration_even(
                Duration::from_millis(15),
                Duration::from_millis(80),
                numerator,
                denominator,
            ),
            rtx_extra_allowance: lerp_duration_even(
                Duration::ZERO,
                Duration::from_millis(50),
                numerator,
                denominator,
            ),
            probe_queue_impact: lerp_duration_even(
                Duration::from_millis(5),
                Duration::from_millis(20),
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
    core: screamv2::ScreamV2,
    path_epoch: Option<u64>,
    path_available: bool,
    last_feedback: Option<Duration>,
    last_send: Option<Duration>,
    application_limited_since: Option<Duration>,
    application_limited: bool,
    feedback_stale: bool,
    confidence: u16,
    confidence_decay_started: Option<(Duration, u16)>,
    feedback_hold: Duration,
    last_reason: ControllerReason,
}

impl ScreamController {
    pub(crate) fn new(desired_media_rate: u64, path_payload_max: Option<u32>) -> Self {
        Self {
            core: screamv2::ScreamV2::new(
                desired_media_rate.min(TARGET_BITRATE_MAX),
                path_payload_max,
            ),
            path_epoch: None,
            path_available: false,
            last_feedback: None,
            last_send: None,
            application_limited_since: None,
            application_limited: false,
            feedback_stale: false,
            confidence: u16::MAX,
            confidence_decay_started: None,
            feedback_hold: Duration::ZERO,
            last_reason: ControllerReason::Startup,
        }
    }

    pub(crate) fn note_send(&mut self, at: Duration) {
        self.last_send = Some(at);
    }

    pub(crate) fn update(&mut self, now: Duration, input: ControllerInput<'_>) -> SafeRtpEnvelope {
        self.update_path(input.path_epoch, input.path_available);
        self.feedback_hold = input.feedback_hold;
        self.update_application_limited(now, &input);
        if !input.feedback.is_empty() {
            self.last_feedback = Some(now);
            self.feedback_stale = false;
        }
        self.update_staleness(now);
        self.update_confidence(now, !input.feedback.is_empty());

        // SCReAMv2 sees only draft-defined algorithm inputs. PulseBeam's playout-derived
        // queue_delay_ceiling, offered/admitted rates, and pacer state remain outside it.
        let output = self.core.update(
            now,
            screamv2::ControllerInput {
                feedback: input.feedback,
                feedback_hold: input.feedback_hold,
                bytes_in_flight: input.bytes_in_flight,
                target_bitrate_max: input.desired_media_rate.min(TARGET_BITRATE_MAX),
                ecn_mode: input.ecn.mode(),
            },
        );
        let mut reason = ControllerReason::from(output.reason);
        if self.feedback_stale {
            reason = ControllerReason::FeedbackStale;
        } else if self.application_limited {
            reason = ControllerReason::ApplicationLimited;
        }
        self.last_reason = reason;
        self.envelope(output, &input, reason)
    }

    pub(crate) fn snapshot(
        &self,
        desired: u64,
        bytes_in_flight: u64,
        paced_queue_bytes: u64,
    ) -> SafeRtpEnvelope {
        let output = self.core.snapshot(desired.min(TARGET_BITRATE_MAX));
        let input = ControllerInput {
            path_epoch: self.path_epoch,
            path_available: self.path_available,
            feedback: &[],
            feedback_hold: self.feedback_hold,
            bytes_in_flight,
            paced_queue_bytes,
            offered_media_rate: 0,
            admitted_media_rate: 0,
            desired_media_rate: desired,
            window_or_pacer_blocked: false,
            queue_delay_ceiling: Duration::ZERO,
            ecn: EcnValidation::default(),
        };
        self.envelope(output, &input, self.last_reason)
    }

    fn update_path(&mut self, epoch: Option<u64>, available: bool) {
        if self.path_epoch == epoch && self.path_available == available {
            return;
        }
        self.path_epoch = epoch;
        self.path_available = available;
        self.last_feedback = None;
        self.feedback_stale = false;
        self.core.reset_path_evidence();
        self.last_reason = ControllerReason::PathChanged;
    }

    fn update_application_limited(&mut self, now: Duration, input: &ControllerInput<'_>) {
        let credible = input.desired_media_rate.max(input.admitted_media_rate);
        let below = input.offered_media_rate.saturating_mul(100) < credible.saturating_mul(85)
            && !input.window_or_pacer_blocked;
        if below {
            let since = *self.application_limited_since.get_or_insert(now);
            if now.saturating_sub(since) >= Duration::from_millis(200) {
                self.application_limited = true;
            }
        } else {
            self.application_limited_since = None;
            self.application_limited = false;
        }
    }

    fn update_staleness(&mut self, now: Duration) {
        let output = self.core.snapshot(TARGET_BITRATE_MAX);
        let threshold = output
            .smoothed_rtt
            .saturating_mul(3)
            .clamp(Duration::from_millis(500), Duration::from_secs(2));
        let reference = self.last_feedback.or(self.last_send);
        self.feedback_stale = self.path_available
            && reference.is_some_and(|at| now.saturating_sub(at) >= threshold)
            && self.last_send.is_some();
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

    fn envelope(
        &self,
        output: screamv2::Output,
        input: &ControllerInput<'_>,
        reason: ControllerReason,
    ) -> SafeRtpEnvelope {
        let unmet = input.desired_media_rate > output.target_bitrate.saturating_mul(6) / 5
            && input
                .desired_media_rate
                .saturating_sub(output.target_bitrate)
                >= 50_000;
        SafeRtpEnvelope {
            target_media_payload_rate: output.target_bitrate,
            max_rtp_bytes_in_flight: output.max_bytes_in_flight,
            pacing_transport_rate: output.pacing_rate,
            reference_window: output.reference_window,
            native_queue_delay_target: output.queue_delay_target,
            effective_queue_delay_target: output.queue_delay_target,
            queue_delay: output.queue_delay,
            queue_delay_confidence: self.confidence,
            smoothed_rtt: output.smoothed_rtt,
            feedback_hold: self.feedback_hold,
            delivered_rtp_transport_rate: output.delivered_rate,
            application_limited: self.application_limited,
            feedback_stale: self.feedback_stale,
            policer_detected: output.policer_detected,
            l4s_enabled: output.l4s_enabled,
            probe_permitted: self.path_available
                && !self.feedback_stale
                && !self.application_limited
                && unmet
                && input
                    .bytes_in_flight
                    .saturating_add(input.paced_queue_bytes)
                    < output.max_bytes_in_flight,
            reason,
        }
    }
}

fn micros(value: Duration) -> u64 {
    value.as_micros().min(u128::from(u64::MAX)) as u64
}

fn mul_fixed(value: u64, factor: u64) -> u64 {
    ((u128::from(value) * u128::from(factor)) / u128::from(ONE)).min(u128::from(u64::MAX)) as u64
}

fn duration_from_micros(value: u64) -> Duration {
    Duration::from_micros(value)
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

fn half_life(base: u16, elapsed: Duration) -> u16 {
    let half_life = 5_000_000_u64;
    let elapsed = micros(elapsed);
    let halves = elapsed / half_life;
    let remainder = elapsed % half_life;
    let shifted = u64::from(base) >> halves.min(15);
    let value = shifted.saturating_mul(half_life.saturating_mul(2).saturating_sub(remainder))
        / half_life.saturating_mul(2);
    value.min(u64::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulsebeam_queue_policy_cannot_change_scream_state() {
        let mut tight = ScreamController::new(2_000_000, None);
        let mut loose = ScreamController::new(2_000_000, None);
        let base = ControllerInput {
            path_epoch: Some(1),
            path_available: true,
            feedback: &[],
            feedback_hold: Duration::ZERO,
            bytes_in_flight: 0,
            paced_queue_bytes: 0,
            offered_media_rate: 0,
            admitted_media_rate: 0,
            desired_media_rate: 2_000_000,
            window_or_pacer_blocked: false,
            queue_delay_ceiling: Duration::from_millis(15),
            ecn: EcnValidation::default(),
        };
        let tight_output = tight.update(Duration::ZERO, base);
        let loose_output = loose.update(
            Duration::ZERO,
            ControllerInput {
                queue_delay_ceiling: Duration::from_millis(500),
                ..base
            },
        );
        assert_eq!(tight_output.reference_window, loose_output.reference_window);
        assert_eq!(
            tight_output.native_queue_delay_target,
            loose_output.native_queue_delay_target
        );
        assert_eq!(
            tight_output.effective_queue_delay_target,
            tight_output.native_queue_delay_target
        );
    }

    #[test]
    fn latency_governor_remains_outer_policy_only() {
        let urgent = LatencyGovernor::operating_point(0, 1_000_000);
        let quality = LatencyGovernor::operating_point(50, 1_000_000);
        assert!(quality.queue_delay_ceiling >= urgent.queue_delay_ceiling);
        assert!(quality.pacer_horizon >= urgent.pacer_horizon);
    }
}
