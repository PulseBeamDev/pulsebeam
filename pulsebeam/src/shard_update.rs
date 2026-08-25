#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::manual_find, clippy::manual_flatten)]

use std::collections::{HashSet, VecDeque};

use crate::id::ShardId;
use crate::keys::{ParticipantKey, TrackKey};
use crate::route::{RouteAction, RouteHandle, TransportHandle};
use pulsebeam_runtime::mailbox;
use str0m::media::Rid;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TrackPlan {
    pub local: Vec<ParticipantKey>,
    pub remote: Vec<RouteHandle>,
    pub reverse_route: Option<RouteHandle>,
}

impl TrackPlan {
    pub(crate) fn new(
        local: impl IntoIterator<Item = ParticipantKey>,
        remote: impl IntoIterator<Item = RouteHandle>,
        reverse_route: Option<RouteHandle>,
    ) -> Self {
        Self {
            local: unique(local, "local"),
            remote: unique(remote, "remote"),
            reverse_route,
        }
    }

    pub(crate) fn is_valid(&self) -> bool {
        fn distinct<K: Copy + Eq + std::hash::Hash>(values: &[K]) -> bool {
            let mut seen = HashSet::with_capacity(values.len());
            values.iter().copied().all(|value| seen.insert(value))
        }

        distinct(&self.local) && distinct(&self.remote)
    }
}

fn unique<K>(values: impl IntoIterator<Item = K>, name: &str) -> Vec<K>
where
    K: Copy + Eq + std::hash::Hash + std::fmt::Debug,
{
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        if !seen.insert(value) {
            debug_assert!(false, "a track plan cannot contain duplicate {name} values");
            continue;
        }
        result.push(value);
    }
    result
}

#[derive(Debug, Clone)]
pub(crate) struct TrackPlanUpdate {
    pub key: TrackKey,
    pub plan: Option<TrackPlan>,
}

#[derive(Debug, Clone)]
pub(crate) struct TrackDescriptor {
    pub origin_key: ParticipantKey,
    pub encodings: Vec<Option<Rid>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TrackRuntime {
    pub descriptor: Option<TrackDescriptor>,
    pub publisher: Option<ParticipantKey>,
    pub publisher_effect: Option<crate::participant::ParticipantEffect>,
}

#[derive(Debug, Default)]
pub(crate) struct TransportImage {
    slots: Vec<Option<TransportBinding>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TransportBinding {
    pub handle: TransportHandle,
    pub participant: ParticipantKey,
}

impl TransportImage {
    pub fn resolve(&self, handle: TransportHandle) -> Option<ParticipantKey> {
        match self.slots.get(handle.route.index()) {
            Some(Some(binding)) if binding.handle == handle => Some(binding.participant),
            _ => None,
        }
    }

    pub(crate) fn install(&mut self, binding: TransportBinding) {
        let idx = binding.handle.route.index();
        if idx >= self.slots.len() {
            self.slots.resize_with(idx.saturating_add(1), || None);
        }
        let Some(slot) = self.slots.get_mut(idx) else {
            debug_assert!(false, "transport slot must exist after resize");
            return;
        };
        *slot = Some(binding);
    }

    pub(crate) fn retire(&mut self, handle: TransportHandle) {
        let Some(slot) = self.slots.get_mut(handle.route.index()) else {
            return;
        };
        if slot.is_some_and(|binding| binding.handle == handle) {
            *slot = None;
        }
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "shard updates are control-plane messages and runtime state stays inline"
)]
#[derive(Debug, Clone)]
pub(crate) enum ShardUpdateOp {
    InstallRoute {
        handle: RouteHandle,
        action: RouteAction,
    },
    RetireRoute {
        handle: RouteHandle,
    },
    InstallTransport {
        binding: TransportBinding,
    },
    RetireTransport {
        handle: TransportHandle,
    },
    InsertParticipant,
    RemoveParticipant {
        key: ParticipantKey,
    },
    InsertTrackRuntime {
        key: TrackKey,
        runtime: TrackRuntime,
    },
    RemoveTrackRuntime {
        key: TrackKey,
    },
    Placeholder,
}

#[derive(Debug)]
pub(crate) struct ShardUpdate {
    pub shard: ShardId,
    pub generation: u64,
    pub participant_effects: Vec<(ParticipantKey, crate::participant::ParticipantEffect)>,
    pub lifecycle: Vec<ShardUpdateOp>,
    pub plans: Vec<TrackPlanUpdate>,
}

impl ShardUpdate {
    fn new(shard: ShardId, generation: u64) -> Self {
        Self {
            shard,
            generation,
            participant_effects: Vec::new(),
            lifecycle: Vec::new(),
            plans: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.participant_effects.is_empty() && self.lifecycle.is_empty() && self.plans.is_empty()
    }

    pub(crate) fn validate_for(&self, shard: ShardId, current_revision: u64) -> bool {
        self.shard == shard
            && self.generation > current_revision
            && self.lifecycle.iter().all(|op| op.is_owned_by(shard))
    }
}

pub(crate) struct ShardUpdateWriter {
    shard: ShardId,
    tx: mailbox::Sender<Box<ShardUpdate>>,
    staged: Option<Box<ShardUpdate>>,
    backlog: VecDeque<Box<ShardUpdate>>,
    closed: bool,
}

impl ShardUpdateWriter {
    pub fn stage(&mut self, generation: u64, op: ShardUpdateOp) {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation {
            pulsebeam_runtime::fatal!("a shard generation cannot mix lifecycle generations");
        }
        commit.lifecycle.push(op);
    }

    pub fn stage_participant_effect(
        &mut self,
        generation: u64,
        participant: ParticipantKey,
        effect: crate::participant::ParticipantEffect,
    ) {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation {
            pulsebeam_runtime::fatal!("a shard generation cannot mix participant effects");
        }
        commit.participant_effects.push((participant, effect));
    }

    pub fn stage_plans(&mut self, generation: u64, plans: Vec<TrackPlanUpdate>) {
        if plans.is_empty() {
            return;
        }
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation {
            pulsebeam_runtime::fatal!("a shard generation cannot mix plan generations");
        }
        commit.plans.extend(plans);
    }

    pub fn publish(&mut self) -> Option<u64> {
        let commit = self.staged.take()?;
        if commit.is_empty() {
            return None;
        }
        let generation = commit.generation;
        if self.closed || !self.flush_backlog() {
            self.backlog.push_back(commit);
            return None;
        }
        match self.tx.try_send(commit) {
            Ok(()) => Some(generation),
            Err(mailbox::TrySendError::Full(commit)) => {
                self.backlog.push_back(commit);
                Some(generation)
            }
            Err(mailbox::TrySendError::Closed(commit)) => {
                self.backlog.push_back(commit);
                self.closed = true;
                None
            }
        }
    }

    #[cfg(test)]
    pub fn enqueue(&mut self) -> Option<u64> {
        let commit = self.staged.take()?;
        if commit.is_empty() {
            return None;
        }
        let generation = commit.generation;
        self.backlog.push_back(commit);
        Some(generation)
    }

    pub fn flush_one(&mut self) -> bool {
        if self.closed {
            return false;
        }
        let Some(commit) = self.backlog.pop_front() else {
            return true;
        };
        match self.tx.try_send(commit) {
            Ok(()) => true,
            Err(mailbox::TrySendError::Full(commit)) => {
                self.backlog.push_front(commit);
                false
            }
            Err(mailbox::TrySendError::Closed(commit)) => {
                self.backlog.push_front(commit);
                self.closed = true;
                false
            }
        }
    }

    fn flush_backlog(&mut self) -> bool {
        if self.closed {
            return false;
        }
        while let Some(commit) = self.backlog.pop_front() {
            match self.tx.try_send(commit) {
                Ok(()) => {}
                Err(mailbox::TrySendError::Full(commit)) => {
                    self.backlog.push_front(commit);
                    return false;
                }
                Err(mailbox::TrySendError::Closed(commit)) => {
                    self.backlog.push_front(commit);
                    self.closed = true;
                    return false;
                }
            }
        }
        true
    }

    pub fn has_backlog(&self) -> bool {
        !self.backlog.is_empty()
    }
}

pub(crate) fn new_shard_update(
    shard: ShardId,
) -> (ShardUpdateWriter, mailbox::Receiver<Box<ShardUpdate>>) {
    let (tx, rx) = mailbox::new(crate::shard::worker::SHARD_UPDATE_CAPACITY);
    (
        ShardUpdateWriter {
            shard,
            tx,
            staged: None,
            backlog: VecDeque::new(),
            closed: false,
        },
        rx,
    )
}

impl ShardUpdateOp {
    pub(crate) fn is_owned_by(&self, shard: ShardId) -> bool {
        match self {
            Self::InstallRoute { handle, .. } | Self::RetireRoute { handle } => {
                handle.shard() == shard
            }
            Self::InstallTransport { binding } => binding.handle.shard() == shard,
            Self::RetireTransport { handle } => handle.shard() == shard,
            Self::InsertParticipant { .. }
            | Self::RemoveParticipant { .. }
            | Self::InsertTrackRuntime { .. }
            | Self::RemoveTrackRuntime { .. }
            | Self::Placeholder => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::TrackKey;

    fn track_plan() -> (TrackKey, Vec<TrackPlanUpdate>) {
        let mut keys = slotmap::SlotMap::<TrackKey, ()>::with_key();
        let key = keys.insert(());
        let plans = vec![TrackPlanUpdate {
            key,
            plan: Some(TrackPlan::default()),
        }];
        (key, plans)
    }

    #[test]
    fn lifecycle_and_plan_are_one_ordered_generation_without_shared_interpretation() {
        let shard = ShardId::new(1);
        let (mut writer, mut rx) = new_shard_update(shard);
        let (_, plans) = track_plan();
        writer.stage(7, ShardUpdateOp::InsertParticipant);
        writer.stage_plans(7, plans);
        assert_eq!(writer.publish(), Some(7));

        let commit = rx.try_recv().unwrap();
        assert_eq!(commit.shard, shard);
        assert_eq!(commit.generation, 7);
        assert_eq!(commit.lifecycle.len(), 1);
        assert_eq!(commit.plans.len(), 1);
    }

    #[test]
    fn queued_generations_remain_in_order() {
        let shard = ShardId::new(0);
        let (mut writer, mut rx) = new_shard_update(shard);
        for generation in 1..=(crate::shard::worker::SHARD_UPDATE_CAPACITY + 1) as u64 {
            writer.stage(generation, ShardUpdateOp::InsertParticipant);
            assert_eq!(writer.publish(), Some(generation));
        }
        for expected in 1..=crate::shard::worker::SHARD_UPDATE_CAPACITY as u64 {
            assert_eq!(rx.try_recv().unwrap().generation, expected);
        }
        assert!(writer.flush_one());
        assert_eq!(
            rx.try_recv().unwrap().generation,
            (crate::shard::worker::SHARD_UPDATE_CAPACITY + 1) as u64
        );
    }

    #[test]
    fn one_flush_attempt_never_drains_multiple_generations() {
        let shard = ShardId::new(0);
        let (mut writer, mut rx) = new_shard_update(shard);
        writer.stage(1, ShardUpdateOp::InsertParticipant);
        assert_eq!(writer.enqueue(), Some(1));
        writer.stage(2, ShardUpdateOp::InsertParticipant);
        assert_eq!(writer.enqueue(), Some(2));

        assert!(writer.flush_one());
        assert_eq!(rx.try_recv().unwrap().generation, 1);
        assert!(writer.has_backlog());
        assert!(matches!(rx.try_recv(), Err(mailbox::TryRecvError::Empty)));

        assert!(writer.flush_one());
        assert_eq!(rx.try_recv().unwrap().generation, 2);
        assert!(!writer.has_backlog());
    }

    #[test]
    fn shard_update_validation_requires_new_owned_revision() {
        let shard = ShardId::new(2);
        let valid = ShardUpdate::new(shard, 4);
        assert!(valid.validate_for(shard, 3));
        assert!(!valid.validate_for(shard, 4));
        assert!(!valid.validate_for(ShardId::new(3), 3));
    }
}
