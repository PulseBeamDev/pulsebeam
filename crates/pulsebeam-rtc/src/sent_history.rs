#![allow(
    dead_code,
    reason = "private normalized feedback state is consumed by the connection controller"
)]
#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_wrap,
    clippy::indexing_slicing,
    reason = "bounded ring indexes and wrapping RTP sequence spaces are checked locally"
)]

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::{
    PacketFeedbackKind,
    congestion::{EcnMark as ControllerEcnMark, FeedbackSample},
    connection::{CommitParticipant, TransmitCommitContext},
    rtcp::{ArrivalOffset, FeedbackBatch, FeedbackReport, Rfc8888Status, TwccStatus},
    transport::{DatagramKind, PathEpoch, RtpService},
};

pub(crate) const SENT_HISTORY_CAPACITY: usize = 32_768;
const SENT_HISTORY_MAX_AGE: Duration = Duration::from_secs(5);
pub(crate) const MAX_EXPIRATIONS_PER_POLL: usize = 256;
const MAX_ACK_REORDERING: u64 = 4_096;
const MAX_FEEDBACK_STATUSES: usize = 8_192;
const INITIAL_REORDERING_WINDOW: Duration = Duration::from_millis(30);
const MAX_REORDERING_WINDOW: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct SentPacketId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReceiverTime {
    Micros(i64),
    OverRange,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EcnMark {
    NotEct,
    Ect1,
    Ect0,
    Ce,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PacketFeedback {
    pub(crate) sent_id: SentPacketId,
    pub(crate) committed_at: Instant,
    pub(crate) transport_bytes: u32,
    pub(crate) service: RtpService,
    pub(crate) received: bool,
    pub(crate) newly_acked: bool,
    pub(crate) lost: bool,
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

#[derive(Debug, Default)]
pub(crate) struct ControllerInputs {
    pub(crate) feedback: Vec<PacketFeedback>,
    pub(crate) timing: Option<FeedbackTiming>,
    pub(crate) path_change: Option<PathChange>,
    pub(crate) application_limited: bool,
    pub(crate) bytes_in_flight: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HistoryCounters {
    pub(crate) unknown_feedback: u64,
    pub(crate) duplicate_feedback: u64,
    pub(crate) stale_feedback: u64,
    pub(crate) reordered_feedback: u64,
    pub(crate) wrong_path_feedback: u64,
    pub(crate) expired: u64,
    pub(crate) received: u64,
    pub(crate) not_received: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Acknowledgment {
    Pending,
    Missing { since: Instant },
    Received,
    Lost,
    Retired,
}

impl Acknowledgment {
    const fn is_terminal(self) -> bool {
        matches!(self, Self::Received | Self::Lost | Self::Retired)
    }
}

#[derive(Clone, Copy, Debug)]
struct SentEntry {
    id: SentPacketId,
    committed_at: Instant,
    wire_len: usize,
    path_epoch: PathEpoch,
    ssrc: u32,
    rtp_sequence: u64,
    twcc_sequence: Option<u64>,
    service: RtpService,
    acknowledgment: Acknowledgment,
    in_flight: bool,
}

#[derive(Clone, Copy, Debug)]
struct TwccIndex {
    sequence: u64,
    sent_id: SentPacketId,
}

#[derive(Debug)]
struct SsrcHistory {
    ssrc: u32,
    first_sequence: u64,
    newest_sequence: u64,
    highest_acked_sequence: Option<u64>,
    sent_ids: VecDeque<SentPacketId>,
    last_report_timestamp: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HistoryError {
    Exhausted,
    InvalidCommit,
}

pub(crate) struct SentHistory {
    mode: PacketFeedbackKind,
    entries: Box<[Option<SentEntry>]>,
    twcc_index: Box<[Option<TwccIndex>]>,
    ssrcs: Vec<SsrcHistory>,
    next_sent_id: u64,
    oldest_sent_id: u64,
    oldest_twcc: Option<u64>,
    newest_twcc: Option<u64>,
    highest_twcc_acked: Option<u64>,
    twcc_reference: Option<i64>,
    twcc_feedback_count: Option<i64>,
    active_epoch: Option<PathEpoch>,
    inputs: ControllerInputs,
    counters: HistoryCounters,
    bytes_in_flight: u64,
    reordering_window: Duration,
    missing: VecDeque<SentPacketId>,
}

impl SentHistory {
    pub(crate) fn new(mode: PacketFeedbackKind) -> Self {
        Self {
            mode,
            entries: std::iter::repeat_with(|| None)
                .take(SENT_HISTORY_CAPACITY)
                .collect(),
            twcc_index: std::iter::repeat_with(|| None)
                .take(SENT_HISTORY_CAPACITY)
                .collect(),
            ssrcs: Vec::new(),
            next_sent_id: 0,
            oldest_sent_id: 0,
            oldest_twcc: None,
            newest_twcc: None,
            highest_twcc_acked: None,
            twcc_reference: None,
            twcc_feedback_count: None,
            active_epoch: None,
            inputs: ControllerInputs {
                feedback: Vec::with_capacity(MAX_FEEDBACK_STATUSES),
                ..ControllerInputs::default()
            },
            counters: HistoryCounters::default(),
            bytes_in_flight: 0,
            reordering_window: INITIAL_REORDERING_WINDOW,
            missing: VecDeque::new(),
        }
    }

    fn commit_context(&mut self, context: TransmitCommitContext) -> Result<(), HistoryError> {
        self.preflight_commit(context)?;
        if context.kind != DatagramKind::Rtp {
            return Ok(());
        }
        let (Some(epoch), Some(rtp)) = (context.path_epoch, context.rtp) else {
            return Err(HistoryError::InvalidCommit);
        };
        let sent_id = SentPacketId(self.next_sent_id);
        let slot = ring_index(sent_id.0);
        if let Some(previous) = self.entries[slot] {
            self.remove_indexes(previous);
        }
        let ssrc_index = self.ssrc_index_or_insert(rtp.ssrc, rtp.sequence);
        let empty_history = self.ssrcs[ssrc_index].sent_ids.is_empty();
        let rtp_sequence = if empty_history {
            u64::from(rtp.sequence)
        } else {
            unwrap_forward(rtp.sequence, self.ssrcs[ssrc_index].newest_sequence)
        };
        if empty_history {
            self.ssrcs[ssrc_index].first_sequence = rtp_sequence;
        }
        self.ssrcs[ssrc_index].newest_sequence = rtp_sequence;
        let twcc_sequence = rtp.twcc_sequence.map(|sequence| {
            let unwrapped = self.newest_twcc.map_or_else(
                || u64::from(sequence),
                |newest| unwrap_forward(sequence, newest),
            );
            self.oldest_twcc.get_or_insert(unwrapped);
            self.newest_twcc = Some(unwrapped);
            self.twcc_index[ring_index(unwrapped)] = Some(TwccIndex {
                sequence: unwrapped,
                sent_id,
            });
            unwrapped
        });
        self.ssrcs[ssrc_index].sent_ids.push_back(sent_id);
        self.entries[slot] = Some(SentEntry {
            id: sent_id,
            committed_at: context.at,
            wire_len: context.wire_len,
            path_epoch: epoch,
            ssrc: rtp.ssrc,
            rtp_sequence,
            twcc_sequence,
            service: rtp.service,
            acknowledgment: Acknowledgment::Pending,
            in_flight: true,
        });
        self.next_sent_id = self
            .next_sent_id
            .checked_add(1)
            .ok_or(HistoryError::Exhausted)?;
        self.bytes_in_flight = self
            .bytes_in_flight
            .checked_add(context.wire_len as u64)
            .ok_or(HistoryError::Exhausted)?;
        self.inputs.bytes_in_flight = self.bytes_in_flight;
        Ok(())
    }

    #[cfg(test)]
    fn commit(&mut self, context: TransmitCommitContext) -> Result<(), HistoryError> {
        self.commit_context(context)
    }

    pub(crate) fn preflight_commit(
        &self,
        context: TransmitCommitContext,
    ) -> Result<(), HistoryError> {
        if context.kind != DatagramKind::Rtp {
            return Ok(());
        }
        let (Some(epoch), Some(rtp)) = (context.path_epoch, context.rtp) else {
            return Err(HistoryError::InvalidCommit);
        };
        if self.active_epoch != Some(epoch)
            || context.wire_len == 0
            || !matches!(
                (self.mode, rtp.twcc_sequence),
                (PacketFeedbackKind::TransportWide, Some(_)) | (PacketFeedbackKind::Rfc8888, None)
            )
            || self.next_sent_id == u64::MAX
            || self
                .bytes_in_flight
                .checked_add(context.wire_len as u64)
                .is_none()
        {
            return Err(HistoryError::InvalidCommit);
        }
        let slot = ring_index(self.next_sent_id);
        if let Some(previous) = self.entries[slot]
            && !previous.acknowledgment.is_terminal()
            && previous.path_epoch == epoch
        {
            return Err(HistoryError::Exhausted);
        }
        Ok(())
    }

    pub(crate) fn path_changed(&mut self, epoch: PathEpoch, available: bool) {
        if self.active_epoch == Some(epoch) && self.active_epoch.is_some() == available {
            return;
        }
        self.active_epoch = available.then_some(epoch);
        for entry in self.entries.iter_mut().flatten() {
            if !entry.acknowledgment.is_terminal() && self.active_epoch != Some(entry.path_epoch) {
                entry.acknowledgment = Acknowledgment::Retired;
                if entry.in_flight {
                    entry.in_flight = false;
                    self.bytes_in_flight =
                        self.bytes_in_flight.saturating_sub(entry.wire_len as u64);
                }
            }
        }
        self.inputs.bytes_in_flight = self.bytes_in_flight;
        // Controller evidence is path scoped. Never let feedback emitted before a
        // selected-path replacement reach the controller on the new epoch.
        self.inputs.feedback.clear();
        self.inputs.timing = None;
        self.inputs.path_change = Some(PathChange { epoch, available });
        self.twcc_reference = None;
        self.twcc_feedback_count = None;
        self.highest_twcc_acked = None;
        self.oldest_twcc = None;
        self.newest_twcc = None;
        self.missing.clear();
        self.reordering_window = INITIAL_REORDERING_WINDOW;
        for ssrc in &mut self.ssrcs {
            ssrc.last_report_timestamp = None;
            ssrc.highest_acked_sequence = None;
            ssrc.sent_ids.clear();
        }
    }

    fn ssrc_index_or_insert(&mut self, ssrc: u32, sequence: u16) -> usize {
        if let Some(index) = self.ssrcs.iter().position(|state| state.ssrc == ssrc) {
            return index;
        }
        let sequence = u64::from(sequence);
        self.ssrcs.push(SsrcHistory {
            ssrc,
            first_sequence: sequence,
            newest_sequence: sequence,
            highest_acked_sequence: None,
            sent_ids: VecDeque::new(),
            last_report_timestamp: None,
        });
        self.ssrcs.len() - 1
    }

    fn remove_indexes(&mut self, entry: SentEntry) {
        if let Some(sequence) = entry.twcc_sequence {
            let slot = ring_index(sequence);
            if self.twcc_index[slot].is_some_and(|index| index.sent_id == entry.id) {
                self.twcc_index[slot] = None;
            }
        }
        if let Some(state) = self.ssrcs.iter_mut().find(|state| state.ssrc == entry.ssrc)
            && state.sent_ids.front() == Some(&entry.id)
        {
            state.sent_ids.pop_front();
        }
    }

    fn entry(&self, id: SentPacketId) -> Option<&SentEntry> {
        self.entries[ring_index(id.0)]
            .as_ref()
            .filter(|entry| entry.id == id)
    }

    fn entry_mut(&mut self, id: SentPacketId) -> Option<&mut SentEntry> {
        self.entries[ring_index(id.0)]
            .as_mut()
            .filter(|entry| entry.id == id)
    }

    fn add_unknown(&mut self, count: usize) {
        self.counters.unknown_feedback =
            self.counters.unknown_feedback.saturating_add(count as u64);
    }

    pub(crate) const fn counters(&self) -> HistoryCounters {
        self.counters
    }

    pub(crate) const fn controller_inputs(&self) -> &ControllerInputs {
        &self.inputs
    }

    pub(crate) fn clear_controller_inputs(&mut self) {
        self.inputs.feedback.clear();
        self.inputs.timing = None;
        self.inputs.path_change = None;
    }

    pub(crate) fn set_application_limited(&mut self, application_limited: bool) {
        self.inputs.application_limited = application_limited;
    }
}

impl PacketFeedback {
    pub(crate) fn controller_sample(self, received_at: Instant, origin: Instant) -> FeedbackSample {
        FeedbackSample {
            sent_at: self.committed_at.saturating_duration_since(origin),
            received_at: received_at.saturating_duration_since(origin),
            transport_bytes: self.transport_bytes,
            received: self.received,
            newly_acked: self.newly_acked,
            lost: self.lost,
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

impl CommitParticipant for SentHistory {
    fn preflight(&self, context: TransmitCommitContext) -> Result<(), HistoryError> {
        self.preflight_commit(context)
    }

    fn commit_preflighted(&mut self, context: TransmitCommitContext) {
        let result = self.commit_context(context);
        debug_assert_eq!(result, Ok(()));
    }
}

fn ring_index(value: u64) -> usize {
    usize::try_from(value % SENT_HISTORY_CAPACITY as u64).unwrap_or_default()
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

fn feedback_status_count(report: &FeedbackReport) -> usize {
    match report {
        FeedbackReport::Twcc { statuses, .. } => statuses.len(),
        FeedbackReport::Rfc8888 { reports, .. } => {
            reports.iter().map(|report| report.statuses.len()).sum()
        }
    }
}

mod feedback;
#[cfg(test)]
mod tests;
