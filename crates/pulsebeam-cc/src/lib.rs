//! Caller-driven, transport-neutral SCReAMv2 and byte-debt pacing.
//!
//! Times are monotonic durations from a caller-selected origin and must not
//! exceed [`MAX_TIME`]. The host authenticates packet identities, replay and
//! path membership before delivering normalized feedback. Serialize readiness,
//! transport acceptance and `commit_send` as a transaction: readiness does not
//! reserve window or pacing credit, and every accepted departure must be
//! committed exactly once. The host owns packet history, queues and scheduling.

use std::{fmt, time::Duration};

mod congestion;
mod pacer;

#[cfg(test)]
mod network;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_deadlines;
#[cfg(test)]
mod tests_no_cap;
#[cfg(test)]
mod tests_time;

use congestion::ScreamController;
pub use congestion::{
    ControllerInput as Observation, ControllerReason, EcnMark, EcnValidation, FeedbackSample,
    LossOrigin, SafeRtpEnvelope as Envelope,
};
use pacer::Pacer;

/// Largest accepted relative time, equal to `u64::MAX` microseconds.
pub const MAX_TIME: Duration = Duration::from_micros(u64::MAX);
/// Maximum batch size, including transport feedback and ACK-edge gaps.
pub const MAX_FEEDBACK_SAMPLES: usize = 8_448;

/// Aggregate media-payload rate constraint, in bits per second.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Config {
    /// `None` means no configured cap; `Some(0)` is invalid.
    pub maximum_payload_rate: Option<u64>,
}

/// Invalid controller configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    /// A finite rate cap must be positive.
    ZeroMaximumPayloadRate,
}

/// Observation rejected without changing controller or pacer state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateError {
    /// Host time is out of range or precedes the time fence.
    InvalidTime,
    /// More than [`MAX_FEEDBACK_SAMPLES`] were supplied.
    TooManyFeedbackSamples,
    /// An available path must specify its epoch.
    MissingPathEpoch,
    /// Feedback is not accepted in the observation changing path epochs.
    PathChangeWithFeedback,
    /// A sample or feedback hold has invalid time or inconsistent semantics.
    InvalidFeedback,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ConfigError {}
impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for UpdateError {}

/// Non-reserving readiness for one complete transport packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Readiness {
    Ready,
    PathUnavailable,
    CongestionWindow,
    Pacing { deadline: Option<Duration> },
    InvalidTime,
    InvalidPacket,
}

/// Saturating lifetime departure counter and current path flight peak.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Diagnostics {
    pub charged_bytes: u128,
    pub flight_peak: u64,
    pub initial_probe_started: Option<Duration>,
}

/// Sole owner of native controller state and the single byte-debt pacer.
pub struct Controller {
    native: ScreamController,
    pacer: Pacer,
    time_fence: Duration,
    epoch: Option<u64>,
    available: bool,
    desired: u64,
    flight: u64,
    queue_bytes: u64,
    queue_age: Duration,
    ecn: EcnValidation,
    feedback_hold: Duration,
    envelope: Envelope,
}

impl Controller {
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        let cap = match config.maximum_payload_rate {
            Some(0) => return Err(ConfigError::ZeroMaximumPayloadRate),
            Some(cap) => cap,
            None => u64::MAX,
        };
        let native = ScreamController::with_cap(0, None, cap);
        let envelope = native.snapshot(0, 0, 0);
        Ok(Self {
            native,
            pacer: Pacer::default(),
            time_fence: Duration::ZERO,
            epoch: None,
            available: false,
            desired: 0,
            flight: 0,
            queue_bytes: 0,
            queue_age: Duration::ZERO,
            ecn: EcnValidation::default(),
            feedback_hold: Duration::ZERO,
            envelope,
        })
    }

    /// Validate the complete observation before any state change.
    pub fn update(
        &mut self,
        now: Duration,
        input: Observation<'_>,
    ) -> Result<Envelope, UpdateError> {
        if !self.valid_time(now) {
            return Err(UpdateError::InvalidTime);
        }
        if input.feedback.len() > MAX_FEEDBACK_SAMPLES {
            return Err(UpdateError::TooManyFeedbackSamples);
        }
        if input.path_available && input.path_epoch.is_none() {
            return Err(UpdateError::MissingPathEpoch);
        }
        let changed = input.path_epoch.is_some() && input.path_epoch != self.epoch;
        if changed && !input.feedback.is_empty() {
            return Err(UpdateError::PathChangeWithFeedback);
        }
        if input.feedback_hold > MAX_TIME
            || input.feedback.iter().any(|sample| {
                sample.transport_bytes == 0
                    || sample.sent_at > sample.received_at
                    || sample.received_at > now
                    || (sample.received && sample.lost)
                    || (sample.recovered && !sample.received)
                    || (sample.recovered && sample.newly_acked)
                    || (!sample.received
                        && (sample.receiver_arrival_micros.is_some() || sample.ecn.is_some()))
            })
        {
            return Err(UpdateError::InvalidFeedback);
        }
        self.time_fence = now;
        if changed && self.epoch.is_some() {
            self.pacer.reset();
        }
        if input.path_epoch.is_some() {
            self.epoch = input.path_epoch;
        }
        self.available = input.path_available;
        self.desired = input.desired_media_rate;
        self.flight = input.bytes_in_flight;
        self.queue_bytes = input.paced_queue_bytes;
        self.ecn = input.ecn;
        self.feedback_hold = input.feedback_hold;
        self.envelope = self.native.update(now, input);
        self.configure_pacer(now);
        Ok(self.envelope)
    }

    pub fn snapshot(&self) -> Envelope {
        self.envelope
    }

    /// Raise effective pacing for queue drain without changing the nominal rate.
    pub fn update_queue(&mut self, bytes: u64, average_age: Duration) {
        self.queue_bytes = bytes;
        self.queue_age = average_age;
        self.envelope = self.native.snapshot(self.desired, self.flight, bytes);
        self.pacer.update_queue(bytes, average_age);
        self.envelope.pacing_transport_rate = self.pacer.transport_rate();
    }

    /// Test a complete packet against path, window, and pacer without reserving.
    pub fn ready(&self, now: Duration, flight: u64, complete_transport_bytes: usize) -> Readiness {
        if !self.valid_time(now) {
            return Readiness::InvalidTime;
        }
        if complete_transport_bytes == 0 {
            return Readiness::InvalidPacket;
        }
        if !self.available {
            return Readiness::PathUnavailable;
        }
        if u128::from(flight).saturating_add(complete_transport_bytes as u128)
            > u128::from(self.envelope.max_rtp_bytes_in_flight)
        {
            return Readiness::CongestionWindow;
        }
        if !self.pacer.eligible(now) {
            return Readiness::Pacing {
                deadline: self.pacer.next_deadline(),
            };
        }
        Readiness::Ready
    }

    /// Charge an already accepted transport departure exactly once.
    /// Invalid or backwards time is clamped to the last valid time fence.
    pub fn commit_send(
        &mut self,
        now: Duration,
        bytes: usize,
        post_send_flight: u64,
        padding: bool,
    ) {
        let at = if self.accept_time(now) {
            now
        } else {
            self.time_fence
        };
        if padding {
            self.pacer.commit_padding(at, bytes);
        } else {
            self.pacer.commit(at, bytes);
        }
        self.native.note_send(at);
        self.native.observe_bytes_in_flight(post_send_flight);
        self.flight = post_send_flight;
    }

    /// Preserve a host's accepted flight peak before retiring batched receipts.
    pub fn observe_bytes_in_flight(&mut self, bytes: u64) {
        self.native.observe_bytes_in_flight(bytes);
    }

    /// Prepare one padding quantum. The host must still check the actual packet's readiness.
    pub fn prepare_padding(&mut self, now: Duration) -> Option<usize> {
        if !self.valid_time(now)
            || !self.available
            || self.flight >= self.envelope.max_rtp_bytes_in_flight
        {
            return None;
        }
        self.time_fence = now;
        self.pacer.prepare_padding(now)
    }

    /// Unreserved padding request size for host-side complete-packet estimation.
    pub fn padding_request_bytes(&self) -> usize {
        self.pacer.padding_request_bytes()
    }

    pub fn padding_unavailable(&mut self) {
        self.pacer.padding_unavailable();
    }

    /// Advance maintenance without receiver-driven congestion-window growth.
    pub fn process_idle(&mut self, now: Duration) {
        if !self.accept_time(now) {
            return;
        }
        self.pacer.process_idle(now);
        self.envelope = self.native.update(
            now,
            Observation {
                path_epoch: self.epoch,
                path_available: self.available,
                feedback: &[],
                feedback_hold: self.feedback_hold,
                fresh_network_feedback: false,
                bytes_in_flight: self.flight,
                paced_queue_bytes: self.queue_bytes,
                desired_media_rate: self.desired,
                ecn: self.ecn,
            },
        );
        self.configure_pacer(now);
    }

    /// Return the earliest actionable maintenance, pacing or padding deadline.
    /// Preserve the scheduled time of due work so hosts can suppress already-attempted sends.
    pub fn next_deadline(
        &self,
        now: Duration,
        send_candidate: Option<(u64, usize)>,
        padding_candidate: Option<(u64, usize)>,
    ) -> Option<Duration> {
        if !self.valid_time(now)
            || !self.available
            || send_candidate.is_some_and(|(_, bytes)| bytes == 0)
            || padding_candidate.is_some_and(|(_, bytes)| bytes == 0)
        {
            return None;
        }
        let mut deadline = [
            self.native.feedback_deadline(),
            self.native.padding_deadline(),
        ]
        .into_iter()
        .flatten()
        .min();
        let mut include = |candidate: Option<Duration>| {
            if let Some(candidate) = candidate {
                deadline = Some(deadline.map_or(candidate, |old| old.min(candidate)));
            }
        };
        if let Some((flight, bytes)) = send_candidate
            && bytes > 0
            && u128::from(flight).saturating_add(bytes as u128)
                <= u128::from(self.envelope.max_rtp_bytes_in_flight)
        {
            include(self.pacer.next_deadline());
        }
        if let Some((flight, bytes)) = padding_candidate
            && bytes > 0
            && u128::from(flight).saturating_add(bytes as u128)
                <= u128::from(self.envelope.max_rtp_bytes_in_flight)
        {
            include(self.pacer.padding_deadline());
        }
        deadline.filter(|at| *at <= MAX_TIME)
    }

    pub fn diagnostics(&self) -> Diagnostics {
        Diagnostics {
            charged_bytes: self.pacer.charged_bytes(),
            flight_peak: self.native.flight_peak(),
            initial_probe_started: self.native.initial_probe_started(),
        }
    }

    fn valid_time(&self, now: Duration) -> bool {
        now >= self.time_fence && now <= MAX_TIME
    }
    fn accept_time(&mut self, now: Duration) -> bool {
        if !self.valid_time(now) {
            return false;
        }
        self.time_fence = now;
        true
    }
    fn configure_pacer(&mut self, now: Duration) {
        self.pacer.configure(
            now,
            self.envelope.nominal_pacing_transport_rate,
            self.envelope.padding_transport_rate,
            self.envelope.pacing_window,
        );
        self.pacer.update_queue(self.queue_bytes, self.queue_age);
        self.envelope.pacing_transport_rate = self.pacer.transport_rate();
    }
}
