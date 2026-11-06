#![allow(
    dead_code,
    reason = "private feedback projection is consumed by the connection controller"
)]
#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_wrap,
    reason = "finite wire counter and receiver-time conversion is checked at the RTC boundary"
)]

use crate::{
    PacketFeedbackKind,
    congestion::{EcnMark as ControllerEcnMark, FeedbackSample, LossOrigin},
    connection::TransmitCommitContext,
    rtcp::{ArrivalOffset, FeedbackBatch, FeedbackReport, Rfc8888Status, TwccStatus},
    transport::{DatagramKind, PathEpoch, RtpService},
};
pub(crate) use pulsebeam_feedback::{Ecn as EcnMark, HistoryCounters, ReceiverTime, SentPacketId};
use pulsebeam_feedback::{
    Epoch, FeedbackMode, PacketId, Sender, SenderConfig, SenderError, SpaceId,
};
use std::time::{Duration, Instant};

pub(crate) const SENT_HISTORY_CAPACITY: usize = 32_768;
pub(crate) const MAX_EXPIRATIONS_PER_POLL: usize = 256;
fn config() -> SenderConfig {
    SenderConfig {
        capacity: SENT_HISTORY_CAPACITY,
        retention: Duration::from_secs(5),
        max_expirations_per_poll: MAX_EXPIRATIONS_PER_POLL,
        max_ack_reordering: 4_096,
        max_feedback_statuses: 8_192,
        initial_reordering_window: Duration::from_millis(30),
        max_reordering_window: Duration::from_millis(500),
    }
}
fn mode(profile: PacketFeedbackKind) -> FeedbackMode {
    match profile {
        PacketFeedbackKind::TransportWide => FeedbackMode::Continuous,
        PacketFeedbackKind::Rfc8888 => FeedbackMode::Independent,
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HistoryError {
    Exhausted,
    InvalidCommit,
}
impl From<SenderError> for HistoryError {
    fn from(error: SenderError) -> Self {
        match error {
            SenderError::Exhausted => Self::Exhausted,
            SenderError::InvalidCommit => Self::InvalidCommit,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PacketFeedback {
    pub(crate) sent_id: SentPacketId,
    pub(crate) ssrc: u32,
    pub(crate) committed_at: Instant,
    pub(crate) transport_bytes: u32,
    pub(crate) service: RtpService,
    pub(crate) received: bool,
    pub(crate) newly_acked: bool,
    pub(crate) lost: bool,
    pub(crate) reported_lost_for_the_first_time: bool,
    pub(crate) recovered: bool,
    pub(crate) observed_at: Option<Instant>,
    pub(crate) loss_origin: Option<LossOrigin>,
    pub(crate) receiver_arrival: Option<ReceiverTime>,
    pub(crate) ecn: Option<EcnMark>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FeedbackTiming {
    pub(crate) received_at: Instant,
    pub(crate) feedback_hold: Option<Duration>,
    pub(crate) newest_send_age: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PathChange {
    pub(crate) epoch: PathEpoch,
    pub(crate) available: bool,
}

pub(crate) struct ControllerInputs<'a> {
    pub(crate) feedback: FeedbackView<'a>,
    pub(crate) synthetic: FeedbackView<'a>,
    pub(crate) exhausted: bool,
    pub(crate) timing: Option<FeedbackTiming>,
    pub(crate) fresh_network_feedback: bool,
    pub(crate) path_change: Option<PathChange>,
    pub(crate) application_limited: bool,
    pub(crate) bytes_in_flight: u64,
}

pub(crate) struct SentHistory {
    sender: Sender,
    origin: Instant,
}
pub(crate) struct PreparedFeedbackProfile<'a>(pulsebeam_feedback::PreparedProfile<'a>);
impl PreparedFeedbackProfile<'_> {
    pub(crate) fn commit(self) {
        self.0.commit();
    }
}
impl SentHistory {
    #[cfg(test)]
    pub(crate) fn new(profile: impl Into<Option<PacketFeedbackKind>>, origin: Instant) -> Self {
        Self::with_limit(
            profile,
            crate::ConnectionLimits::DEFAULT_MAX_SENT_HISTORY_BYTES,
            origin,
        )
        .expect("default sent-history backing allowance")
    }
    pub(crate) fn with_limit(
        profile: impl Into<Option<PacketFeedbackKind>>,
        limit: usize,
        origin: Instant,
    ) -> Result<Self, HistoryError> {
        Ok(Self {
            sender: Sender::new(config(), profile.into().map(mode), limit)?,
            origin,
        })
    }
    pub(crate) fn prepare_feedback_profile(
        &mut self,
        profile: PacketFeedbackKind,
    ) -> Result<PreparedFeedbackProfile<'_>, HistoryError> {
        Ok(PreparedFeedbackProfile(
            self.sender.prepare_feedback_profile(mode(profile))?,
        ))
    }
    fn normalize_commit(
        &self,
        context: TransmitCommitContext,
    ) -> Result<Option<pulsebeam_feedback::SendCommit>, HistoryError> {
        if context.kind != DatagramKind::Rtp {
            return Ok(None);
        }
        let (Some(epoch), Some(identity)) = (context.path_epoch, context.rtp) else {
            return Err(HistoryError::InvalidCommit);
        };
        let feedback_sequence = identity.twcc_sequence.map(|sequence| {
            PacketId(self.sender.newest_feedback_packet().map_or_else(
                || u64::from(sequence),
                |newest| unwrap_forward(sequence, newest.0),
            ))
        });
        Ok(Some(pulsebeam_feedback::SendCommit {
            at: context.at.saturating_duration_since(self.origin),
            wire_len: context.wire_len,
            path_epoch: Epoch(epoch.value()),
            identity: pulsebeam_feedback::SendIdentity {
                space: SpaceId(u64::from(identity.ssrc)),
                packet: PacketId(identity.index),
                feedback_sequence,
                tag: match identity.service {
                    RtpService::Original => 0,
                    RtpService::Repair => 1,
                    RtpService::Padding => 2,
                },
            },
        }))
    }
    pub(crate) fn prepare_commit(
        &mut self,
        context: TransmitCommitContext,
    ) -> Result<(), HistoryError> {
        if let Some(context) = self.normalize_commit(context)? {
            self.sender.prepare_commit(context)?;
        }
        Ok(())
    }
    pub(crate) fn preflight_commit(
        &self,
        context: TransmitCommitContext,
    ) -> Result<(), HistoryError> {
        if let Some(context) = self.normalize_commit(context)? {
            self.sender.preflight_commit(context)?;
        }
        Ok(())
    }
    pub(crate) fn commit_preflighted(
        &mut self,
        context: TransmitCommitContext,
    ) -> Result<(), HistoryError> {
        if let Some(context) = self.normalize_commit(context)? {
            self.sender.commit_preflighted(context)?;
        }
        Ok(())
    }
    #[cfg(test)]
    fn commit(&mut self, context: TransmitCommitContext) -> Result<(), HistoryError> {
        self.prepare_commit(context)?;
        self.commit_preflighted(context)
    }
    pub(crate) fn path_changed(&mut self, epoch: PathEpoch, available: bool) {
        self.sender
            .set_availability(Epoch(epoch.value()), available);
    }
    pub(crate) fn retire_source(&mut self, ssrc: u32) {
        self.sender.retire_source(SpaceId(u64::from(ssrc)));
    }
    pub(crate) fn expire(&mut self, now: Instant, limit: usize) -> usize {
        self.sender
            .expire(now.saturating_duration_since(self.origin), limit)
    }
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.origin.checked_add(
            self.sender
                .next_deadline(now.saturating_duration_since(self.origin))?,
        )
    }
    pub(crate) fn set_application_limited(&mut self, value: bool) {
        self.sender.set_application_limited(value);
    }
    pub(crate) fn clear_controller_inputs(&mut self) {
        self.sender.clear_observations();
    }
    pub(crate) fn counters(&self) -> HistoryCounters {
        self.sender.counters()
    }
    #[cfg(test)]
    pub(crate) fn test_active_epoch(&self) -> Option<PathEpoch> {
        self.sender
            .active_epoch()
            .map(|epoch| PathEpoch::from_value(epoch.0))
    }
    #[cfg(test)]
    pub(crate) fn assert_storage(&self) -> usize {
        self.sender.storage_bytes().expect("bounded sender backing")
    }
    pub(crate) fn controller_inputs(&self) -> ControllerInputs<'_> {
        let inputs = self.sender.observations();
        ControllerInputs {
            feedback: FeedbackView {
                entries: &inputs.feedback,
                origin: self.origin,
            },
            synthetic: FeedbackView {
                entries: &inputs.synthetic,
                origin: self.origin,
            },
            exhausted: inputs.exhausted,
            timing: inputs.timing.map(|timing| FeedbackTiming {
                received_at: self
                    .origin
                    .checked_add(timing.received_at)
                    .unwrap_or(self.origin),
                feedback_hold: timing.feedback_hold,
                newest_send_age: timing.newest_send_age,
            }),
            fresh_network_feedback: inputs.fresh_network_feedback,
            path_change: inputs.path_change.map(|change| PathChange {
                epoch: PathEpoch::from_value(change.epoch.0),
                available: change.available,
            }),
            application_limited: inputs.application_limited,
            bytes_in_flight: inputs.bytes_in_flight,
        }
    }
}

pub(crate) struct FeedbackView<'a> {
    entries: &'a [pulsebeam_feedback::SentObservation],
    origin: Instant,
}
impl FeedbackView<'_> {
    pub(crate) fn iter(&self) -> impl Iterator<Item = PacketFeedback> + '_ {
        self.entries.iter().map(|entry| PacketFeedback {
            sent_id: entry.sent_id,
            ssrc: u32::try_from(entry.space.0).unwrap_or_default(),
            committed_at: self
                .origin
                .checked_add(entry.committed_at)
                .unwrap_or(self.origin),
            transport_bytes: entry.transport_bytes,
            service: match entry.tag {
                0 => RtpService::Original,
                1 => RtpService::Repair,
                _ => RtpService::Padding,
            },
            received: entry.received,
            newly_acked: entry.newly_acked,
            lost: entry.lost,
            reported_lost_for_the_first_time: entry.reported_lost_for_the_first_time,
            recovered: entry.recovered,
            observed_at: entry.observed_at.and_then(|at| self.origin.checked_add(at)),
            loss_origin: entry.loss_origin.map(|origin| match origin {
                pulsebeam_feedback::LossOrigin::Reordering => LossOrigin::Reordering,
                pulsebeam_feedback::LossOrigin::HistoryExpiry => LossOrigin::HistoryExpiry,
            }),
            receiver_arrival: entry.receiver_arrival,
            ecn: entry.ecn,
        })
    }
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl PacketFeedback {
    pub(crate) fn controller_sample(self, received_at: Instant, origin: Instant) -> FeedbackSample {
        FeedbackSample {
            sent_at: self.committed_at.saturating_duration_since(origin),
            received_at: self
                .observed_at
                .unwrap_or(received_at)
                .saturating_duration_since(origin),
            transport_bytes: self.transport_bytes,
            received: self.received,
            newly_acked: self.newly_acked,
            lost: self.reported_lost_for_the_first_time,
            recovered: self.recovered,
            receiver_arrival_micros: match self.receiver_arrival {
                Some(ReceiverTime::Micros(value)) => Some(value),
                Some(ReceiverTime::OverRange | ReceiverTime::Unavailable) | None => None,
            },
            ecn: self.ecn.map(|mark| match mark {
                EcnMark::NotEct => ControllerEcnMark::NotEct,
                EcnMark::Ect1 => ControllerEcnMark::Ect1,
                EcnMark::Ect0 => ControllerEcnMark::Ect0,
                EcnMark::Ce => ControllerEcnMark::Ce,
            }),
        }
    }
}

fn unwrap_forward(value: u16, previous: u64) -> u64 {
    let mut candidate = (previous & !0xffff) | u64::from(value);
    if candidate <= previous {
        candidate = candidate.saturating_add(1 << 16);
    }
    candidate
}

fn unwrap_near(value: u64, reference: u64, modulus: u64) -> Option<u64> {
    let base = reference / modulus * modulus;
    let mut candidate = i128::from(base) + i128::from(value);
    let reference = i128::from(reference);
    let modulus = i128::from(modulus);
    let half = modulus / 2;
    if candidate + half < reference {
        candidate += modulus;
    } else if candidate > reference + half {
        candidate -= modulus;
    }
    u64::try_from(candidate).ok()
}

fn unwrap_near_signed(value: i64, reference: Option<i64>, modulus: i64) -> i64 {
    let Some(reference) = reference else {
        return value;
    };
    let base = reference.div_euclid(modulus).saturating_mul(modulus);
    let candidate = base.saturating_add(value);
    let half = modulus / 2;
    if candidate.saturating_add(half) < reference {
        candidate.saturating_add(modulus)
    } else if candidate > reference.saturating_add(half) {
        candidate.saturating_sub(modulus)
    } else {
        candidate
    }
}

fn ecn_mark(value: u8) -> EcnMark {
    match value {
        0 => EcnMark::NotEct,
        1 => EcnMark::Ect1,
        2 => EcnMark::Ect0,
        _ => EcnMark::Ce,
    }
}

mod feedback;
#[cfg(test)]
mod tests;
