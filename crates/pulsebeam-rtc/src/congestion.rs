use std::time::Duration;

mod scenario;
mod screamv2;

pub(crate) use scenario::{ScenarioMetrics, run_fixed_scenario_matrix};
pub(crate) use screamv2::{EcnMark, FeedbackSample};

const ONE: u64 = 65_536;
const TARGET_BITRATE_MAX: u64 = 100_000_000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct EcnValidation {
    pub(crate) classic: bool,
    pub(crate) l4s: bool,
    pub(crate) bleached: bool,
}

impl EcnValidation {
    const fn mode(self) -> screamv2::EcnMode {
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
    ApplicationLimited,
    FeedbackStale,
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct ControllerInput<'a> {
    pub(crate) path_epoch: Option<u64>,
    pub(crate) path_available: bool,
    pub(crate) feedback: &'a [FeedbackSample],
    pub(crate) feedback_hold: Duration,
    /// True only when the current network feedback report covered a committed
    /// packet. Locally synthesized expiry/loss evidence does not set this.
    pub(crate) fresh_network_feedback: bool,
    pub(crate) bytes_in_flight: u64,
    pub(crate) paced_queue_bytes: u64,
    pub(crate) offered_media_rate: u64,
    pub(crate) admitted_media_rate: u64,
    pub(crate) desired_media_rate: u64,
    pub(crate) window_or_pacer_blocked: bool,
    pub(crate) ecn: EcnValidation,
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct SenderOperatingPoint {
    pub(crate) allocation_utilization: u64,
    pub(crate) pacer_horizon: Duration,
    pub(crate) rtx_extra_allowance: Duration,
    pub(crate) probe_queue_impact: Duration,
    pub(crate) governed_demand: u64,
}

pub(crate) struct LatencyGovernor;

impl LatencyGovernor {
    pub(crate) fn operating_point(playout_max_ticks: u16, desired_bitrate: u64) -> SenderOperatingPoint {
        let playout_max = Duration::from_millis(u64::from(playout_max_ticks) * 10);
        let low = Duration::from_millis(75);
        let high = Duration::from_millis(500);
        let x = if playout_max_ticks == 0 || playout_max <= low {
            0
        } else if playout_max >= high {
            ONE
        } else {
            playout_max
                .saturating_sub(low)
                .as_micros()
                .saturating_mul(u128::from(ONE))
                .checked_div(high.saturating_sub(low).as_micros().max(1))
                .unwrap_or(0)
                .min(u128::from(ONE)) as u64
        };
        let allocation_utilization = lerp_even(80 * ONE / 100, 95 * ONE / 100, x, ONE);
        let pacer_horizon = lerp_duration_even(
            Duration::from_millis(15),
            Duration::from_millis(80),
            x,
            ONE,
        );
        let rtx_extra_allowance = lerp_duration_even(
            Duration::ZERO,
            Duration::from_millis(50),
            x,
            ONE,
        );
        let probe_queue_impact = lerp_duration_even(
            Duration::from_millis(5),
            Duration::from_millis(20),
            x,
            ONE,
        );
        SenderOperatingPoint {
            allocation_utilization,
            pacer_horizon,
            rtx_extra_allowance,
            probe_queue_impact,
            governed_demand: mul_fixed(desired_bitrate, allocation_utilization),
        }
    }
}

pub(crate) struct ScreamController {
    core: screamv2::ScreamV2,
    pub(crate) path_epoch: Option<u64>,
    path_available: bool,
    last_feedback: Option<Duration>,
    last_send: Option<Duration>,
    pub(crate) application_limited_since: Option<Duration>,
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
        self.update_path(
            input.path_epoch,
            input.path_available,
            input.desired_media_rate.min(TARGET_BITRATE_MAX),
        );
        self.feedback_hold = input.feedback_hold;
        self.update_application_limited(now, &input);
        if input.fresh_network_feedback {
            self.last_feedback = Some(now);
            self.feedback_stale = false;
        }
        self.update_staleness(now);
        self.update_confidence(now, input.fresh_network_feedback);

        // SCReAMv2 sees only draft-defined algorithm inputs. PulseBeam's playout,
        // offered/admitted-rate, and pacer policy remain outside it.
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
            fresh_network_feedback: false,
            bytes_in_flight,
            paced_queue_bytes,
            offered_media_rate: 0,
            admitted_media_rate: 0,
            desired_media_rate: desired,
            window_or_pacer_blocked: false,
            ecn: EcnValidation::default(),
        };
        self.envelope(output, &input, self.last_reason)
    }

    fn update_path(&mut self, epoch: Option<u64>, available: bool, target_bitrate_max: u64) {
        if self.path_epoch == epoch && self.path_available == available {
            return;
        }
        self.path_epoch = epoch;
        self.path_available = available;
        // Everything below is evidence about the selected path, not application
        // policy. A replacement starts in the same unproven state as a new path.
        self.last_feedback = None;
        self.last_send = None;
        self.application_limited_since = None;
        self.application_limited = false;
        self.feedback_stale = false;
        self.confidence = u16::MAX;
        self.confidence_decay_started = None;
        self.feedback_hold = Duration::ZERO;
        self.core.reset_path(target_bitrate_max);
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

    fn base_input<'a>(feedback: &'a [FeedbackSample]) -> ControllerInput<'a> {
        ControllerInput {
            path_epoch: Some(1),
            path_available: true,
            feedback,
            feedback_hold: Duration::ZERO,
            fresh_network_feedback: false,
            bytes_in_flight: 0,
            paced_queue_bytes: 0,
            offered_media_rate: 2_000_000,
            admitted_media_rate: 2_000_000,
            desired_media_rate: 2_000_000,
            window_or_pacer_blocked: false,
            ecn: EcnValidation::default(),
        }
    }

    #[test]
    fn synthetic_loss_does_not_refresh_feedback_freshness() {
        let mut cc = ScreamController::new(2_000_000, None);
        cc.note_send(Duration::ZERO);
        let base = base_input(&[]);
        cc.update(Duration::from_millis(600), base);
        assert!(cc.feedback_stale);
        let loss = [FeedbackSample {
            sent_at: Duration::ZERO,
            received_at: Duration::from_millis(600),
            transport_bytes: 1_000,
            received: false,
            newly_acked: false,
            lost: true,
            receiver_arrival_micros: None,
            ecn: None,
        }];
        let output = cc.update(
            Duration::from_millis(650),
            ControllerInput {
                feedback: &loss,
                fresh_network_feedback: false,
                ..base_input(&loss)
            },
        );
        assert!(output.feedback_stale);
        assert_eq!(cc.last_feedback, None);
    }

    #[test]
    fn draft_bytes_in_flight_gate_limits_growth_under_low_offer() {
        let mut cc = ScreamController::new(4_000_000, None);
        cc.note_send(Duration::ZERO);
        let mut base = base_input(&[]);
        base.bytes_in_flight = 1_000;
        base.offered_media_rate = 64_000;
        base.admitted_media_rate = 64_000;
        base.desired_media_rate = 4_000_000;
        cc.update(Duration::ZERO, base);
        let entered = cc.update(Duration::from_millis(250), base);
        assert!(entered.application_limited);
        let before = entered.reference_window;
        let feedback = [FeedbackSample {
            sent_at: Duration::from_millis(240),
            received_at: Duration::from_millis(300),
            transport_bytes: 1_000,
            received: true,
            newly_acked: true,
            lost: false,
            receiver_arrival_micros: None,
            ecn: None,
        }];
        let after = cc.update(
            Duration::from_millis(300),
            ControllerInput {
                feedback: &feedback,
                fresh_network_feedback: true,
                ..base
            },
        );
        assert!(after.application_limited);
        assert_eq!(after.reference_window, before);
    }

    #[test]
    fn confidence_has_five_second_half_life_while_application_limited() {
        let mut cc = ScreamController::new(2_000_000, None);
        cc.note_send(Duration::ZERO);
        let mut input = base_input(&[]);
        input.offered_media_rate = 64_000;
        input.admitted_media_rate = 64_000;
        cc.update(Duration::ZERO, input);
        let started = cc.update(Duration::from_millis(250), input);
        assert!(started.application_limited);
        let half = cc.update(Duration::from_millis(5_250), input);
        assert_eq!(half.queue_delay_confidence, u16::MAX / 2);
    }

    #[test]
    fn path_replacement_resets_outer_path_evidence() {
        let mut cc = ScreamController::new(2_000_000, None);
        cc.note_send(Duration::ZERO);
        let mut input = base_input(&[]);
        input.offered_media_rate = 64_000;
        input.admitted_media_rate = 64_000;
        cc.update(Duration::ZERO, input);
        let stale_alr = cc.update(Duration::from_secs(3), input);
        assert!(stale_alr.feedback_stale);
        assert!(stale_alr.application_limited);

        input.path_epoch = Some(2);
        let replaced = cc.update(Duration::from_secs(3), input);
        assert!(!replaced.feedback_stale);
        assert!(!replaced.application_limited);
        assert_eq!(cc.last_send, None);
        assert_eq!(cc.last_feedback, None);
        assert_eq!(cc.application_limited_since, Some(Duration::from_secs(3)));
        assert_eq!(replaced.reason, ControllerReason::PathChanged);
    }

    #[test]
    fn latency_governor_remains_outer_policy_only() {
        let urgent = LatencyGovernor::operating_point(0, 1_000_000);
        let quality = LatencyGovernor::operating_point(50, 1_000_000);
        assert!(quality.allocation_utilization >= urgent.allocation_utilization);
        assert!(quality.pacer_horizon >= urgent.pacer_horizon);
    }
}
