// Copyright 2025 The WebRTC Project Authors. All rights reserved.
// Rust adaptation of scream_network_controller.cc at libwebrtc
// 326fd06bb4d93900ed6947205b94cefd2413afca. See congestion/screamv2/LICENSE and PATENTS.

#![allow(
    clippy::arithmetic_side_effects,
    reason = "bounded native-controller unit arithmetic and diagnostic confidence decay"
)]

use std::time::Duration;

mod screamv2;

pub(crate) use screamv2::{EcnMark, FeedbackSample, LossOrigin};

const TARGET_BITRATE_MAX: u64 = 100_000_000;
// api/transport/network_types.h, PacerConfig::kDefaultTimeInterval at the same pin.
const DEFAULT_PACING_WINDOW: Duration = Duration::from_millis(40);

#[cfg(test)]
#[path = "congestion/scenario.rs"]
mod scenario;

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

/// The lean transport's adapter to the pinned ScreamNetworkController behavior.
/// Demand maps to max_total_allocated_bitrate, not measured bandwidth or a new
/// controller constraint. Source/frame and encoder policy remain caller-owned.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ControllerInput<'a> {
    pub(crate) path_epoch: Option<u64>,
    pub(crate) path_available: bool,
    pub(crate) feedback: &'a [FeedbackSample],
    pub(crate) feedback_hold: Duration,
    pub(crate) fresh_network_feedback: bool,
    pub(crate) bytes_in_flight: u64,
    pub(crate) paced_queue_bytes: u64,
    pub(crate) desired_media_rate: u64,
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
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SafeRtpEnvelope {
    pub(crate) target_media_payload_rate: u64,
    pub(crate) encoder_target_media_payload_rate: u64,
    pub(crate) max_rtp_bytes_in_flight: u64,
    pub(crate) pacing_transport_rate: u64,
    pub(crate) padding_transport_rate: u64,
    pub(crate) pacing_window: Duration,
    /// Native cwnd_reduce_ratio, scaled to 0..=65535. Never a capacity multiplier.
    pub(crate) cwnd_reduce_ratio: u16,
    pub(crate) encoder_paused: bool,
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

pub(crate) struct ScreamController {
    core: screamv2::ScreamV2,
    path_epoch: Option<u64>,
    path_available: bool,
    last_feedback: Option<Duration>,
    first_send: Option<Duration>,
    feedback_stale: bool,
    confidence: u16,
    confidence_decay_started: Option<(Duration, u16)>,
    desired_rate: u64,
    max_seen_allocated_rate: u64,
    initial_probe_started: Option<Duration>,
    padding_interval_end: Option<Duration>,
    padding_allowed: bool,
    padding_deadline: Option<Duration>,
    encoder_paused: bool,
    last_pacing_window: Duration,
    last_reason: ControllerReason,
}

impl ScreamController {
    pub(crate) fn new(desired_media_rate: u64, path_payload_max: Option<u32>) -> Self {
        Self {
            core: screamv2::ScreamV2::new(TARGET_BITRATE_MAX, path_payload_max),
            path_epoch: None,
            path_available: false,
            last_feedback: None,
            first_send: None,
            feedback_stale: false,
            confidence: 0,
            confidence_decay_started: None,
            desired_rate: desired_media_rate,
            max_seen_allocated_rate: desired_media_rate.min(TARGET_BITRATE_MAX),
            initial_probe_started: None,
            padding_interval_end: None,
            padding_allowed: false,
            padding_deadline: None,
            encoder_paused: false,
            last_pacing_window: DEFAULT_PACING_WINDOW,
            last_reason: ControllerReason::Startup,
        }
    }

    pub(crate) fn note_send(&mut self, at: Duration) {
        self.first_send.get_or_insert(at);
    }

    #[cfg(test)]
    pub(crate) fn initial_probe_started(&self) -> Option<Duration> {
        self.initial_probe_started
    }

    #[cfg(test)]
    pub(crate) fn test_flight_peak(&self) -> u64 {
        self.core.test_flight_peak()
    }

    pub(crate) fn observe_bytes_in_flight(&mut self, bytes: u64) {
        self.core.observe_bytes_in_flight(bytes);
    }

    pub(crate) fn update(&mut self, now: Duration, input: ControllerInput<'_>) -> SafeRtpEnvelope {
        self.update_path(input.path_epoch, input.path_available);
        self.desired_rate = input.desired_media_rate;
        self.max_seen_allocated_rate = self
            .max_seen_allocated_rate
            .max(input.desired_media_rate.min(TARGET_BITRATE_MAX));
        if self.path_available && input.desired_media_rate > 0 {
            self.initial_probe_started.get_or_insert(now);
        }
        if input.fresh_network_feedback {
            self.last_feedback = Some(
                input
                    .feedback
                    .iter()
                    .map(|sample| sample.received_at)
                    .max()
                    .unwrap_or(now)
                    .min(now),
            );
        }
        let output = self.core.update(
            now,
            screamv2::ControllerInput {
                feedback: input.feedback,
                feedback_hold: input.feedback_hold,
                growth_eligible: input.fresh_network_feedback,
                bytes_in_flight: input.bytes_in_flight,
                target_bitrate_max: TARGET_BITRATE_MAX,
                ecn_mode: input.ecn.mode(),
            },
        );
        self.feedback_stale = self.feedback_expiry().is_some_and(|expiry| now >= expiry);
        self.update_confidence(
            now,
            input.fresh_network_feedback,
            output.application_limited,
        );
        let pacing_delay = pacing_delay(input.paced_queue_bytes, output.pacing_rate);
        let congested = input.bytes_in_flight > output.max_bytes_in_flight;
        if congested && pacing_delay > screamv2::PARAMETERS.max_pacing_delay_for_pushback {
            self.encoder_paused = true;
        } else if !congested && pacing_delay < screamv2::PARAMETERS.min_pacing_delay_for_pushback {
            self.encoder_paused = false;
        }
        self.last_pacing_window = if elapsed(now, output.last_congestion_reaction)
            > screamv2::PARAMETERS.allow_large_pacing_bursts_after_congestion_time
        {
            DEFAULT_PACING_WINDOW
        } else {
            Duration::from_millis(10)
        };
        self.update_padding(now, output);
        self.last_reason = if self.feedback_stale {
            ControllerReason::FeedbackStale
        } else if output.application_limited {
            ControllerReason::ApplicationLimited
        } else {
            output.reason.into()
        };
        self.envelope(output, input.desired_media_rate, input.paced_queue_bytes)
    }

    pub(crate) fn snapshot(
        &self,
        desired: u64,
        _bytes_in_flight: u64,
        paced_queue_bytes: u64,
    ) -> SafeRtpEnvelope {
        self.envelope(
            self.core.snapshot(TARGET_BITRATE_MAX),
            desired,
            paced_queue_bytes,
        )
    }

    fn update_path(&mut self, epoch: Option<u64>, available: bool) {
        if let Some(epoch) = epoch {
            if self.path_epoch.is_some_and(|previous| previous != epoch) {
                self.core.reset_path(TARGET_BITRATE_MAX);
                self.last_feedback = None;
                self.first_send = None;
                self.feedback_stale = false;
                self.confidence = 0;
                self.confidence_decay_started = None;
                self.encoder_paused = false;
                self.padding_interval_end = None;
            }
            self.path_epoch = Some(epoch);
        }
        self.path_available = available;
    }

    fn update_padding(&mut self, now: Duration, output: screamv2::Output) {
        let params = screamv2::PARAMETERS;
        self.padding_allowed = false;
        self.padding_deadline = None;
        if !self.path_available {
            return;
        }
        if elapsed(now, output.last_window_decrease)
            > params.allow_padding_after_last_congestion_time
        {
            if self.padding_interval_end.is_some_and(|end| now < end) {
                self.padding_allowed = true;
            } else if self
                .padding_interval_end
                .is_none_or(|end| now.saturating_sub(end) >= params.time_between_periodic_padding)
            {
                self.padding_interval_end = now.checked_add(params.periodic_padding_duration);
                self.padding_allowed = true;
            }
        } else if self.padding_interval_end.is_some_and(|end| now < end) {
            self.padding_interval_end = Some(now);
        }
        if self.desired_rate > 0 {
            self.padding_deadline = if self.padding_allowed {
                self.padding_interval_end
            } else {
                let interval_due = self
                    .padding_interval_end
                    .and_then(|end| end.checked_add(params.time_between_periodic_padding))
                    .unwrap_or(now);
                // The native cooldown is strict. A past interval boundary is
                // not runnable work while a later congestion cooldown blocks it.
                let cooldown_due = output
                    .last_window_decrease
                    .and_then(|last| {
                        last.checked_add(params.allow_padding_after_last_congestion_time)
                    })
                    .and_then(|due| due.checked_add(Duration::from_micros(1)))
                    .unwrap_or(now);
                Some(interval_due.max(cooldown_due))
            };
        }
    }

    fn padding_cap(&self, desired: u64) -> u64 {
        TARGET_BITRATE_MAX
            .min(self.max_seen_allocated_rate)
            .min(desired.saturating_mul(2))
    }

    pub(crate) fn next_deadline(&self) -> Option<Duration> {
        let feedback = (!self.feedback_stale)
            .then(|| self.feedback_expiry())
            .flatten();
        [feedback, self.padding_deadline]
            .into_iter()
            .flatten()
            .min()
    }

    fn feedback_expiry(&self) -> Option<Duration> {
        if !self.path_available || self.first_send.is_none() {
            return None;
        }
        let threshold = self
            .core
            .snapshot(TARGET_BITRATE_MAX)
            .smoothed_rtt
            .saturating_mul(3)
            .clamp(Duration::from_millis(500), Duration::from_secs(2));
        self.last_feedback
            .or(self.first_send)?
            .checked_add(threshold)
    }

    fn update_confidence(&mut self, now: Duration, fresh: bool, application_limited: bool) {
        if self.feedback_stale || application_limited {
            let (started, base) = *self
                .confidence_decay_started
                .get_or_insert((now, self.confidence));
            self.confidence = half_life(base, now.saturating_sub(started));
        } else {
            self.confidence_decay_started = None;
            if fresh {
                self.confidence = self
                    .confidence
                    .saturating_add((u16::MAX - self.confidence) / 8 + 1);
            }
        }
    }

    fn envelope(
        &self,
        output: screamv2::Output,
        desired: u64,
        paced_queue_bytes: u64,
    ) -> SafeRtpEnvelope {
        let padding_rate = if self.path_available
            && desired > 0
            && self.padding_allowed
            && output.target_bitrate < self.padding_cap(desired)
        {
            output.target_bitrate
        } else {
            0
        };
        SafeRtpEnvelope {
            // The lean API reports SCReAM's nominal target. Upstream's encoder
            // target (including pause-to-zero) and pushback are separate sender
            // instructions, not a new bandwidth estimate or SFU frame policy.
            target_media_payload_rate: output.target_bitrate,
            encoder_target_media_payload_rate: if self.encoder_paused {
                0
            } else {
                output.target_bitrate
            },
            max_rtp_bytes_in_flight: output.max_bytes_in_flight,
            pacing_transport_rate: output.pacing_rate,
            padding_transport_rate: padding_rate,
            pacing_window: self.last_pacing_window,
            cwnd_reduce_ratio: pushback_ratio(pacing_delay(paced_queue_bytes, output.pacing_rate)),
            encoder_paused: self.encoder_paused,
            reference_window: output.reference_window,
            native_queue_delay_target: output.queue_delay_target,
            effective_queue_delay_target: output.queue_delay_target,
            queue_delay: output.queue_delay,
            queue_delay_confidence: self.confidence,
            smoothed_rtt: output.smoothed_rtt,
            feedback_hold: output.feedback_hold,
            delivered_rtp_transport_rate: output.delivered_rate,
            application_limited: output.application_limited,
            feedback_stale: self.feedback_stale,
            policer_detected: output.policer_detected,
            l4s_enabled: output.l4s_enabled,
            probe_permitted: padding_rate > 0,
            reason: self.last_reason,
        }
    }
}

fn elapsed(now: Duration, previous: Option<Duration>) -> Duration {
    previous.map_or(Duration::MAX, |previous| now.saturating_sub(previous))
}

fn pacing_delay(bytes: u64, bps: u64) -> Duration {
    if bps == 0 {
        return Duration::ZERO;
    }
    Duration::from_micros(
        u64::try_from(u128::from(bytes).saturating_mul(8_000_000) / u128::from(bps))
            .unwrap_or(u64::MAX),
    )
}

fn pushback_ratio(delay: Duration) -> u16 {
    let params = screamv2::PARAMETERS;
    let range = params
        .max_pacing_delay_for_pushback
        .saturating_sub(params.min_pacing_delay_for_pushback);
    if range.is_zero() {
        return 0;
    }
    let excess = delay
        .saturating_sub(params.min_pacing_delay_for_pushback)
        .min(range);
    u16::try_from(
        excess.as_micros().saturating_mul(u128::from(u16::MAX)) / range.as_micros().max(1),
    )
    .unwrap_or(u16::MAX)
}

fn half_life(base: u16, elapsed: Duration) -> u16 {
    let half_life = 5_000_000_u64;
    let elapsed = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
    let halves = elapsed / half_life;
    let remainder = elapsed % half_life;
    let shifted = u64::from(base) >> halves.min(15);
    let value = shifted.saturating_mul(half_life.saturating_mul(2).saturating_sub(remainder))
        / half_life.saturating_mul(2);
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ControllerInput<'static> {
        ControllerInput {
            path_epoch: Some(1),
            path_available: true,
            feedback: &[],
            feedback_hold: Duration::ZERO,
            fresh_network_feedback: false,
            bytes_in_flight: 0,
            paced_queue_bytes: 0,
            desired_media_rate: 4_000_000,
            ecn: EcnValidation::default(),
        }
    }

    #[test]
    fn hints_are_allocation_caps_not_capacity_and_polling_does_not_invent_alr() {
        let mut controller = ScreamController::new(0, None);
        let output = controller.update(Duration::ZERO, input());
        assert_eq!(output.target_media_payload_rate, 300_000);
        assert!(!output.application_limited);
        let quiet = controller.update(Duration::from_millis(250), input());
        assert!(!quiet.application_limited);
        let cleared = controller.update(
            Duration::from_secs(1),
            ControllerInput {
                desired_media_rate: 0,
                ..input()
            },
        );
        assert_eq!(
            cleared.target_media_payload_rate,
            output.target_media_payload_rate
        );
        assert_eq!(cleared.padding_transport_rate, 0);
    }

    #[test]
    fn upstream_padding_has_three_second_intervals_and_three_second_gaps() {
        let mut controller = ScreamController::new(4_000_000, None);
        for (millis, padding) in [
            (0, true),
            (2_999, true),
            (3_000, false),
            (5_999, false),
            (6_000, true),
            (8_999, true),
            (9_000, false),
        ] {
            let output = controller.update(Duration::from_millis(millis), input());
            assert_eq!(output.padding_transport_rate > 0, padding, "at {millis}");
        }
    }

    #[test]
    fn congestion_after_an_expired_interval_schedules_future_work_not_a_spin() {
        let mut controller = ScreamController::new(4_000_000, None);
        controller.update(Duration::ZERO, input());
        let now = Duration::from_secs(10);
        let mut native = controller.core.snapshot(TARGET_BITRATE_MAX);
        native.last_window_decrease = Some(now);
        for end in [Some(Duration::from_secs(3)), None] {
            controller.padding_interval_end = end;
            controller.update_padding(now, native);
            assert!(!controller.padding_allowed);
            let due = controller.padding_deadline.unwrap();
            assert_eq!(due, Duration::from_secs(11) + Duration::from_micros(1));
            controller.update_padding(due, native);
            assert!(controller.padding_allowed);
            assert!(controller.padding_deadline.unwrap() > due);
        }
    }

    #[test]
    fn hint_and_real_route_changes_cannot_rearm_initial_discovery() {
        let mut controller = ScreamController::new(4_000_000, None);
        controller.update(Duration::ZERO, input());
        controller.update(
            Duration::from_secs(1),
            ControllerInput {
                desired_media_rate: 0,
                ..input()
            },
        );
        controller.update(Duration::from_secs(2), input());
        controller.update(
            Duration::from_secs(4),
            ControllerInput {
                path_epoch: Some(2),
                ..input()
            },
        );
        assert_eq!(controller.initial_probe_started, Some(Duration::ZERO));
        assert!(
            elapsed(Duration::from_secs(6), controller.initial_probe_started)
                >= screamv2::PARAMETERS.initial_probing_duration
        );
    }

    #[test]
    fn native_pushback_is_not_a_bandwidth_multiplier_and_pause_has_hysteresis() {
        let mut controller = ScreamController::new(4_000_000, None);
        let output = controller.update(
            Duration::ZERO,
            ControllerInput {
                paced_queue_bytes: 12_375,
                ..input()
            },
        );
        assert_eq!(output.target_media_payload_rate, 300_000);
        assert_eq!(output.cwnd_reduce_ratio, u16::MAX / 2);
        assert!(!output.encoder_paused);
        let paused = controller.update(
            Duration::from_millis(1),
            ControllerInput {
                paced_queue_bytes: 30_000,
                bytes_in_flight: output.max_rtp_bytes_in_flight + 1,
                ..input()
            },
        );
        assert!(paused.encoder_paused);
        assert_eq!(paused.target_media_payload_rate, 300_000);
        assert_eq!(paused.encoder_target_media_payload_rate, 0);
        let held = controller.update(
            Duration::from_millis(2),
            ControllerInput {
                paced_queue_bytes: 12_375,
                ..input()
            },
        );
        assert!(held.encoder_paused);
        let resumed = controller.update(Duration::from_millis(3), input());
        assert!(!resumed.encoder_paused);
        assert_eq!(resumed.target_media_payload_rate, 300_000);
        assert_eq!(resumed.encoder_target_media_payload_rate, 300_000);
    }

    #[test]
    fn stale_feedback_is_diagnostic_and_cannot_replay_native_growth() {
        let mut controller = ScreamController::new(4_000_000, None);
        controller.update(Duration::ZERO, input());
        controller.note_send(Duration::ZERO);
        let output = controller.update(Duration::from_millis(500), input());
        assert!(output.feedback_stale);
        assert_eq!(output.target_media_payload_rate, 300_000);
        assert_eq!(output.reference_window, 1_000);
        assert_eq!(output.padding_transport_rate, 300_000);
        let stale = [FeedbackSample {
            sent_at: Duration::ZERO,
            received_at: Duration::from_millis(500),
            transport_bytes: 1_000,
            received: true,
            newly_acked: true,
            lost: false,
            recovered: false,
            receiver_arrival_micros: Some(50_000),
            ecn: None,
        }];
        let unchanged = controller.update(
            Duration::from_millis(600),
            ControllerInput {
                feedback: &stale,
                ..input()
            },
        );
        assert_eq!(unchanged.reference_window, output.reference_window);
        assert_eq!(
            unchanged.target_media_payload_rate,
            output.target_media_payload_rate
        );
    }

    #[test]
    fn network_unavailability_without_a_new_route_does_not_reset_the_core() {
        let mut controller = ScreamController::new(4_000_000, None);
        controller.update(Duration::ZERO, input());
        controller.observe_bytes_in_flight(12_000);
        controller.update(
            Duration::from_millis(10),
            ControllerInput {
                path_epoch: None,
                path_available: false,
                ..input()
            },
        );
        controller.update(Duration::from_millis(20), input());
        assert_eq!(controller.test_flight_peak(), 12_000);
        controller.update(
            Duration::from_millis(30),
            ControllerInput {
                path_epoch: Some(2),
                ..input()
            },
        );
        assert_eq!(controller.test_flight_peak(), 0);
    }
}
