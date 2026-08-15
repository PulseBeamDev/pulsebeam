#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::manual_find, clippy::manual_flatten)]

use crate::entity::{ParticipantId, TrackId};
use crate::id::ShardId;
use crate::keys::{DownstreamSlotKey, ParticipantKey, TrackKey};
use crate::route::{RouteAction, RouteId, TransportHandle, TransportRoute};
use crate::shard::router::{DataStreamKey, ReliableStreamKey};
use pulsebeam_runtime::mailbox;
use slotmap::SecondaryMap;
use str0m::channel::ChannelId;
use str0m::media::Rid;

#[derive(Debug, Default)]
pub(crate) struct ShardView {
    pub shard: ShardId,
    pub generation: u64,
    pub routes: RouteImage,
    pub transports: TransportImage,
    pub tracks: TrackForwardingImage,
    pub audio: AudioForwardingImage,
    pub data: DataForwardingImage,
    pub reliable: ReliableForwardingImage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteRoutePlan {
    pub shard_id: ShardId,
    pub route: RouteId,
    pub epoch: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackForwardingPlan {
    pub track_id: TrackId,
    pub origin: ParticipantId,
    pub local_subscribers: Vec<(ParticipantKey, DownstreamSlotKey)>,
    pub remote_routes: Vec<RemoteRoutePlan>,
    pub reverse_route: Option<RemoteRoutePlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudioForwardingPlan {
    pub track_id: TrackId,
    pub origin: ParticipantId,
    pub local_subscribers: Vec<ParticipantKey>,
    pub remote_routes: Vec<RemoteRoutePlan>,
    pub reverse_route: Option<RemoteRoutePlan>,
}

#[derive(Debug, Clone)]
pub(crate) struct TrackDescriptor {
    pub id: TrackId,
    pub origin_key: ParticipantKey,
    pub participant: Option<ParticipantKey>,
    pub encodings: Vec<Option<Rid>>,
    pub states: crate::track::TrackStates,
    pub publication: crate::track::Track,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreamForwardingPlan {
    pub local_subscribers: Vec<(ParticipantKey, ChannelId)>,
    pub remote_routes: Vec<RemoteRoutePlan>,
    pub reverse_route: Option<RemoteRoutePlan>,
}

#[derive(Debug, Default)]
pub(crate) struct TrackForwardingImage {
    plans: SecondaryMap<TrackKey, TrackForwardingPlan>,
}

impl TrackForwardingImage {
    pub fn resolve(&self, key: TrackKey) -> Option<&TrackForwardingPlan> {
        self.plans.get(key)
    }

    fn upsert(&mut self, key: TrackKey, plan: TrackForwardingPlan) {
        let _ = self.plans.insert(key, plan);
    }

    fn remove(&mut self, key: TrackKey) {
        let _ = self.plans.remove(key);
    }
}

#[derive(Debug, Default)]
pub(crate) struct AudioForwardingImage {
    plans: SecondaryMap<TrackKey, AudioForwardingPlan>,
}

impl AudioForwardingImage {
    pub fn resolve(&self, key: TrackKey) -> Option<&AudioForwardingPlan> {
        self.plans.get(key)
    }

    fn upsert(&mut self, key: TrackKey, plan: AudioForwardingPlan) {
        let _ = self.plans.insert(key, plan);
    }

    fn remove(&mut self, key: TrackKey) {
        let _ = self.plans.remove(key);
    }
}

#[derive(Debug, Default)]
pub(crate) struct DataForwardingImage {
    plans: SecondaryMap<DataStreamKey, StreamForwardingPlan>,
}

impl DataForwardingImage {
    pub fn resolve(&self, key: DataStreamKey) -> Option<&StreamForwardingPlan> {
        self.plans.get(key)
    }

    fn upsert(&mut self, key: DataStreamKey, plan: StreamForwardingPlan) {
        let _ = self.plans.insert(key, plan);
    }

    fn remove(&mut self, key: DataStreamKey) {
        let _ = self.plans.remove(key);
    }
}

#[derive(Debug, Default)]
pub(crate) struct ReliableForwardingImage {
    plans: SecondaryMap<ReliableStreamKey, StreamForwardingPlan>,
}

impl ReliableForwardingImage {
    pub fn resolve(&self, key: ReliableStreamKey) -> Option<&StreamForwardingPlan> {
        self.plans.get(key)
    }

    fn upsert(&mut self, key: ReliableStreamKey, plan: StreamForwardingPlan) {
        let _ = self.plans.insert(key, plan);
    }

    fn remove(&mut self, key: ReliableStreamKey) {
        let _ = self.plans.remove(key);
    }
}

#[derive(Debug, Default)]
pub(crate) struct RouteImage {
    slots: Vec<Option<RouteBinding>>,
}

#[derive(Debug, Clone)]
pub(crate) struct RouteBinding {
    pub epoch: u16,
    pub action: RouteAction,
}

impl RouteImage {
    pub fn resolve(&self, route: RouteId, epoch: u16) -> Option<&RouteAction> {
        self.resolve_binding(route, epoch)
            .map(|binding| &binding.action)
    }

    pub fn resolve_binding(&self, route: RouteId, epoch: u16) -> Option<&RouteBinding> {
        match self.slots.get(route.index()) {
            Some(Some(binding)) if binding.epoch == epoch => Some(binding),
            _ => None,
        }
    }

    fn install(&mut self, route: RouteId, binding: RouteBinding) {
        let idx = route.index();
        if idx >= self.slots.len() {
            self.slots.resize_with(idx.saturating_add(1), || None);
        }
        let Some(slot) = self.slots.get_mut(idx) else {
            debug_assert!(false, "route slot must exist after resize");
            return;
        };
        *slot = Some(binding);
    }

    fn retire(&mut self, route: RouteId, epoch: u16) {
        let Some(slot) = self.slots.get_mut(route.index()) else {
            return;
        };
        if slot.as_ref().is_some_and(|binding| binding.epoch == epoch) {
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
    pub epoch: u16,
    pub participant: ParticipantKey,
}

impl TransportImage {
    pub fn resolve(&self, handle: TransportHandle) -> Option<ParticipantKey> {
        match self.slots.get(handle.route.index()) {
            Some(Some(binding)) if binding.epoch == handle.epoch => Some(binding.participant),
            _ => None,
        }
    }

    fn install(&mut self, route: TransportRoute, binding: TransportBinding) {
        let idx = route.index();
        if idx >= self.slots.len() {
            self.slots.resize_with(idx.saturating_add(1), || None);
        }
        let Some(slot) = self.slots.get_mut(idx) else {
            debug_assert!(false, "transport slot must exist after resize");
            return;
        };
        *slot = Some(binding);
    }

    fn retire(&mut self, handle: TransportHandle) {
        let Some(slot) = self.slots.get_mut(handle.route.index()) else {
            return;
        };
        if slot.is_some_and(|binding| binding.epoch == handle.epoch) {
            *slot = None;
        }
    }
}

#[derive(Debug)]
pub(crate) struct ShardViewDelta {
    pub shard: ShardId,
    pub generation: u64,
    pub ops: Vec<ViewOp>,
}

#[derive(Debug, Clone)]
pub(crate) enum ViewOp {
    InstallRoute {
        route: RouteId,
        binding: RouteBinding,
    },
    RetireRoute {
        route: RouteId,
        epoch: u16,
    },
    InstallTransport {
        route: TransportRoute,
        binding: TransportBinding,
    },
    RetireTransport {
        handle: TransportHandle,
    },
    InsertParticipant {
        key: ParticipantKey,
    },
    RemoveParticipant {
        key: ParticipantKey,
    },
    InsertTrackRuntime {
        key: TrackKey,
        descriptor: TrackDescriptor,
    },
    RemoveTrackRuntime {
        key: TrackKey,
    },
    InsertDataRuntime {
        key: DataStreamKey,
        id: crate::shard::router::DataStreamId,
        publisher: ParticipantKey,
    },
    RemoveDataRuntime {
        key: DataStreamKey,
    },
    InsertReliableRuntime {
        key: ReliableStreamKey,
        id: crate::shard::router::DataStreamId,
        publisher: ParticipantKey,
    },
    RemoveReliableRuntime {
        key: ReliableStreamKey,
    },
    SetTrackPlan {
        key: TrackKey,
        plan: TrackForwardingPlan,
    },
    RemoveTrackPlan {
        key: TrackKey,
    },
    SetAudioPlan {
        key: TrackKey,
        plan: AudioForwardingPlan,
    },
    RemoveAudioPlan {
        key: TrackKey,
    },
    SetDataPlan {
        key: DataStreamKey,
        plan: StreamForwardingPlan,
    },
    RemoveDataPlan {
        key: DataStreamKey,
    },
    SetReliablePlan {
        key: ReliableStreamKey,
        plan: StreamForwardingPlan,
    },
    RemoveReliablePlan {
        key: ReliableStreamKey,
    },
}

impl ShardViewDelta {
    pub fn new(shard: ShardId, generation: u64) -> Self {
        Self {
            shard,
            generation,
            ops: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn apply(self, view: &mut ShardView) {
        debug_assert_eq!(self.shard, view.shard, "delta applied to its owner");
        for op in self.ops {
            match op {
                ViewOp::InstallRoute { route, binding } => view.routes.install(route, binding),
                ViewOp::RetireRoute { route, epoch } => view.routes.retire(route, epoch),
                ViewOp::InstallTransport { route, binding } => {
                    view.transports.install(route, binding);
                }
                ViewOp::RetireTransport { handle } => view.transports.retire(handle),
                ViewOp::InsertParticipant { key } => {
                    let _ = key;
                }
                ViewOp::RemoveParticipant { .. }
                | ViewOp::InsertTrackRuntime { .. }
                | ViewOp::RemoveTrackRuntime { .. }
                | ViewOp::InsertDataRuntime { .. }
                | ViewOp::RemoveDataRuntime { .. }
                | ViewOp::InsertReliableRuntime { .. }
                | ViewOp::RemoveReliableRuntime { .. } => {}
                ViewOp::SetTrackPlan { key, plan } => view.tracks.upsert(key, plan),
                ViewOp::RemoveTrackPlan { key } => view.tracks.remove(key),
                ViewOp::SetAudioPlan { key, plan } => view.audio.upsert(key, plan),
                ViewOp::RemoveAudioPlan { key } => view.audio.remove(key),
                ViewOp::SetDataPlan { key, plan } => view.data.upsert(key, plan),
                ViewOp::RemoveDataPlan { key } => view.data.remove(key),
                ViewOp::SetReliablePlan { key, plan } => view.reliable.upsert(key, plan),
                ViewOp::RemoveReliablePlan { key } => view.reliable.remove(key),
            }
        }
        debug_assert!(
            self.generation > view.generation,
            "view generations are monotonic"
        );
        view.generation = self.generation;
    }
}

pub(crate) struct ShardViewWriter {
    shard: ShardId,
    tx: mailbox::Sender<Box<ShardViewDelta>>,
    staged: Option<Box<ShardViewDelta>>,
    backlog: Option<Box<ShardViewDelta>>,
}

impl ShardViewWriter {
    pub fn stage(&mut self, generation: u64, op: ViewOp) {
        let delta = self
            .staged
            .get_or_insert_with(|| Box::new(ShardViewDelta::new(self.shard, generation)));
        debug_assert_eq!(
            delta.generation, generation,
            "a writer stages one generation"
        );
        delta.ops.push(op);
    }

    pub fn abort(&mut self) {
        self.staged = None;
    }

    pub fn publish(&mut self) -> Option<u64> {
        let delta = self.staged.take()?;
        if delta.is_empty() {
            return None;
        }
        let generation = delta.generation;
        self.flush_backlog();
        if self.backlog.is_some() {
            self.coalesce(delta);
            return Some(generation);
        }
        match self.tx.try_send(delta) {
            Ok(()) => Some(generation),
            Err(mailbox::TrySendError::Full(delta)) => {
                self.backlog = Some(delta);
                Some(generation)
            }
            Err(mailbox::TrySendError::Closed(_)) => None,
        }
    }

    fn coalesce(&mut self, delta: Box<ShardViewDelta>) {
        if let Some(backlog) = self.backlog.as_mut() {
            debug_assert_eq!(backlog.shard, self.shard);
            backlog.ops.extend(delta.ops);
            backlog.generation = delta.generation;
        } else {
            self.backlog = Some(delta);
        }
    }

    pub fn flush_backlog(&mut self) -> bool {
        let Some(delta) = self.backlog.take() else {
            return true;
        };
        match self.tx.try_send(delta) {
            Ok(()) | Err(mailbox::TrySendError::Closed(_)) => true,
            Err(mailbox::TrySendError::Full(delta)) => {
                self.backlog = Some(delta);
                false
            }
        }
    }
}

pub(crate) fn new_shard_view(
    shard: ShardId,
) -> (ShardViewWriter, mailbox::Receiver<Box<ShardViewDelta>>) {
    let (tx, rx) = mailbox::new(crate::shard::worker::SHARD_VIEW_CAPACITY);
    (
        ShardViewWriter {
            shard,
            tx,
            staged: None,
            backlog: None,
        },
        rx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delta_preserves_owner_and_generation() {
        let shard = ShardId::new(2);
        let delta = ShardViewDelta::new(shard, 1);
        assert_eq!(delta.shard, shard);
        assert_eq!(delta.generation, 1);
    }

    #[test]
    fn an_empty_generation_is_not_published() {
        let shard = ShardId::new(0);
        let (mut writer, _rx) = new_shard_view(shard);
        assert_eq!(writer.publish(), None);
    }
}
