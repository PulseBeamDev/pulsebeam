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
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    mem::{size_of, size_of_val},
    time::{Duration, Instant},
};

use crate::{
    PacketFeedbackKind,
    congestion::{EcnMark as ControllerEcnMark, FeedbackSample, LossOrigin},
    connection::TransmitCommitContext,
    rtcp::{ArrivalOffset, FeedbackBatch, FeedbackReport, Rfc8888Status, TwccStatus},
    transport::{DatagramKind, PathEpoch, PreparedRtpIdentity, RtpService},
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

#[cfg(test)]
impl SentPacketId {
    pub(crate) const fn for_test(value: u64) -> Self {
        Self(value)
    }
}

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

#[derive(Debug, Default)]
pub(crate) struct ControllerInputs {
    pub(crate) feedback: Vec<PacketFeedback>,
    pub(crate) synthetic: Vec<PacketFeedback>,
    pub(crate) exhausted: bool,
    pub(crate) timing: Option<FeedbackTiming>,
    pub(crate) fresh_network_feedback: bool,
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
    Missing {
        since: Instant,
        higher_received_at: Option<Instant>,
    },
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
    controller_loss_reported: bool,
    missing_since: Option<Instant>,
    frozen_reorder_deadline: Option<Instant>,
    in_flight: bool,
}

#[derive(Clone, Copy, Debug)]
struct TwccIndex {
    sequence: u64,
    sent_id: SentPacketId,
}

#[derive(Debug)]
struct SsrcHistory {
    first_sequence: u64,
    newest_sequence: u64,
    highest_acked_sequence: Option<u64>,
    by_sequence: HashMap<u64, SentPacketId>,
    // Tombstones can reduce usable capacity without releasing table backing.
    index_capacity: usize,
    last_report_timestamp: Option<i64>,
    latest_commit_at: Option<Instant>,
    terminal: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SourceReadiness {
    Retired {
        deadline: Option<Instant>,
        ssrc: u32,
    },
    Unscheduled(u32),
}

impl SsrcHistory {
    fn readiness(&self, ssrc: u32) -> SourceReadiness {
        if self.terminal {
            match self.latest_commit_at {
                None => {
                    return SourceReadiness::Retired {
                        deadline: None,
                        ssrc,
                    };
                }
                Some(at) => {
                    if let Some(deadline) = at.checked_add(SENT_HISTORY_MAX_AGE) {
                        return SourceReadiness::Retired {
                            deadline: Some(deadline),
                            ssrc,
                        };
                    }
                    // An unrepresentable age deadline cannot establish quiescence.
                }
            }
        }
        SourceReadiness::Unscheduled(ssrc)
    }
}

const SOURCE_PAYLOAD_BYTES: usize =
    size_of::<u32>() + size_of::<SsrcHistory>() + size_of::<SourceReadiness>();
const SOURCE_INDEX_ENTRY_BYTES: usize = size_of::<(u64, SentPacketId)>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HistoryError {
    Exhausted,
    InvalidCommit,
}

pub(crate) struct PreparedFeedbackProfile<'a> {
    history: &'a mut SentHistory,
    profile: PacketFeedbackKind,
    storage: Option<FixedHistoryStorage>,
}

impl PreparedFeedbackProfile<'_> {
    pub(crate) fn commit(self) {
        if let Some(storage) = self.storage {
            self.history.entries = storage.entries;
            self.history.twcc_index = storage.twcc_index;
            self.history.inputs.feedback = storage.feedback;
            self.history.inputs.synthetic = storage.synthetic;
        }
        self.history.mode = Some(self.profile);
    }
}

struct FixedHistoryStorage {
    entries: Box<[Option<SentEntry>]>,
    twcc_index: Box<[Option<TwccIndex>]>,
    feedback: Vec<PacketFeedback>,
    synthetic: Vec<PacketFeedback>,
}

impl FixedHistoryStorage {
    fn bytes(feedback_capacity: usize, synthetic_capacity: usize) -> Option<usize> {
        SENT_HISTORY_CAPACITY
            .checked_mul(size_of::<Option<SentEntry>>())?
            .checked_add(SENT_HISTORY_CAPACITY.checked_mul(size_of::<Option<TwccIndex>>())?)?
            .checked_add(
                feedback_capacity
                    .checked_add(synthetic_capacity)?
                    .checked_mul(size_of::<PacketFeedback>())?,
            )
    }

    fn new(limit: usize) -> Result<Self, HistoryError> {
        Self::bytes(MAX_FEEDBACK_STATUSES, MAX_EXPIRATIONS_PER_POLL)
            .filter(|bytes| *bytes <= limit)
            .ok_or(HistoryError::Exhausted)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(SENT_HISTORY_CAPACITY)
            .map_err(|_| HistoryError::Exhausted)?;
        entries.resize(SENT_HISTORY_CAPACITY, None);
        let mut twcc_index = Vec::new();
        twcc_index
            .try_reserve_exact(SENT_HISTORY_CAPACITY)
            .map_err(|_| HistoryError::Exhausted)?;
        twcc_index.resize(SENT_HISTORY_CAPACITY, None);
        let mut feedback = Vec::new();
        feedback
            .try_reserve_exact(MAX_FEEDBACK_STATUSES)
            .map_err(|_| HistoryError::Exhausted)?;
        let mut synthetic = Vec::new();
        synthetic
            .try_reserve_exact(MAX_EXPIRATIONS_PER_POLL)
            .map_err(|_| HistoryError::Exhausted)?;
        Self::bytes(feedback.capacity(), synthetic.capacity())
            .filter(|bytes| *bytes <= limit)
            .ok_or(HistoryError::Exhausted)?;
        Ok(Self {
            entries: entries.into_boxed_slice(),
            twcc_index: twcc_index.into_boxed_slice(),
            feedback,
            synthetic,
        })
    }
}

pub(crate) struct SentHistory {
    mode: Option<PacketFeedbackKind>,
    max_backing_bytes: usize,
    entries: Box<[Option<SentEntry>]>,
    twcc_index: Box<[Option<TwccIndex>]>,
    ssrcs: BTreeMap<u32, SsrcHistory>,
    source_backing_bytes: usize,
    source_readiness: BTreeSet<SourceReadiness>,
    next_sent_id: u64,
    oldest_sent_id: u64,
    oldest_twcc: Option<u64>,
    newest_twcc: Option<u64>,
    highest_twcc_acked: Option<u64>,
    twcc_reference: Option<i64>,
    twcc_feedback_count: Option<i64>,
    active_epoch: Option<PathEpoch>,
    last_path: Option<(PathEpoch, bool)>,
    inputs: ControllerInputs,
    counters: HistoryCounters,
    bytes_in_flight: u64,
    reordering_window: Duration,
    observed_reordering_window: Duration,
    missing: VecDeque<SentPacketId>,
    loss_work: usize,
}

impl SentHistory {
    #[cfg(test)]
    pub(crate) fn new(mode: impl Into<Option<PacketFeedbackKind>>) -> Self {
        Self::with_limit(
            mode,
            crate::ConnectionLimits::DEFAULT_MAX_SENT_HISTORY_BYTES,
        )
        .expect("default sent-history backing allowance")
    }

    pub(crate) fn with_limit(
        mode: impl Into<Option<PacketFeedbackKind>>,
        max_backing_bytes: usize,
    ) -> Result<Self, HistoryError> {
        let mut history = Self {
            mode: None,
            max_backing_bytes,
            entries: Box::new([]),
            twcc_index: Box::new([]),
            ssrcs: BTreeMap::new(),
            source_backing_bytes: 0,
            source_readiness: BTreeSet::new(),
            next_sent_id: 0,
            oldest_sent_id: 0,
            oldest_twcc: None,
            newest_twcc: None,
            highest_twcc_acked: None,
            twcc_reference: None,
            twcc_feedback_count: None,
            active_epoch: None,
            last_path: None,
            inputs: ControllerInputs::default(),
            counters: HistoryCounters::default(),
            bytes_in_flight: 0,
            reordering_window: INITIAL_REORDERING_WINDOW,
            observed_reordering_window: INITIAL_REORDERING_WINDOW,
            missing: VecDeque::new(),
            loss_work: 0,
        };
        if let Some(profile) = mode.into() {
            history.prepare_feedback_profile(profile)?.commit();
        }
        Ok(history)
    }

    pub(crate) fn prepare_feedback_profile(
        &mut self,
        profile: PacketFeedbackKind,
    ) -> Result<PreparedFeedbackProfile<'_>, HistoryError> {
        let storage = match self.mode {
            None => Some(FixedHistoryStorage::new(self.max_backing_bytes)?),
            Some(current) if current == profile => None,
            Some(_) => return Err(HistoryError::InvalidCommit),
        };
        Ok(PreparedFeedbackProfile {
            history: self,
            profile,
            storage,
        })
    }

    fn backing_bytes(&self) -> Option<usize> {
        size_of_val(self.entries.as_ref())
            .checked_add(size_of_val(self.twcc_index.as_ref()))?
            .checked_add(
                self.inputs
                    .feedback
                    .capacity()
                    .checked_add(self.inputs.synthetic.capacity())?
                    .checked_mul(size_of::<PacketFeedback>())?,
            )?
            .checked_add(
                self.missing
                    .capacity()
                    .checked_mul(size_of::<SentPacketId>())?,
            )?
            .checked_add(self.source_backing_bytes)
    }

    #[cfg(test)]
    pub(crate) fn assert_storage(&self) -> usize {
        assert_eq!(self.source_readiness.len(), self.ssrcs.len());
        let sources = self
            .ssrcs
            .iter()
            .try_fold(0usize, |bytes, (&ssrc, source)| {
                assert!(self.source_readiness.contains(&source.readiness(ssrc)));
                assert!(source.by_sequence.capacity() <= source.index_capacity);
                bytes.checked_add(SOURCE_PAYLOAD_BYTES)?.checked_add(
                    source
                        .index_capacity
                        .checked_mul(SOURCE_INDEX_ENTRY_BYTES)?,
                )
            })
            .unwrap();
        assert_eq!(self.source_backing_bytes, sources);
        let bytes = self.backing_bytes().unwrap();
        assert!(bytes <= self.max_backing_bytes);
        bytes
    }

    fn reserve_missing_slot(&mut self) -> Option<()> {
        if self.missing.len() < self.missing.capacity() {
            return Some(());
        }
        let stride = size_of::<SentPacketId>();
        let base = self
            .backing_bytes()?
            .checked_sub(self.missing.capacity().checked_mul(stride)?)?;
        let max_capacity = self.max_backing_bytes.checked_sub(base)? / stride;
        let required = self.missing.len().checked_add(1)?;
        if required > max_capacity {
            return None;
        }
        let target = self
            .missing
            .capacity()
            .checked_mul(2)
            .unwrap_or(max_capacity)
            .max(required)
            .min(max_capacity);
        let mut replacement = VecDeque::new();
        replacement.try_reserve_exact(target).ok()?;
        let bytes = base.checked_add(replacement.capacity().checked_mul(stride)?)?;
        if bytes > self.max_backing_bytes {
            return None;
        }
        replacement.extend(self.missing.drain(..));
        self.missing = replacement;
        Some(())
    }

    fn reserve_feedback_slot(&mut self) -> Option<()> {
        if self.inputs.feedback.len() < self.inputs.feedback.capacity() {
            return Some(());
        }
        let stride = size_of::<PacketFeedback>();
        let base = self
            .backing_bytes()?
            .checked_sub(self.inputs.feedback.capacity().checked_mul(stride)?)?;
        let max_capacity = self.max_backing_bytes.checked_sub(base)? / stride;
        let required = self.inputs.feedback.len().checked_add(1)?;
        if required > max_capacity {
            return None;
        }
        let target = self
            .inputs
            .feedback
            .capacity()
            .checked_mul(2)
            .unwrap_or(max_capacity)
            .max(required)
            .min(max_capacity);
        let mut replacement = Vec::new();
        replacement.try_reserve_exact(target).ok()?;
        let bytes = base.checked_add(replacement.capacity().checked_mul(stride)?)?;
        if bytes > self.max_backing_bytes {
            return None;
        }
        replacement.extend(self.inputs.feedback.drain(..));
        self.inputs.feedback = replacement;
        Some(())
    }

    pub(crate) fn prepare_commit(
        &mut self,
        context: TransmitCommitContext,
    ) -> Result<(), HistoryError> {
        self.validate_commit(context)?;
        let Some(rtp) = context.rtp.filter(|_| context.kind == DatagramKind::Rtp) else {
            return Ok(());
        };
        // Removing an overwritten key may leave a tombstone, not insertion capacity.
        let required = self
            .ssrcs
            .get(&rtp.ssrc)
            .map_or(0, |state| state.by_sequence.len())
            .checked_add(1)
            .ok_or(HistoryError::Exhausted)?;
        self.reserve_source_capacity(rtp, required)
    }

    fn reserve_source_capacity(
        &mut self,
        rtp: PreparedRtpIdentity,
        required: usize,
    ) -> Result<(), HistoryError> {
        let state = self.ssrcs.get(&rtp.ssrc);
        if state.is_some_and(|state| required <= state.by_sequence.capacity()) {
            return Ok(());
        }
        let old_capacity = state.map_or(0, |state| state.index_capacity);
        let reserve = || -> Option<(HashMap<u64, SentPacketId>, usize)> {
            let stride = SOURCE_INDEX_ENTRY_BYTES;
            let payload = if state.is_none() {
                SOURCE_PAYLOAD_BYTES
            } else {
                0
            };
            let old_bytes = old_capacity.checked_mul(stride)?;
            let base = self
                .backing_bytes()?
                .checked_sub(old_bytes)?
                .checked_add(payload)?;
            let max_capacity = self.max_backing_bytes.checked_sub(base)? / stride;
            if required > max_capacity {
                return None;
            }
            let target = if required <= old_capacity {
                old_capacity
            } else {
                old_capacity.checked_mul(2).unwrap_or(max_capacity)
            }
            .max(required)
            .min(max_capacity);
            let mut replacement = HashMap::new();
            replacement.try_reserve(target).ok()?;
            if replacement.capacity() > max_capacity && target > required {
                replacement = HashMap::new();
                replacement.try_reserve(required).ok()?;
            }
            let bytes = replacement.capacity().checked_mul(stride)?;
            if base.checked_add(bytes)? > self.max_backing_bytes {
                return None;
            }
            let sources = self
                .source_backing_bytes
                .checked_sub(old_bytes)?
                .checked_add(payload)?
                .checked_add(bytes)?;
            Some((replacement, sources))
        };
        let (mut replacement, sources) = reserve().ok_or(HistoryError::Exhausted)?;
        let index_capacity = replacement.capacity();
        if let Some(state) = self.ssrcs.get_mut(&rtp.ssrc) {
            replacement.extend(state.by_sequence.drain());
            state.by_sequence = replacement;
            state.index_capacity = index_capacity;
        } else {
            self.ssrcs.insert(
                rtp.ssrc,
                SsrcHistory {
                    first_sequence: rtp.index,
                    newest_sequence: rtp.index,
                    highest_acked_sequence: None,
                    by_sequence: replacement,
                    index_capacity,
                    last_report_timestamp: None,
                    latest_commit_at: None,
                    terminal: false,
                },
            );
            self.source_readiness
                .insert(SourceReadiness::Unscheduled(rtp.ssrc));
        }
        self.source_backing_bytes = sources;
        Ok(())
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
        let rtp_sequence = rtp.index;
        let state = self
            .ssrcs
            .get_mut(&rtp.ssrc)
            .ok_or(HistoryError::InvalidCommit)?;
        if state.by_sequence.is_empty() {
            state.first_sequence = rtp_sequence;
            state.newest_sequence = rtp_sequence;
        } else {
            state.first_sequence = state.first_sequence.min(rtp_sequence);
            state.newest_sequence = state.newest_sequence.max(rtp_sequence);
        }
        state.by_sequence.insert(rtp.index, sent_id);
        state.latest_commit_at = Some(
            state
                .latest_commit_at
                .map_or(context.at, |at| at.max(context.at)),
        );
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
            controller_loss_reported: false,
            missing_since: None,
            frozen_reorder_deadline: None,
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
        self.prepare_commit(context)?;
        self.commit_context(context)
    }

    pub(crate) fn preflight_commit(
        &self,
        context: TransmitCommitContext,
    ) -> Result<(), HistoryError> {
        self.validate_commit(context)?;
        if let Some(rtp) = context.rtp.filter(|_| context.kind == DatagramKind::Rtp)
            && self
                .ssrcs
                .get(&rtp.ssrc)
                .is_none_or(|state| state.by_sequence.len() >= state.by_sequence.capacity())
        {
            return Err(HistoryError::Exhausted);
        }
        Ok(())
    }

    fn validate_commit(&self, context: TransmitCommitContext) -> Result<(), HistoryError> {
        if context.kind != DatagramKind::Rtp {
            return Ok(());
        }
        let (Some(epoch), Some(rtp)) = (context.path_epoch, context.rtp) else {
            return Err(HistoryError::InvalidCommit);
        };
        if self.active_epoch != Some(epoch)
            || self
                .ssrcs
                .get(&rtp.ssrc)
                .is_some_and(|source| source.terminal)
            || context.wire_len == 0
            || !matches!(
                (self.mode, rtp.twcc_sequence),
                (Some(PacketFeedbackKind::TransportWide), Some(_))
                    | (Some(PacketFeedbackKind::Rfc8888), None)
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

    pub(crate) fn retire_source(&mut self, ssrc: u32) {
        let Some(source) = self.ssrcs.get_mut(&ssrc) else {
            return;
        };
        if source.terminal {
            return;
        }
        let previous = source.readiness(ssrc);
        source.terminal = true;
        let next = source.readiness(ssrc);
        if previous != next {
            self.source_readiness.remove(&previous);
            self.source_readiness.insert(next);
        }
    }

    fn reclaim_sources(&mut self, now: Instant, limit: usize) -> usize {
        let mut work = 0;
        while work < limit && self.loss_work < MAX_EXPIRATIONS_PER_POLL {
            let Some(SourceReadiness::Retired { deadline, ssrc }) =
                self.source_readiness.first().copied()
            else {
                break;
            };
            if deadline.is_some_and(|deadline| now < deadline) {
                break;
            }
            self.source_readiness.pop_first();
            if let Some(source) = self.ssrcs.remove(&ssrc) {
                self.source_backing_bytes -=
                    SOURCE_PAYLOAD_BYTES + source.index_capacity * SOURCE_INDEX_ENTRY_BYTES;
            }
            work += 1;
            self.loss_work += 1;
        }
        work
    }

    pub(crate) fn path_changed(&mut self, epoch: PathEpoch, available: bool) {
        if self.last_path == Some((epoch, available))
            || (!available && self.last_path.is_some_and(|(_, previous)| !previous))
        {
            return;
        }
        self.last_path = Some((epoch, available));
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
        self.inputs.synthetic.clear();
        self.inputs.timing = None;
        self.inputs.fresh_network_feedback = false;
        self.inputs.path_change = Some(PathChange { epoch, available });
        self.twcc_reference = None;
        self.twcc_feedback_count = None;
        self.highest_twcc_acked = None;
        self.oldest_twcc = None;
        // Keep the connection-wide unwrap anchor across a route change, including
        // a 16-bit rollover. The cleared path floor fences all earlier receipts.
        self.missing.clear();
        self.reordering_window = INITIAL_REORDERING_WINDOW;
        self.observed_reordering_window = INITIAL_REORDERING_WINDOW;
        for ssrc in self.ssrcs.values_mut() {
            ssrc.last_report_timestamp = None;
            ssrc.highest_acked_sequence = None;
            ssrc.by_sequence.clear();
        }
    }

    fn remove_indexes(&mut self, entry: SentEntry) {
        if let Some(sequence) = entry.twcc_sequence {
            let slot = ring_index(sequence);
            if self.twcc_index[slot].is_some_and(|index| index.sent_id == entry.id) {
                self.twcc_index[slot] = None;
            }
            if self.oldest_twcc == Some(sequence) {
                let newest = self.newest_twcc.unwrap_or(sequence);
                self.oldest_twcc = ((sequence.saturating_add(1))..=newest)
                    .find(|&candidate| self.twcc_sent_id(candidate).is_some());
            }
        }
        if let Some(state) = self.ssrcs.get_mut(&entry.ssrc)
            && state.by_sequence.get(&entry.rtp_sequence) == Some(&entry.id)
        {
            state.by_sequence.remove(&entry.rtp_sequence);
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

    #[cfg(test)]
    pub(crate) fn test_active_epoch(&self) -> Option<PathEpoch> {
        self.active_epoch
    }

    pub(crate) const fn counters(&self) -> HistoryCounters {
        self.counters
    }

    pub(crate) const fn controller_inputs(&self) -> &ControllerInputs {
        &self.inputs
    }

    pub(crate) fn clear_controller_inputs(&mut self) {
        self.loss_work = 0;
        self.inputs.feedback.clear();
        self.inputs.synthetic.clear();
        self.inputs.timing = None;
        self.inputs.fresh_network_feedback = false;
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

impl SentHistory {
    pub(crate) fn commit_preflighted(&mut self, context: TransmitCommitContext) {
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
