#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::manual_find, clippy::manual_flatten)]

use std::collections::VecDeque;

use crate::entity::TrackId;
use crate::id::ShardId;
use crate::keys::{ParticipantKey, TrackKey};
use crate::plan::PlanBatch;
use crate::route::{RouteAction, RouteHandle, TransportHandle};
use pulsebeam_runtime::mailbox;
use str0m::channel::ChannelId;
use str0m::media::Rid;

#[derive(Debug, Default)]
pub(crate) struct ShardView {
    pub shard: ShardId,
    pub generation: u64,
    pub routes: RouteImage,
    pub transports: TransportImage,
}

#[derive(Debug, Clone)]
pub(crate) struct TrackDescriptor {
    pub id: TrackId,
    pub origin_key: ParticipantKey,
    pub participant: Option<ParticipantKey>,
    pub encodings: Vec<Option<Rid>>,
    pub publication: crate::track::Track,
}

#[derive(Debug, Clone)]
pub(crate) enum TrackRuntime {
    Media(TrackDescriptor),
    Data {
        publisher: Option<ParticipantKey>,
        publisher_effect: Option<crate::participant::ParticipantEffect>,
    },
}

#[derive(Debug, Default)]
pub(crate) struct RouteImage {
    slots: Vec<Option<RouteBinding>>,
}

#[derive(Debug, Clone)]
pub(crate) struct RouteBinding {
    pub handle: RouteHandle,
    pub action: RouteAction,
}

impl RouteImage {
    pub fn resolve(&self, handle: RouteHandle) -> Option<&RouteAction> {
        self.resolve_binding(handle).map(|binding| &binding.action)
    }

    pub fn resolve_binding(&self, handle: RouteHandle) -> Option<&RouteBinding> {
        match self.slots.get(handle.route.index()) {
            Some(Some(binding)) if binding.handle == handle => Some(binding),
            _ => None,
        }
    }

    pub(crate) fn install(&mut self, binding: RouteBinding) {
        let idx = binding.handle.route.index();
        if idx >= self.slots.len() {
            self.slots.resize_with(idx.saturating_add(1), || None);
        }
        let Some(slot) = self.slots.get_mut(idx) else {
            debug_assert!(false, "route slot must exist after resize");
            return;
        };
        *slot = Some(binding);
    }

    pub(crate) fn retire(&mut self, handle: RouteHandle) {
        let Some(slot) = self.slots.get_mut(handle.route.index()) else {
            return;
        };
        if slot
            .as_ref()
            .is_some_and(|binding| binding.handle == handle)
        {
            *slot = None;
        }
    }
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

#[derive(Debug, Clone)]
pub(crate) enum ViewOp {
    InstallRoute {
        binding: RouteBinding,
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
    BindTrack {
        participant: ParticipantKey,
        key: TrackKey,
        channel: ChannelId,
        lane: crate::track::DataLane,
    },
}

#[derive(Debug)]
pub(crate) struct GenerationCommit {
    pub shard: ShardId,
    pub generation: u64,
    pub participant_effects: Vec<(ParticipantKey, crate::participant::ParticipantEffect)>,
    pub lifecycle: Vec<ViewOp>,
    pub plans: PlanBatch,
}

pub(crate) type ControlBatch = GenerationCommit;

impl GenerationCommit {
    fn new(shard: ShardId, generation: u64) -> Self {
        Self {
            shard,
            generation,
            participant_effects: Vec::new(),
            lifecycle: Vec::new(),
            plans: PlanBatch::default(),
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

pub(crate) struct ShardViewWriter {
    shard: ShardId,
    tx: mailbox::Sender<Box<GenerationCommit>>,
    staged: Option<Box<GenerationCommit>>,
    backlog: VecDeque<Box<GenerationCommit>>,
    closed: bool,
}

impl ShardViewWriter {
    pub fn stage(&mut self, generation: u64, op: ViewOp) {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(GenerationCommit::new(self.shard, generation)));
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
            .get_or_insert_with(|| Box::new(GenerationCommit::new(self.shard, generation)));
        if commit.generation != generation {
            pulsebeam_runtime::fatal!("a shard generation cannot mix participant effects");
        }
        commit.participant_effects.push((participant, effect));
    }

    pub fn stage_plans(&mut self, generation: u64, plans: PlanBatch) {
        if plans.is_empty() {
            return;
        }
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(GenerationCommit::new(self.shard, generation)));
        if commit.generation != generation {
            pulsebeam_runtime::fatal!("a shard generation cannot mix plan generations");
        }
        commit.plans.changes.extend(plans.changes);
    }

    pub fn abort(&mut self) {
        self.staged = None;
    }

    pub fn has_staged(&self) -> bool {
        self.staged
            .as_ref()
            .is_some_and(|commit| !commit.is_empty())
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

    pub fn flush_backlog(&mut self) -> bool {
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
}

pub(crate) fn new_shard_view(
    shard: ShardId,
) -> (ShardViewWriter, mailbox::Receiver<Box<ControlBatch>>) {
    let (tx, rx) = mailbox::new(crate::shard::worker::SHARD_VIEW_CAPACITY);
    (
        ShardViewWriter {
            shard,
            tx,
            staged: None,
            backlog: VecDeque::new(),
            closed: false,
        },
        rx,
    )
}

impl ViewOp {
    pub(crate) fn is_owned_by(&self, shard: ShardId) -> bool {
        match self {
            Self::InstallRoute { binding } => binding.handle.shard() == shard,
            Self::RetireRoute { handle } => handle.shard() == shard,
            Self::InstallTransport { binding } => binding.handle.shard() == shard,
            Self::RetireTransport { handle } => handle.shard() == shard,
            Self::InsertParticipant { .. }
            | Self::RemoveParticipant { .. }
            | Self::InsertTrackRuntime { .. }
            | Self::RemoveTrackRuntime { .. }
            | Self::BindTrack { .. } => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::TrackKey;

    fn track_plan() -> (TrackKey, PlanBatch) {
        let mut keys = slotmap::SlotMap::<TrackKey, ()>::with_key();
        let key = keys.insert(());
        let mut batch = PlanBatch::default();
        batch.push(crate::plan::PlanChange {
            key,
            create: true,
            remove: false,
            local: crate::plan::MembershipDelta::default(),
            remote: crate::plan::MembershipDelta::default(),
            reverse: crate::plan::ReverseRouteChange::Unchanged,
        });
        (key, batch)
    }

    #[test]
    fn lifecycle_and_plan_are_one_ordered_generation_without_shared_interpretation() {
        let shard = ShardId::new(1);
        let (mut writer, mut rx) = new_shard_view(shard);
        let (_, plans) = track_plan();
        writer.stage(7, ViewOp::InsertParticipant);
        writer.stage_plans(7, plans);
        assert_eq!(writer.publish(), Some(7));

        let commit = rx.try_recv().unwrap();
        assert_eq!(commit.shard, shard);
        assert_eq!(commit.generation, 7);
        assert_eq!(commit.lifecycle.len(), 1);
        assert_eq!(commit.plans.changes.len(), 1);
    }

    #[test]
    fn queued_generations_remain_in_order() {
        let shard = ShardId::new(0);
        let (mut writer, mut rx) = new_shard_view(shard);
        for generation in 1..=(crate::shard::worker::SHARD_VIEW_CAPACITY + 1) as u64 {
            writer.stage(generation, ViewOp::InsertParticipant);
            assert_eq!(writer.publish(), Some(generation));
        }
        for expected in 1..=crate::shard::worker::SHARD_VIEW_CAPACITY as u64 {
            assert_eq!(rx.try_recv().unwrap().generation, expected);
        }
        assert!(writer.flush_backlog());
        assert_eq!(
            rx.try_recv().unwrap().generation,
            (crate::shard::worker::SHARD_VIEW_CAPACITY + 1) as u64
        );
    }

    #[test]
    fn control_batch_validation_requires_new_owned_revision() {
        let shard = ShardId::new(2);
        let valid = GenerationCommit::new(shard, 4);
        assert!(valid.validate_for(shard, 3));
        assert!(!valid.validate_for(shard, 4));
        assert!(!valid.validate_for(ShardId::new(3), 3));
    }
}
