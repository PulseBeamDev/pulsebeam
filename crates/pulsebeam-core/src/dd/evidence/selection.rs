//! Filtered-branch decisions. This state does not own packet storage or infer delivery.

use std::collections::BTreeMap;

use super::{FRAME_HISTORY, FrameStatus, Qualified, SEQUENCE_HISTORY, ancestry::Ancestry};
use crate::dd::DecodeTargetIndication;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeTarget(u8);

impl DecodeTarget {
    pub fn index(self) -> u8 {
        self.0
    }

    pub fn new(index: u8) -> Option<Self> {
        (usize::from(index) < crate::dd::MAX_DECODE_TARGETS).then_some(Self(index))
    }

    fn indication(self, evidence: &Qualified) -> Option<DecodeTargetIndication> {
        if evidence.active_targets & (1_u32 << self.0) == 0 {
            return None;
        }
        evidence
            .descriptor
            .frame_dependencies
            .dtis
            .get(usize::from(self.0))
            .copied()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Enqueue {
    Continue,
    /// Atomically replace pending work only if the first packet can be queued.
    Enter,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    Admitted,
    Entered,
    Refused,
    Omitted,
    AwaitingEntry,
    /// The caller must discard pending work for the old mapping.
    Suspended,
    Fenced,
}

pub struct FilteredBranch {
    target: DecodeTarget,
    pending: Option<DecodeTarget>,
    independent_entry_qualified: bool,
    active: bool,
    highest_evaluated: Option<u64>,
    fence: Option<u64>,
    high_frame: Option<u64>,
    frames: BTreeMap<u64, DecodeTargetIndication>,
    ancestry: Ancestry,
}

impl FilteredBranch {
    /// The binding must guarantee independent entries initialize the negotiated
    /// decoder and that the producer's DD units support filtering for this codec.
    pub fn new(target: DecodeTarget, independent_entry_qualified: bool) -> Self {
        Self {
            target,
            pending: None,
            independent_entry_qualified,
            active: false,
            highest_evaluated: None,
            fence: None,
            high_frame: None,
            frames: BTreeMap::new(),
            ancestry: Ancestry::default(),
        }
    }

    pub fn awaiting_entry(&self) -> bool {
        !self.active
    }

    pub fn selected_target(&self) -> DecodeTarget {
        self.target
    }

    pub fn requested_targets(&self) -> super::TargetSet {
        let current = super::TargetSet::single(self.target);
        self.pending.map_or(current, |pending| {
            current.union(super::TargetSet::single(pending))
        })
    }

    pub fn select(&mut self, target: DecodeTarget) {
        self.pending = (target != self.target).then_some(target);
    }

    pub fn offer(
        &mut self,
        sequence: u64,
        frame: FrameStatus,
        evidence: Option<&Qualified>,
        enqueue: impl FnOnce(Enqueue) -> bool,
    ) -> Decision {
        if self.fence.is_some_and(|fence| sequence <= fence)
            || self
                .highest_evaluated
                .is_some_and(|high| high.saturating_sub(sequence) >= SEQUENCE_HISTORY)
        {
            return Decision::Fenced;
        }
        self.highest_evaluated = Some(
            self.highest_evaluated
                .map_or(sequence, |high| high.max(sequence)),
        );
        let (FrameStatus::Resolved(number), Some(evidence)) = (frame, evidence) else {
            return self.suspend();
        };
        let high = self.high_frame.map_or(number, |high| high.max(number));
        let oldest = high.saturating_sub(FRAME_HISTORY.saturating_sub(1));
        if number < oldest {
            return Decision::Fenced;
        }
        self.high_frame = Some(high);
        while self
            .frames
            .first_key_value()
            .is_some_and(|(&frame, _)| frame < oldest)
        {
            self.frames.pop_first();
        }
        self.ancestry
            .prune(oldest, self.highest_evaluated.unwrap_or(sequence));
        let desired = self.pending.unwrap_or(self.target);
        let independent = self.independent_entry_qualified
            && evidence.descriptor.start_of_frame
            && evidence
                .descriptor
                .frame_dependencies
                .frame_diffs
                .is_empty();
        let fresh = !self.frames.contains_key(&number)
            && number == high
            && self.highest_evaluated == Some(sequence);
        let can_switch = fresh
            && self.active
            && self.pending.is_some()
            && evidence.descriptor.start_of_frame
            && desired.indication(evidence) == Some(DecodeTargetIndication::Switch)
            && self
                .ancestry
                .supports_switch(number, evidence, usize::from(desired.0));
        let entry =
            (fresh && independent && (!self.active || self.pending.is_some())) || can_switch;
        if entry
            && let Some(indication) = desired
                .indication(evidence)
                .filter(|dti| *dti != DecodeTargetIndication::NotPresent)
        {
            if !enqueue(Enqueue::Enter) {
                return Decision::Refused;
            }
            self.fence = sequence.checked_sub(1);
            self.frames.clear();
            if independent {
                self.ancestry = Ancestry::default();
            }
            self.frames.insert(number, indication);
            self.ancestry
                .record(number, sequence, evidence, independent);
            self.target = desired;
            self.pending = None;
            self.active = true;
            return Decision::Entered;
        }
        if !self.active {
            return Decision::AwaitingEntry;
        }
        let Some(indication) = self.target.indication(evidence) else {
            return self.suspend();
        };
        if self
            .frames
            .get(&number)
            .is_some_and(|frozen| *frozen != indication)
        {
            return self.suspend();
        }
        self.frames.entry(number).or_insert(indication);
        if indication == DecodeTargetIndication::NotPresent {
            Decision::Omitted
        } else if enqueue(Enqueue::Continue) {
            self.ancestry
                .record(number, sequence, evidence, independent);
            Decision::Admitted
        } else {
            Decision::Refused
        }
    }

    pub fn lose_context(&mut self) {
        self.suspend();
    }

    fn suspend(&mut self) -> Decision {
        self.fence = self.highest_evaluated;
        self.frames.clear();
        self.ancestry = Ancestry::default();
        if std::mem::replace(&mut self.active, false) {
            Decision::Suspended
        } else {
            Decision::AwaitingEntry
        }
    }
}
