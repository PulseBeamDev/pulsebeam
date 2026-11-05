#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "configured bounded ring indexes and checked credit arithmetic"
)]

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    mem::{size_of, size_of_val},
    time::Duration,
};

use crate::{Ecn as EcnMark, Epoch, PacketId, ReceiverTime, SpaceId};

/// Correlation topology. Continuous receipts can acknowledge an unreported prefix;
/// independent spaces maintain their own receipt edges and native loss evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedbackMode {
    Continuous,
    Independent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LossOrigin {
    Reordering,
    HistoryExpiry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SenderConfig {
    pub capacity: usize,
    pub retention: Duration,
    pub max_expirations_per_poll: usize,
    pub max_ack_reordering: u64,
    pub max_feedback_statuses: usize,
    pub initial_reordering_window: Duration,
    pub max_reordering_window: Duration,
}
impl SenderConfig {
    fn validate(self) -> Result<(), SenderError> {
        if self.capacity == 0
            || self.max_expirations_per_poll < 2
            || self.max_feedback_statuses == 0
            || self.retention.is_zero()
            || self.initial_reordering_window > self.max_reordering_window
            || Storage::bytes(
                self.capacity,
                self.max_feedback_statuses,
                self.max_expirations_per_poll,
            )
            .is_none()
        {
            return Err(SenderError::InvalidCommit);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendIdentity {
    pub space: SpaceId,
    /// Unique within the caller-owned space and epoch.
    pub packet: PacketId,
    /// Optional instance-wide, strictly increasing span-credit alias.
    pub feedback_sequence: Option<PacketId>,
    /// Opaque metadata, returned unchanged with observations.
    pub tag: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendCommit {
    pub at: Duration,
    pub wire_len: usize,
    pub path_epoch: Epoch,
    pub identity: SendIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendStatus {
    pub received: bool,
    pub arrival: Option<ReceiverTime>,
    pub ecn: Option<EcnMark>,
    pub hold: Option<Duration>,
}
#[derive(Clone, Copy, Debug)]
pub struct IndependentReport<'a> {
    pub space: SpaceId,
    pub base: PacketId,
    pub generation: i64,
    pub statuses: &'a [SendStatus],
}
#[derive(Clone, Copy, Debug)]
pub enum SendReport<'a> {
    Continuous {
        base_sequence: PacketId,
        reference_micros: i64,
        generation: i64,
        statuses: &'a [SendStatus],
    },
    Independent {
        reports: &'a [IndependentReport<'a>],
    },
}
#[derive(Clone, Copy, Debug)]
pub struct SendFeedback<'a> {
    pub received_at: Duration,
    pub path_epoch: Epoch,
    pub report: SendReport<'a>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SentPacketId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SentObservation {
    pub sent_id: SentPacketId,
    pub space: SpaceId,
    pub committed_at: Duration,
    pub transport_bytes: u32,
    pub tag: u64,
    pub received: bool,
    pub newly_acked: bool,
    pub lost: bool,
    pub reported_lost_for_the_first_time: bool,
    pub recovered: bool,
    pub observed_at: Option<Duration>,
    pub loss_origin: Option<LossOrigin>,
    pub receiver_arrival: Option<ReceiverTime>,
    pub ecn: Option<EcnMark>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedbackTiming {
    pub received_at: Duration,
    pub feedback_hold: Option<Duration>,
    pub newest_send_age: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AvailabilityChange {
    pub epoch: Epoch,
    pub available: bool,
}

#[derive(Debug, Default)]
pub struct SenderInputs {
    pub feedback: Vec<SentObservation>,
    pub synthetic: Vec<SentObservation>,
    pub exhausted: bool,
    pub timing: Option<FeedbackTiming>,
    pub fresh_network_feedback: bool,
    pub path_change: Option<AvailabilityChange>,
    pub application_limited: bool,
    pub bytes_in_flight: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HistoryCounters {
    pub unknown_feedback: u64,
    pub duplicate_feedback: u64,
    pub stale_feedback: u64,
    pub reordered_feedback: u64,
    pub wrong_path_feedback: u64,
    pub expired: u64,
    pub received: u64,
    pub not_received: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Acknowledgment {
    Pending,
    Missing {
        since: Duration,
        higher_received_at: Option<Duration>,
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
    committed_at: Duration,
    wire_len: usize,
    path_epoch: Epoch,
    space: SpaceId,
    packet_sequence: u64,
    feedback_sequence: Option<u64>,
    tag: u64,
    acknowledgment: Acknowledgment,
    network_loss_reported: bool,
    missing_since: Option<Duration>,
    frozen_reorder_deadline: Option<Duration>,
    in_flight: bool,
}

impl SentEntry {
    fn reorder_deadline(&self, window: Duration) -> Option<Duration> {
        match self.acknowledgment {
            Acknowledgment::Missing {
                higher_received_at: Some(anchor),
                ..
            } => self
                .frozen_reorder_deadline
                .or_else(|| anchor.checked_add(window)),
            _ => None,
        }
    }

    fn readiness(&self, window: Duration) -> Option<Readiness> {
        self.reorder_deadline(window)
            .map(|deadline| Readiness::Missing {
                deadline,
                sent_id: self.id,
            })
    }
}

#[derive(Clone, Copy, Debug)]
struct FeedbackIndex {
    sequence: u64,
    sent_id: SentPacketId,
}

#[derive(Debug)]
struct SpaceHistory {
    first_sequence: u64,
    newest_sequence: u64,
    highest_acked_sequence: Option<u64>,
    by_sequence: HashMap<u64, SentPacketId>,
    // Tombstones can reduce usable capacity without releasing table backing.
    index_capacity: usize,
    last_report_generation: Option<(Epoch, i64)>,
    latest_commit_at: Option<Duration>,
    terminal: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Readiness {
    Retired {
        deadline: Option<Duration>,
        space: SpaceId,
    },
    Unscheduled(SpaceId),
    Missing {
        deadline: Duration,
        sent_id: SentPacketId,
    },
}

impl SpaceHistory {
    fn readiness(&self, space: SpaceId, retention: Duration) -> Readiness {
        if self.terminal {
            match self.latest_commit_at {
                None => {
                    return Readiness::Retired {
                        deadline: None,
                        space,
                    };
                }
                Some(at) => {
                    if let Some(deadline) = at.checked_add(retention) {
                        return Readiness::Retired {
                            deadline: Some(deadline),
                            space,
                        };
                    }
                    // An unrepresentable age deadline cannot establish quiescence.
                }
            }
        }
        Readiness::Unscheduled(space)
    }
}

const SOURCE_PAYLOAD_BYTES: usize =
    size_of::<SpaceId>() + size_of::<SpaceHistory>() + size_of::<Readiness>();
// Each queue slot prepays its optional reorder-deadline key.
const MISSING_SLOT_BYTES: usize = size_of::<SentPacketId>() + size_of::<Readiness>();
const SOURCE_INDEX_ENTRY_BYTES: usize = size_of::<(u64, SentPacketId)>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SenderError {
    Exhausted,
    InvalidCommit,
}

pub struct PreparedProfile<'a> {
    history: &'a mut Sender,
    profile: FeedbackMode,
    storage: Option<Storage>,
}

impl PreparedProfile<'_> {
    pub fn commit(self) {
        if let Some(storage) = self.storage {
            self.history.entries = storage.entries;
            self.history.feedback_index = storage.feedback_index;
            self.history.inputs.feedback = storage.feedback;
            self.history.inputs.synthetic = storage.synthetic;
        }
        self.history.mode = Some(self.profile);
    }
}

struct Storage {
    entries: Box<[Option<SentEntry>]>,
    feedback_index: Box<[Option<FeedbackIndex>]>,
    feedback: Vec<SentObservation>,
    synthetic: Vec<SentObservation>,
}

impl Storage {
    fn bytes(
        capacity: usize,
        feedback_capacity: usize,
        synthetic_capacity: usize,
    ) -> Option<usize> {
        capacity
            .checked_mul(size_of::<Option<SentEntry>>())?
            .checked_add(capacity.checked_mul(size_of::<Option<FeedbackIndex>>())?)?
            .checked_add(
                feedback_capacity
                    .checked_add(synthetic_capacity)?
                    .checked_mul(size_of::<SentObservation>())?,
            )
    }

    fn new(config: SenderConfig, limit: usize) -> Result<Self, SenderError> {
        Self::bytes(
            config.capacity,
            config.max_feedback_statuses,
            config.max_expirations_per_poll,
        )
        .filter(|bytes| *bytes <= limit)
        .ok_or(SenderError::Exhausted)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(config.capacity)
            .map_err(|_| SenderError::Exhausted)?;
        entries.resize(config.capacity, None);
        let mut feedback_index = Vec::new();
        feedback_index
            .try_reserve_exact(config.capacity)
            .map_err(|_| SenderError::Exhausted)?;
        feedback_index.resize(config.capacity, None);
        let mut feedback = Vec::new();
        feedback
            .try_reserve_exact(config.max_feedback_statuses)
            .map_err(|_| SenderError::Exhausted)?;
        let mut synthetic = Vec::new();
        synthetic
            .try_reserve_exact(config.max_expirations_per_poll)
            .map_err(|_| SenderError::Exhausted)?;
        Self::bytes(config.capacity, feedback.capacity(), synthetic.capacity())
            .filter(|bytes| *bytes <= limit)
            .ok_or(SenderError::Exhausted)?;
        Ok(Self {
            entries: entries.into_boxed_slice(),
            feedback_index: feedback_index.into_boxed_slice(),
            feedback,
            synthetic,
        })
    }
}

pub struct Sender {
    config: SenderConfig,
    mode: Option<FeedbackMode>,
    max_backing_bytes: usize,
    entries: Box<[Option<SentEntry>]>,
    feedback_index: Box<[Option<FeedbackIndex>]>,
    spaces: BTreeMap<SpaceId, SpaceHistory>,
    source_backing_bytes: usize,
    readiness: BTreeSet<Readiness>,
    next_sent_id: u64,
    oldest_sent_id: u64,
    oldest_feedback: Option<u64>,
    newest_feedback: Option<u64>,
    highest_feedback_acked: Option<u64>,
    feedback_reference: Option<i64>,
    feedback_generation: Option<i64>,
    active_epoch: Option<Epoch>,
    last_path: Option<(Epoch, bool)>,
    inputs: SenderInputs,
    counters: HistoryCounters,
    bytes_in_flight: u64,
    reordering_window: Duration,
    observed_reordering_window: Duration,
    missing: VecDeque<SentPacketId>,
    loss_work: usize,
}

impl Sender {
    pub fn new(
        config: SenderConfig,
        mode: impl Into<Option<FeedbackMode>>,
        max_backing_bytes: usize,
    ) -> Result<Self, SenderError> {
        config.validate()?;
        let mut history = Self {
            config,
            mode: None,
            max_backing_bytes,
            entries: Box::new([]),
            feedback_index: Box::new([]),
            spaces: BTreeMap::new(),
            source_backing_bytes: 0,
            readiness: BTreeSet::new(),
            next_sent_id: 0,
            oldest_sent_id: 0,
            oldest_feedback: None,
            newest_feedback: None,
            highest_feedback_acked: None,
            feedback_reference: None,
            feedback_generation: None,
            active_epoch: None,
            last_path: None,
            inputs: SenderInputs::default(),
            counters: HistoryCounters::default(),
            bytes_in_flight: 0,
            reordering_window: config.initial_reordering_window,
            observed_reordering_window: config.initial_reordering_window,
            missing: VecDeque::new(),
            loss_work: 0,
        };
        if let Some(profile) = mode.into() {
            history.prepare_feedback_profile(profile)?.commit();
        }
        Ok(history)
    }

    pub fn prepare_feedback_profile(
        &mut self,
        profile: FeedbackMode,
    ) -> Result<PreparedProfile<'_>, SenderError> {
        let storage = match self.mode {
            None => Some(Storage::new(self.config, self.max_backing_bytes)?),
            Some(current) if current == profile => None,
            Some(_) => return Err(SenderError::InvalidCommit),
        };
        Ok(PreparedProfile {
            history: self,
            profile,
            storage,
        })
    }

    fn backing_bytes(&self) -> Option<usize> {
        size_of_val(self.entries.as_ref())
            .checked_add(size_of_val(self.feedback_index.as_ref()))?
            .checked_add(
                self.inputs
                    .feedback
                    .capacity()
                    .checked_add(self.inputs.synthetic.capacity())?
                    .checked_mul(size_of::<SentObservation>())?,
            )?
            .checked_add(self.missing.capacity().checked_mul(MISSING_SLOT_BYTES)?)?
            .checked_add(self.source_backing_bytes)
    }

    #[cfg(test)]
    pub fn assert_storage(&self) -> usize {
        let missing = self
            .missing
            .iter()
            .filter_map(|id| self.entry(*id))
            .filter_map(|entry| {
                let key = entry.readiness(self.reordering_window)?;
                assert!(self.readiness.contains(&key));
                Some(key)
            })
            .count();
        assert!(missing <= self.missing.len());
        assert_eq!(self.readiness.len(), self.spaces.len() + missing);
        let sources = self
            .spaces
            .iter()
            .try_fold(0usize, |bytes, (&space, source)| {
                assert!(
                    self.readiness
                        .contains(&source.readiness(space, self.config.retention))
                );
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
        let stride = MISSING_SLOT_BYTES;
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
        let stride = size_of::<SentObservation>();
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
        replacement.append(&mut self.inputs.feedback);
        self.inputs.feedback = replacement;
        Some(())
    }

    pub fn prepare_commit(&mut self, context: SendCommit) -> Result<(), SenderError> {
        self.validate_commit(context)?;
        let identity = context.identity;
        // Removing an overwritten key may leave a tombstone, not insertion capacity.
        let required = self
            .spaces
            .get(&identity.space)
            .map_or(0, |state| state.by_sequence.len())
            .checked_add(1)
            .ok_or(SenderError::Exhausted)?;
        self.reserve_source_capacity(identity, required)
    }

    fn reserve_source_capacity(
        &mut self,
        identity: SendIdentity,
        required: usize,
    ) -> Result<(), SenderError> {
        let state = self.spaces.get(&identity.space);
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
        let (mut replacement, sources) = reserve().ok_or(SenderError::Exhausted)?;
        let index_capacity = replacement.capacity();
        if let Some(state) = self.spaces.get_mut(&identity.space) {
            replacement.extend(state.by_sequence.drain());
            state.by_sequence = replacement;
            state.index_capacity = index_capacity;
        } else {
            self.spaces.insert(
                identity.space,
                SpaceHistory {
                    first_sequence: identity.packet.0,
                    newest_sequence: identity.packet.0,
                    highest_acked_sequence: None,
                    by_sequence: replacement,
                    index_capacity,
                    last_report_generation: None,
                    latest_commit_at: None,
                    terminal: false,
                },
            );
            self.readiness
                .insert(Readiness::Unscheduled(identity.space));
        }
        self.source_backing_bytes = sources;
        Ok(())
    }

    fn commit_context(&mut self, context: SendCommit) -> Result<(), SenderError> {
        self.preflight_commit(context)?;
        let epoch = context.path_epoch;
        let identity = context.identity;
        let sent_id = SentPacketId(self.next_sent_id);
        let slot = self.slot(sent_id.0);
        if let Some(previous) = self.entries[slot] {
            self.remove_indexes(previous);
        }
        let packet_sequence = identity.packet.0;
        let state = self
            .spaces
            .get_mut(&identity.space)
            .ok_or(SenderError::InvalidCommit)?;
        if state
            .last_report_generation
            .is_some_and(|(path, _)| path != epoch)
        {
            state.last_report_generation = None;
            state.highest_acked_sequence = None;
        }
        if state.by_sequence.is_empty() {
            state.first_sequence = packet_sequence;
            state.newest_sequence = packet_sequence;
        } else {
            state.first_sequence = state.first_sequence.min(packet_sequence);
            state.newest_sequence = state.newest_sequence.max(packet_sequence);
        }
        state.by_sequence.insert(identity.packet.0, sent_id);
        state.latest_commit_at = Some(
            state
                .latest_commit_at
                .map_or(context.at, |at| at.max(context.at)),
        );
        let feedback_sequence = identity.feedback_sequence.map(|packet| {
            let sequence = packet.0;
            self.oldest_feedback.get_or_insert(sequence);
            self.newest_feedback = Some(sequence);
            let index = self.slot(sequence);
            self.feedback_index[index] = Some(FeedbackIndex { sequence, sent_id });
            sequence
        });
        self.entries[slot] = Some(SentEntry {
            id: sent_id,
            committed_at: context.at,
            wire_len: context.wire_len,
            path_epoch: epoch,
            space: identity.space,
            packet_sequence,
            feedback_sequence,
            tag: identity.tag,
            acknowledgment: Acknowledgment::Pending,
            network_loss_reported: false,
            missing_since: None,
            frozen_reorder_deadline: None,
            in_flight: true,
        });
        self.next_sent_id = self
            .next_sent_id
            .checked_add(1)
            .ok_or(SenderError::Exhausted)?;
        self.oldest_sent_id = self.oldest_sent_id.max(
            self.next_sent_id
                .saturating_sub(self.config.capacity as u64),
        );
        self.bytes_in_flight = self
            .bytes_in_flight
            .checked_add(context.wire_len as u64)
            .ok_or(SenderError::Exhausted)?;
        self.inputs.bytes_in_flight = self.bytes_in_flight;
        Ok(())
    }

    pub fn preflight_commit(&self, context: SendCommit) -> Result<(), SenderError> {
        self.validate_commit(context)?;
        if self
            .spaces
            .get(&context.identity.space)
            .is_none_or(|state| state.by_sequence.len() >= state.by_sequence.capacity())
        {
            return Err(SenderError::Exhausted);
        }
        Ok(())
    }

    fn validate_commit(&self, context: SendCommit) -> Result<(), SenderError> {
        let epoch = context.path_epoch;
        let identity = context.identity;
        if self.active_epoch != Some(epoch)
            || self.spaces.get(&identity.space).is_some_and(|source| {
                source.terminal || source.by_sequence.contains_key(&identity.packet.0)
            })
            || context.wire_len == 0
            || u32::try_from(context.wire_len).is_err()
            || !matches!(
                (self.mode, identity.feedback_sequence),
                (Some(FeedbackMode::Continuous), Some(_)) | (Some(FeedbackMode::Independent), None)
            )
            || self.next_sent_id == u64::MAX
            || self
                .bytes_in_flight
                .checked_add(context.wire_len as u64)
                .is_none()
        {
            return Err(SenderError::InvalidCommit);
        }
        if let Some(packet) = identity.feedback_sequence {
            if self
                .newest_feedback
                .is_some_and(|newest| packet.0 <= newest)
            {
                return Err(SenderError::InvalidCommit);
            }
            if self.feedback_index[self.slot(packet.0)]
                .and_then(|index| self.entry(index.sent_id))
                .is_some_and(|entry| {
                    entry.path_epoch == epoch && !entry.acknowledgment.is_terminal()
                })
            {
                return Err(SenderError::Exhausted);
            }
        }
        let slot = self.slot(self.next_sent_id);
        if let Some(previous) = self.entries[slot]
            && !previous.acknowledgment.is_terminal()
            && previous.path_epoch == epoch
        {
            return Err(SenderError::Exhausted);
        }
        Ok(())
    }

    pub fn retire_source(&mut self, space: SpaceId) {
        let Some(source) = self.spaces.get_mut(&space) else {
            return;
        };
        if source.terminal {
            return;
        }
        let previous = source.readiness(space, self.config.retention);
        source.terminal = true;
        let next = source.readiness(space, self.config.retention);
        if previous != next {
            self.readiness.remove(&previous);
            self.readiness.insert(next);
        }
    }

    fn reclaim_sources(&mut self, now: Duration, limit: usize) -> usize {
        let mut work = 0;
        while work < limit && self.loss_work < self.config.max_expirations_per_poll {
            let Some(Readiness::Retired { deadline, space }) = self.readiness.first().copied()
            else {
                break;
            };
            if deadline.is_some_and(|deadline| now < deadline) {
                break;
            }
            self.readiness.pop_first();
            if let Some(source) = self.spaces.remove(&space) {
                self.source_backing_bytes -=
                    SOURCE_PAYLOAD_BYTES + source.index_capacity * SOURCE_INDEX_ENTRY_BYTES;
            }
            work += 1;
            self.loss_work += 1;
        }
        work
    }

    pub fn set_availability(&mut self, epoch: Epoch, available: bool) {
        if self.last_path == Some((epoch, available))
            || (!available && self.last_path.is_some_and(|(_, previous)| !previous))
        {
            return;
        }
        self.last_path = Some((epoch, available));
        self.active_epoch = available.then_some(epoch);
        // Report anchors and sequence-index members belong only to retained packets.
        for entry in self.entries.iter_mut().flatten() {
            if let Some(key) = entry.readiness(self.reordering_window) {
                self.readiness.remove(&key);
            }
            if let Some(source) = self.spaces.get_mut(&entry.space) {
                source.last_report_generation = None;
                source.highest_acked_sequence = None;
                source.by_sequence.remove(&entry.packet_sequence);
            }
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
        // Observations are epoch scoped. Never let feedback emitted before a
        // selected-path replacement reach the controller on the new epoch.
        self.inputs.feedback.clear();
        self.inputs.synthetic.clear();
        self.inputs.timing = None;
        self.inputs.fresh_network_feedback = false;
        self.inputs.path_change = Some(AvailabilityChange { epoch, available });
        self.feedback_reference = None;
        self.feedback_generation = None;
        self.highest_feedback_acked = None;
        self.oldest_feedback = None;
        // Keep the connection-wide unwrap anchor across a route change, including
        // a 16-bit rollover. The cleared path floor fences all earlier receipts.
        self.missing.clear();
        self.reordering_window = self.config.initial_reordering_window;
        self.observed_reordering_window = self.config.initial_reordering_window;
    }

    fn remove_indexes(&mut self, entry: SentEntry) {
        if let Some(key) = entry.readiness(self.reordering_window) {
            self.readiness.remove(&key);
        }
        if let Some(sequence) = entry.feedback_sequence {
            let slot = self.slot(sequence);
            if self.feedback_index[slot].is_some_and(|index| index.sent_id == entry.id) {
                self.feedback_index[slot] = None;
            }
            if self.oldest_feedback == Some(sequence) {
                self.oldest_feedback = self
                    .feedback_index
                    .iter()
                    .flatten()
                    .filter(|index| index.sequence > sequence)
                    .map(|index| index.sequence)
                    .min();
            }
        }
        if let Some(state) = self.spaces.get_mut(&entry.space)
            && state.by_sequence.get(&entry.packet_sequence) == Some(&entry.id)
        {
            state.by_sequence.remove(&entry.packet_sequence);
        }
    }

    fn entry(&self, id: SentPacketId) -> Option<&SentEntry> {
        self.entries[self.slot(id.0)]
            .as_ref()
            .filter(|entry| entry.id == id)
    }

    fn entry_mut(&mut self, id: SentPacketId) -> Option<&mut SentEntry> {
        let slot = self.slot(id.0);
        self.entries[slot].as_mut().filter(|entry| entry.id == id)
    }

    fn add_unknown(&mut self, count: usize) {
        self.counters.unknown_feedback =
            self.counters.unknown_feedback.saturating_add(count as u64);
    }

    pub fn active_epoch(&self) -> Option<Epoch> {
        self.active_epoch
    }

    pub const fn counters(&self) -> HistoryCounters {
        self.counters
    }

    pub const fn observations(&self) -> &SenderInputs {
        &self.inputs
    }

    pub fn clear_observations(&mut self) {
        self.loss_work = 0;
        self.inputs.feedback.clear();
        self.inputs.synthetic.clear();
        self.inputs.timing = None;
        self.inputs.fresh_network_feedback = false;
        self.inputs.path_change = None;
    }

    pub fn set_application_limited(&mut self, application_limited: bool) {
        self.inputs.application_limited = application_limited;
    }
}

impl Sender {
    /// All fallible capacity preparation belongs in `prepare_commit`. Rechecks
    /// identity and funding before mutation; successful submission allocates nothing.
    pub fn commit_preflighted(&mut self, context: SendCommit) -> Result<(), SenderError> {
        self.commit_context(context)
    }
}

impl Sender {
    fn slot(&self, value: u64) -> usize {
        usize::try_from(value % self.config.capacity as u64).unwrap_or_default()
    }
    pub const fn config(&self) -> SenderConfig {
        self.config
    }
    pub const fn newest_feedback_packet(&self) -> Option<PacketId> {
        match self.newest_feedback {
            Some(v) => Some(PacketId(v)),
            None => None,
        }
    }
    pub const fn receiver_reference_micros(&self) -> Option<i64> {
        self.feedback_reference
    }
    pub const fn report_generation(&self) -> Option<i64> {
        self.feedback_generation
    }
    pub fn space_anchor(&self, space: SpaceId) -> Option<(PacketId, Option<i64>)> {
        let state = self.spaces.get(&space)?;
        Some((
            PacketId(state.newest_sequence),
            state
                .last_report_generation
                .filter(|(epoch, _)| Some(*epoch) == self.active_epoch)
                .map(|(_, generation)| generation),
        ))
    }
    pub fn storage_bytes(&self) -> Option<usize> {
        self.backing_bytes()
    }

    /// Drops logical history and space indexes, retaining the funded fixed backing.
    /// The caller owns epoch and packet-id uniqueness across logical lifetimes.
    pub fn clear(&mut self, epoch: Epoch) {
        self.entries.fill(None);
        self.feedback_index.fill(None);
        self.spaces.clear();
        self.source_backing_bytes = 0;
        self.readiness.clear();
        self.missing.clear();
        self.oldest_sent_id = self.next_sent_id;
        self.oldest_feedback = None;
        self.newest_feedback = None;
        self.highest_feedback_acked = None;
        self.feedback_reference = None;
        self.feedback_generation = None;
        self.active_epoch = Some(epoch);
        self.last_path = Some((epoch, true));
        self.bytes_in_flight = 0;
        self.clear_observations();
        self.inputs.bytes_in_flight = 0;
        self.inputs.exhausted = false;
        self.inputs.application_limited = false;
        self.inputs.path_change = Some(AvailabilityChange {
            epoch,
            available: true,
        });
        self.reordering_window = self.config.initial_reordering_window;
        self.observed_reordering_window = self.config.initial_reordering_window;
    }

    /// Releases all backing. A feedback profile must be funded again before sends.
    pub fn abort(&mut self) {
        self.clear(self.active_epoch.unwrap_or(Epoch(0)));
        self.entries = Box::new([]);
        self.feedback_index = Box::new([]);
        self.inputs.feedback = Vec::new();
        self.inputs.synthetic = Vec::new();
        self.missing = VecDeque::new();
        self.mode = None;
        self.active_epoch = None;
        self.last_path = None;
        self.inputs.path_change = None;
    }
}
fn feedback_status_count(report: &SendReport<'_>) -> usize {
    match report {
        SendReport::Continuous { statuses, .. } => statuses.len(),
        SendReport::Independent { reports } => reports
            .iter()
            .fold(0usize, |n, r| n.saturating_add(r.statuses.len())),
    }
}
mod feedback;
#[cfg(test)]
mod tests;
