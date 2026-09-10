#![allow(
    dead_code,
    reason = "Plan 10 consumes the private normalized controller inputs introduced here"
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
    Received,
    NotReceived,
    Retired,
}

impl Acknowledgment {
    const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
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
}

#[derive(Clone, Copy, Debug)]
struct TwccIndex {
    sequence: u64,
    sent_id: SentPacketId,
}

#[derive(Debug)]
struct SsrcHistory {
    ssrc: u32,
    newest_sequence: u64,
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
    newest_twcc: Option<u64>,
    twcc_reference: Option<i64>,
    twcc_feedback_count: Option<i64>,
    active_epoch: Option<PathEpoch>,
    inputs: ControllerInputs,
    counters: HistoryCounters,
    bytes_in_flight: u64,
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
            newest_twcc: None,
            twcc_reference: None,
            twcc_feedback_count: None,
            active_epoch: None,
            inputs: ControllerInputs {
                feedback: Vec::with_capacity(MAX_FEEDBACK_STATUSES),
                ..ControllerInputs::default()
            },
            counters: HistoryCounters::default(),
            bytes_in_flight: 0,
        }
    }

    fn commit_context(&mut self, context: TransmitCommitContext) -> Result<(), HistoryError> {
        if context.kind != DatagramKind::Rtp {
            return Ok(());
        }
        let (Some(epoch), Some(rtp)) = (context.path_epoch, context.rtp) else {
            return Err(HistoryError::InvalidCommit);
        };
        if self.active_epoch != Some(epoch) || context.wire_len == 0 {
            return Err(HistoryError::InvalidCommit);
        }
        if !matches!(
            (self.mode, rtp.twcc_sequence),
            (PacketFeedbackKind::TransportWide, Some(_)) | (PacketFeedbackKind::Rfc8888, None)
        ) {
            return Err(HistoryError::InvalidCommit);
        }
        let next_sent_id = self
            .next_sent_id
            .checked_add(1)
            .ok_or(HistoryError::Exhausted)?;
        let next_bytes_in_flight = self
            .bytes_in_flight
            .checked_add(context.wire_len as u64)
            .ok_or(HistoryError::Exhausted)?;
        let sent_id = SentPacketId(self.next_sent_id);
        let slot = ring_index(sent_id.0);
        if let Some(previous) = self.entries[slot]
            && !previous.acknowledgment.is_terminal()
            && previous.path_epoch == epoch
        {
            return Err(HistoryError::Exhausted);
        }
        if let Some(previous) = self.entries[slot] {
            self.remove_indexes(previous);
        }

        let ssrc_index = self.ssrc_index_or_insert(rtp.ssrc, rtp.sequence);
        let rtp_sequence = if self.ssrcs[ssrc_index].sent_ids.is_empty() {
            u64::from(rtp.sequence)
        } else {
            unwrap_forward(rtp.sequence, self.ssrcs[ssrc_index].newest_sequence)
        };
        self.ssrcs[ssrc_index].newest_sequence = rtp_sequence;
        let twcc_sequence = rtp.twcc_sequence.map(|sequence| {
            let unwrapped = self.newest_twcc.map_or_else(
                || u64::from(sequence),
                |newest| unwrap_forward(sequence, newest),
            );
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
        });
        self.bytes_in_flight = next_bytes_in_flight;
        self.inputs.bytes_in_flight = self.bytes_in_flight;
        self.next_sent_id = next_sent_id;
        Ok(())
    }

    pub(crate) fn path_changed(&mut self, epoch: PathEpoch, available: bool) {
        if self.active_epoch == Some(epoch) {
            return;
        }
        self.active_epoch = available.then_some(epoch);
        for entry in self.entries.iter_mut().flatten() {
            if entry.acknowledgment == Acknowledgment::Pending
                && self.active_epoch != Some(entry.path_epoch)
            {
                entry.acknowledgment = Acknowledgment::Retired;
                self.bytes_in_flight = self.bytes_in_flight.saturating_sub(entry.wire_len as u64);
            }
        }
        self.inputs.bytes_in_flight = self.bytes_in_flight;
        self.inputs.path_change = Some(PathChange { epoch, available });
        self.twcc_reference = None;
        self.twcc_feedback_count = None;
        for ssrc in &mut self.ssrcs {
            ssrc.last_report_timestamp = None;
        }
    }

    pub(crate) fn process_feedback(&mut self, batch: FeedbackBatch) {
        self.inputs.feedback.clear();
        self.inputs.timing = None;
        if self.active_epoch != Some(batch.path_epoch) {
            let count = feedback_status_count(&batch.report);
            self.counters.wrong_path_feedback = self
                .counters
                .wrong_path_feedback
                .saturating_add(count as u64);
            return;
        }
        let received_at = batch.received_at.monotonic;
        let feedback_hold = match batch.report {
            FeedbackReport::Twcc {
                base_sequence,
                reference_time,
                feedback_count,
                statuses,
                ..
            } => {
                self.process_twcc(
                    base_sequence,
                    reference_time,
                    feedback_count,
                    &statuses,
                    batch.path_epoch,
                );
                None
            }
            FeedbackReport::Rfc8888 {
                reports,
                report_timestamp,
            } => self.process_rfc8888(&reports, report_timestamp, batch.path_epoch),
        };
        let newest_send_age = self
            .inputs
            .feedback
            .iter()
            .filter_map(|feedback| self.entry(feedback.sent_id))
            .map(|entry| received_at.saturating_duration_since(entry.committed_at))
            .min();
        self.inputs.timing = Some(FeedbackTiming {
            received_at,
            feedback_hold,
            newest_send_age,
        });
    }

    pub(crate) fn expire(&mut self, now: Instant, limit: usize) -> usize {
        let mut work = 0;
        while work < limit && self.oldest_sent_id < self.next_sent_id {
            let id = SentPacketId(self.oldest_sent_id);
            let Some(entry) = self.entry(id) else {
                self.oldest_sent_id += 1;
                work += 1;
                continue;
            };
            let Some(deadline) = entry.committed_at.checked_add(SENT_HISTORY_MAX_AGE) else {
                break;
            };
            if now < deadline {
                break;
            }
            if let Some(entry) = self.entry_mut(id)
                && entry.acknowledgment == Acknowledgment::Pending
            {
                entry.acknowledgment = Acknowledgment::NotReceived;
                let entry = *entry;
                self.bytes_in_flight = self.bytes_in_flight.saturating_sub(entry.wire_len as u64);
                self.inputs.bytes_in_flight = self.bytes_in_flight;
                self.counters.expired = self.counters.expired.saturating_add(1);
                if self.inputs.feedback.len() < MAX_FEEDBACK_STATUSES {
                    self.inputs.feedback.push(PacketFeedback {
                        sent_id: id,
                        committed_at: entry.committed_at,
                        transport_bytes: u32::try_from(entry.wire_len).unwrap_or(u32::MAX),
                        service: entry.service,
                        received: false,
                        receiver_arrival: None,
                        ecn: None,
                    });
                }
            }
            self.oldest_sent_id += 1;
            work += 1;
        }
        work
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let mut id = self.oldest_sent_id;
        while id < self.next_sent_id {
            if let Some(entry) = self.entry(SentPacketId(id)) {
                return entry.committed_at.checked_add(SENT_HISTORY_MAX_AGE);
            }
            id += 1;
        }
        None
    }

    fn process_twcc(
        &mut self,
        base_sequence: u16,
        reference_time: u32,
        feedback_count: u8,
        statuses: &[TwccStatus],
        epoch: PathEpoch,
    ) {
        let Some(newest) = self.newest_twcc else {
            self.add_unknown(statuses.len());
            return;
        };
        let Some(base) = unwrap_near(u64::from(base_sequence), newest, 1 << 16) else {
            self.add_unknown(statuses.len());
            return;
        };
        let reference = unwrap_near_signed(i64::from(reference_time), self.twcc_reference, 1 << 24);
        self.twcc_reference = Some(reference);
        let count = unwrap_near_signed(i64::from(feedback_count), self.twcc_feedback_count, 1 << 8);
        if self
            .twcc_feedback_count
            .is_some_and(|previous| count <= previous)
        {
            self.counters.reordered_feedback = self.counters.reordered_feedback.saturating_add(1);
        } else {
            self.twcc_feedback_count = Some(count);
        }
        let mut arrival_micros = reference.saturating_mul(64_000);
        for (offset, status) in statuses.iter().enumerate() {
            let sequence = base.saturating_add(offset as u64);
            let arrival = match status {
                TwccStatus::NotReceived => None,
                TwccStatus::Received { delta_250us } => {
                    arrival_micros =
                        arrival_micros.saturating_add(i64::from(*delta_250us).saturating_mul(250));
                    Some(ReceiverTime::Micros(arrival_micros))
                }
            };
            let received = matches!(status, TwccStatus::Received { .. });
            let sent_id = self.twcc_index[ring_index(sequence)]
                .filter(|index| index.sequence == sequence)
                .map(|index| index.sent_id);
            self.resolve(sent_id, newest, sequence, epoch, received, arrival, None);
        }
    }

    fn process_rfc8888(
        &mut self,
        reports: &[crate::rtcp::Rfc8888Report],
        report_timestamp: u32,
        epoch: PathEpoch,
    ) -> Option<Duration> {
        let mut minimum_hold: Option<Duration> = None;
        for report in reports {
            let Some(ssrc_index) = self
                .ssrcs
                .iter()
                .position(|state| state.ssrc == report.ssrc)
            else {
                self.add_unknown(report.statuses.len());
                continue;
            };
            let newest = self.ssrcs[ssrc_index].newest_sequence;
            let Some(base) = unwrap_near(u64::from(report.begin_sequence), newest, 1 << 16) else {
                self.add_unknown(report.statuses.len());
                continue;
            };
            let timestamp = unwrap_near_signed(
                i64::from(report_timestamp),
                self.ssrcs[ssrc_index].last_report_timestamp,
                1_i64 << 32,
            );
            self.ssrcs[ssrc_index].last_report_timestamp = Some(timestamp);
            let report_micros = timestamp.saturating_mul(1_000_000) / 65_536;
            for (offset, status) in report.statuses.iter().enumerate() {
                let sequence = base.saturating_add(offset as u64);
                let (received, arrival, ecn, hold) = match status {
                    Rfc8888Status::NotReceived => (false, None, None, None),
                    Rfc8888Status::Received {
                        ecn,
                        arrival_offset,
                    } => match arrival_offset {
                        ArrivalOffset::Ticks(ticks) => {
                            let hold_micros = u64::from(*ticks).saturating_mul(1_000_000) / 1_024;
                            (
                                true,
                                Some(ReceiverTime::Micros(
                                    report_micros.saturating_sub(hold_micros as i64),
                                )),
                                Some(ecn_mark(*ecn)),
                                Some(Duration::from_micros(hold_micros)),
                            )
                        }
                        ArrivalOffset::OverRange => (
                            true,
                            Some(ReceiverTime::OverRange),
                            Some(ecn_mark(*ecn)),
                            None,
                        ),
                        ArrivalOffset::Unavailable => (
                            true,
                            Some(ReceiverTime::Unavailable),
                            Some(ecn_mark(*ecn)),
                            None,
                        ),
                    },
                };
                minimum_hold = match (minimum_hold, hold) {
                    (Some(known), Some(value)) => Some(known.min(value)),
                    (None, Some(value)) => Some(value),
                    (known, None) => known,
                };
                let sent_id = self.lookup_rtp(ssrc_index, sequence);
                self.resolve(sent_id, newest, sequence, epoch, received, arrival, ecn);
            }
        }
        minimum_hold
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are one normalized packet status"
    )]
    fn resolve(
        &mut self,
        sent_id: Option<SentPacketId>,
        newest: u64,
        sequence: u64,
        epoch: PathEpoch,
        received: bool,
        receiver_arrival: Option<ReceiverTime>,
        ecn: Option<EcnMark>,
    ) {
        if newest.saturating_sub(sequence) > MAX_ACK_REORDERING {
            self.counters.stale_feedback = self.counters.stale_feedback.saturating_add(1);
            return;
        }
        let Some(sent_id) = sent_id else {
            self.add_unknown(1);
            return;
        };
        let Some(entry) = self.entry_mut(sent_id) else {
            self.add_unknown(1);
            return;
        };
        if entry.path_epoch != epoch {
            self.counters.wrong_path_feedback = self.counters.wrong_path_feedback.saturating_add(1);
            return;
        }
        if entry.acknowledgment != Acknowledgment::Pending {
            self.counters.duplicate_feedback = self.counters.duplicate_feedback.saturating_add(1);
            return;
        }
        entry.acknowledgment = if received {
            Acknowledgment::Received
        } else {
            Acknowledgment::NotReceived
        };
        let entry = *entry;
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(entry.wire_len as u64);
        self.inputs.bytes_in_flight = self.bytes_in_flight;
        if received {
            self.counters.received = self.counters.received.saturating_add(1);
        } else {
            self.counters.not_received = self.counters.not_received.saturating_add(1);
        }
        self.inputs.feedback.push(PacketFeedback {
            sent_id,
            committed_at: entry.committed_at,
            transport_bytes: u32::try_from(entry.wire_len).unwrap_or(u32::MAX),
            service: entry.service,
            received,
            receiver_arrival,
            ecn,
        });
    }

    fn lookup_rtp(&self, ssrc_index: usize, sequence: u64) -> Option<SentPacketId> {
        let ids = &self.ssrcs[ssrc_index].sent_ids;
        let mut low = 0;
        let mut high = ids.len();
        while low < high {
            let middle = low + (high - low) / 2;
            let id = *ids.get(middle)?;
            let entry = self.entry(id)?;
            match entry.rtp_sequence.cmp(&sequence) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(id),
            }
        }
        None
    }

    fn ssrc_index_or_insert(&mut self, ssrc: u32, sequence: u16) -> usize {
        if let Some(index) = self.ssrcs.iter().position(|state| state.ssrc == ssrc) {
            return index;
        }
        self.ssrcs.push(SsrcHistory {
            ssrc,
            newest_sequence: u64::from(sequence),
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
    fn commit(&mut self, context: TransmitCommitContext) -> Result<(), HistoryError> {
        self.commit_context(context)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        GlobalMediaTime, TimePoint,
        rtcp::{Rfc8888Report, Rfc8888Status},
        transport::{PreparedRtpIdentity, RtpService},
    };
    use proptest::prelude::*;

    fn context(
        at: Instant,
        epoch: PathEpoch,
        ssrc: u32,
        sequence: u16,
        twcc: Option<u16>,
    ) -> TransmitCommitContext {
        TransmitCommitContext {
            at,
            kind: DatagramKind::Rtp,
            wire_len: 1200,
            path_epoch: Some(epoch),
            rtp: Some(PreparedRtpIdentity {
                ssrc,
                sequence,
                twcc_sequence: twcc,
                service: RtpService::Original,
            }),
        }
    }

    fn at(monotonic: Instant) -> TimePoint {
        TimePoint {
            monotonic,
            global: GlobalMediaTime::from_micros(1),
        }
    }

    #[test]
    fn history_wrap_and_duplicate_feedback_are_generation_safe() {
        let now = Instant::now();
        let epoch = PathEpoch::from_value(1);
        let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
        history.path_changed(epoch, true);
        history
            .commit(context(now, epoch, 7, u16::MAX, Some(u16::MAX)))
            .unwrap();
        history.commit(context(now, epoch, 7, 0, Some(0))).unwrap();
        let batch = FeedbackBatch {
            received_at: at(now + Duration::from_millis(20)),
            path_epoch: epoch,
            sender_ssrc: 9,
            report: FeedbackReport::Twcc {
                media_ssrc: 7,
                base_sequence: u16::MAX,
                reference_time: 0x00ff_fffe,
                feedback_count: 1,
                statuses: vec![
                    TwccStatus::Received { delta_250us: 4 },
                    TwccStatus::Received { delta_250us: 4 },
                ]
                .into(),
            },
        };
        history.process_feedback(batch.clone());
        assert_eq!(history.inputs.feedback.len(), 2);
        assert_eq!(history.inputs.bytes_in_flight, 0);
        let sample =
            history.inputs.feedback[0].controller_sample(now + Duration::from_millis(20), now);
        assert_eq!(sample.transport_bytes, 1_200);
        assert_eq!(sample.sent_at, Duration::ZERO);
        assert!(sample.received);
        history.process_feedback(batch);
        assert!(history.inputs.feedback.is_empty());
        assert_eq!(history.counters.duplicate_feedback, 2);
    }

    #[test]
    fn non_rtp_transport_never_enters_controller_bytes_in_flight() {
        let now = Instant::now();
        let epoch = PathEpoch::from_value(11);
        let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
        history.path_changed(epoch, true);
        history
            .commit(TransmitCommitContext {
                at: now,
                kind: DatagramKind::Dtls,
                wire_len: 4_096,
                path_epoch: Some(epoch),
                rtp: None,
            })
            .unwrap();
        assert_eq!(history.inputs.bytes_in_flight, 0);
        history.commit(context(now, epoch, 7, 1, Some(1))).unwrap();
        assert_eq!(history.inputs.bytes_in_flight, 1_200);
    }

    #[test]
    fn feedback_from_before_the_initial_generation_cannot_alias_sequence_zero() {
        let now = Instant::now();
        let epoch = PathEpoch::from_value(10);
        let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
        history.path_changed(epoch, true);
        history.commit(context(now, epoch, 7, 0, Some(0))).unwrap();
        history.process_feedback(FeedbackBatch {
            received_at: at(now + Duration::from_millis(20)),
            path_epoch: epoch,
            sender_ssrc: 9,
            report: FeedbackReport::Twcc {
                media_ssrc: 7,
                base_sequence: u16::MAX,
                reference_time: 0,
                feedback_count: 1,
                statuses: vec![TwccStatus::Received { delta_250us: 1 }].into(),
            },
        });
        assert!(history.inputs.feedback.is_empty());
        assert_eq!(history.counters.unknown_feedback, 1);
        assert_eq!(
            history.entry(SentPacketId(0)).unwrap().acknowledgment,
            Acknowledgment::Pending
        );
    }

    #[test]
    fn rfc8888_keeps_ssrc_progression_and_arrival_sentinels_independent() {
        let now = Instant::now();
        let epoch = PathEpoch::from_value(2);
        let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
        history.path_changed(epoch, true);
        history.commit(context(now, epoch, 7, 10, None)).unwrap();
        history.commit(context(now, epoch, 8, 10, None)).unwrap();
        history.process_feedback(FeedbackBatch {
            received_at: at(now + Duration::from_millis(50)),
            path_epoch: epoch,
            sender_ssrc: 9,
            report: FeedbackReport::Rfc8888 {
                reports: vec![
                    Rfc8888Report {
                        ssrc: 7,
                        begin_sequence: 10,
                        report_count: 1,
                        statuses: vec![Rfc8888Status::Received {
                            ecn: 3,
                            arrival_offset: ArrivalOffset::OverRange,
                        }]
                        .into(),
                    },
                    Rfc8888Report {
                        ssrc: 8,
                        begin_sequence: 10,
                        report_count: 1,
                        statuses: vec![Rfc8888Status::Received {
                            ecn: 1,
                            arrival_offset: ArrivalOffset::Unavailable,
                        }]
                        .into(),
                    },
                ]
                .into(),
                report_timestamp: u32::MAX,
            },
        });
        assert_eq!(history.inputs.feedback.len(), 2);
        assert_eq!(
            history.inputs.feedback[0].receiver_arrival,
            Some(ReceiverTime::OverRange)
        );
        assert_eq!(history.inputs.feedback[0].ecn, Some(EcnMark::Ce));
        assert_eq!(
            history.inputs.feedback[1].receiver_arrival,
            Some(ReceiverTime::Unavailable)
        );
        assert_eq!(history.inputs.feedback[1].ecn, Some(EcnMark::Ect1));
    }

    #[test]
    fn ring_reuse_rejects_stale_generation_without_double_acknowledgment() {
        let now = Instant::now();
        let epoch = PathEpoch::from_value(3);
        let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
        history.path_changed(epoch, true);
        for sequence in 0..SENT_HISTORY_CAPACITY {
            let sequence = u16::try_from(sequence).expect("history capacity fits u16");
            history
                .commit(context(now, epoch, 7, sequence, Some(sequence)))
                .unwrap();
            let id = SentPacketId(u64::from(sequence));
            history.entry_mut(id).unwrap().acknowledgment = Acknowledgment::Received;
        }
        history.commit(context(now, epoch, 7, 0, Some(0))).unwrap();
        assert!(history.entry(SentPacketId(0)).is_none());
        assert_eq!(
            history
                .entry(SentPacketId(SENT_HISTORY_CAPACITY as u64))
                .unwrap()
                .wire_len,
            1200
        );
    }

    #[test]
    fn expiration_and_path_replacement_are_bounded_and_epoch_checked() {
        let now = Instant::now();
        let first = PathEpoch::from_value(4);
        let second = PathEpoch::from_value(5);
        let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
        history.path_changed(first, true);
        for sequence in 0..300 {
            history
                .commit(context(now, first, 7, sequence, Some(sequence)))
                .unwrap();
        }
        assert_eq!(history.expire(now + Duration::from_secs(5), 256), 256);
        assert_eq!(history.counters.expired, 256);
        assert_eq!(history.expire(now + Duration::from_secs(5), 256), 44);
        history
            .commit(context(
                now + Duration::from_secs(1),
                first,
                7,
                300,
                Some(300),
            ))
            .unwrap();
        history.path_changed(second, true);
        assert_eq!(
            history.entry(SentPacketId(300)).unwrap().acknowledgment,
            Acknowledgment::Retired
        );
        history.process_feedback(FeedbackBatch {
            received_at: at(now + Duration::from_secs(5)),
            path_epoch: first,
            sender_ssrc: 9,
            report: FeedbackReport::Twcc {
                media_ssrc: 7,
                base_sequence: 0,
                reference_time: 0,
                feedback_count: 0,
                statuses: vec![TwccStatus::Received { delta_250us: 1 }].into(),
            },
        });
        assert!(history.inputs.feedback.is_empty());
        assert_eq!(history.counters.wrong_path_feedback, 1);
        assert_eq!(
            history.inputs.path_change,
            Some(PathChange {
                epoch: second,
                available: true
            })
        );
    }

    #[test]
    fn one_feedback_batch_never_exceeds_the_wire_parser_bound() {
        let now = Instant::now();
        let epoch = PathEpoch::from_value(6);
        let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
        history.path_changed(epoch, true);
        history.process_feedback(FeedbackBatch {
            received_at: at(now),
            path_epoch: epoch,
            sender_ssrc: 9,
            report: FeedbackReport::Twcc {
                media_ssrc: 7,
                base_sequence: 0,
                reference_time: 0,
                feedback_count: 0,
                statuses: vec![TwccStatus::NotReceived; MAX_FEEDBACK_STATUSES].into(),
            },
        });
        assert_eq!(
            history.counters.unknown_feedback,
            MAX_FEEDBACK_STATUSES as u64
        );
        assert!(history.inputs.feedback.capacity() >= MAX_FEEDBACK_STATUSES);
    }

    #[test]
    fn pending_ring_exhaustion_is_terminal_but_an_old_path_cannot_block_reuse() {
        let now = Instant::now();
        let first = PathEpoch::from_value(7);
        let second = PathEpoch::from_value(8);
        let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
        history.path_changed(first, true);
        for sequence in 0..SENT_HISTORY_CAPACITY {
            let sequence = u16::try_from(sequence).expect("history capacity fits u16");
            history
                .commit(context(now, first, 7, sequence, Some(sequence)))
                .unwrap();
        }
        assert_eq!(
            history.commit(context(now, first, 7, 0, Some(0))),
            Err(HistoryError::Exhausted)
        );
        history.path_changed(second, true);
        assert_eq!(history.commit(context(now, second, 7, 1, Some(1))), Ok(()));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn reordered_overlapping_feedback_never_acknowledges_twice(
            start in any::<u16>(),
            split in 1usize..63,
        ) {
            let now = Instant::now();
            let epoch = PathEpoch::from_value(9);
            let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
            history.path_changed(epoch, true);
            for offset in 0..64u16 {
                let sequence = start.wrapping_add(offset);
                history
                    .commit(context(now, epoch, 7, sequence, Some(sequence)))
                    .unwrap();
            }
            let make_batch = |base: u16, count: usize, feedback_count: u8| FeedbackBatch {
                received_at: at(now + Duration::from_millis(20)),
                path_epoch: epoch,
                sender_ssrc: 9,
                report: FeedbackReport::Twcc {
                    media_ssrc: 7,
                    base_sequence: base,
                    reference_time: 1,
                    feedback_count,
                    statuses: vec![TwccStatus::Received { delta_250us: 1 }; count].into(),
                },
            };
            let split_sequence = u16::try_from(split).expect("property split fits u16");
            history.process_feedback(make_batch(start.wrapping_add(split_sequence), 64 - split, 1));
            history.process_feedback(make_batch(start, split, 2));
            prop_assert_eq!(history.counters.received, 64);
            history.process_feedback(make_batch(start, 64, 3));
            prop_assert_eq!(history.counters.received, 64);
            prop_assert_eq!(history.counters.duplicate_feedback, 64);
        }
    }
}
