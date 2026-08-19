#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::manual_find, clippy::manual_flatten)]

use arrayvec::ArrayVec;

use crate::entity::TrackId;
use crate::id::ShardId;
use crate::keys::{DownstreamSlotKey, ParticipantKey, TrackKey};
use crate::route::{RouteAction, RouteId, TransportHandle, TransportRoute};
use crate::shard::router::{DataStreamKey, ReliableStreamKey};
use pulsebeam_runtime::mailbox;
use slotmap::SecondaryMap;
use str0m::channel::ChannelId;
use str0m::media::Rid;

/// A dense index into a shard's group table.
///
/// Dense and integer so the data plane resolves a group by array index rather
/// than by hash. Ids are recycled once a group empties; view ops are ordered
/// per shard, so a retire always precedes the reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct GroupId(pub u32);

/// Local members of each audience, indexed by [`GroupId`].
///
/// One membership change serves every publication whose plan names the group,
/// which is the point: a participant joining a room is one insert here, not a
/// rewrite of every plan in it.
#[derive(Debug)]
pub(crate) struct GroupImage<D> {
    members: Vec<Vec<(ParticipantKey, D)>>,
}

impl<D> Default for GroupImage<D> {
    fn default() -> Self {
        Self {
            members: Vec::new(),
        }
    }
}

impl<D> GroupImage<D> {
    pub fn members(&self, group: GroupId) -> &[(ParticipantKey, D)] {
        self.members
            .get(group.0 as usize)
            .map_or(&[][..], |m| &m[..])
    }

    fn insert(&mut self, group: GroupId, key: ParticipantKey, delivery: D) {
        let idx = group.0 as usize;
        if self.members.len() <= idx {
            self.members.resize_with(idx.saturating_add(1), Vec::new);
        }
        let Some(slot) = self.members.get_mut(idx) else {
            debug_assert!(false, "group slot must exist after resize");
            return;
        };
        if let Some(held) = slot.iter_mut().find(|(held, _)| *held == key) {
            held.1 = delivery;
        } else {
            slot.push((key, delivery));
        }
    }

    fn remove(&mut self, group: GroupId, key: ParticipantKey) {
        let Some(slot) = self.members.get_mut(group.0 as usize) else {
            return;
        };
        slot.retain(|(held, _)| *held != key);
    }
}

#[derive(Debug, Default)]
pub(crate) struct ShardView {
    pub shard: ShardId,
    pub generation: u64,
    pub routes: RouteImage,
    pub transports: TransportImage,
    pub tracks: ForwardingImage<TrackKey, DownstreamSlotKey>,
    pub audio: ForwardingImage<TrackKey, ()>,
    pub audio_groups: GroupImage<()>,
    pub data_groups: GroupImage<ChannelId>,
    pub data: ForwardingImage<DataStreamKey, ChannelId>,
    pub reliable: ForwardingImage<ReliableStreamKey, ChannelId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteRoutePlan {
    pub shard_id: ShardId,
    pub route: RouteId,
    pub epoch: u16,
}

/// Where one stream's packets go on one shard.
///
/// The same shape for video, audio and both data lanes, because it is the same
/// question: which local participants get a copy, which sibling shards get one,
/// and where feedback goes back to. `D` is the destination-local delivery key —
/// a downstream slot for video, an SCTP channel for data, nothing for audio,
/// whose selector picks a slot per packet.
///
/// Deliberately carries no stream identity. The shard resolves this plan
/// *through* a key into its own runtime arena, which already knows the track id
/// and its origin; repeating them here made two sources of truth and a
/// `debug_assert` to catch them diverging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForwardingPlan<D> {
    pub local_subscribers: Vec<(ParticipantKey, D)>,
    /// Audiences this stream reaches whose membership is shared with other
    /// streams, so a member joining or leaving does not rewrite this plan. At
    /// most four, because only four patterns can match one subject.
    pub groups: ArrayVec<GroupId, 4>,
    pub remote_routes: Vec<RemoteRoutePlan>,
    pub reverse_route: Option<RemoteRoutePlan>,
}

impl<D> Default for ForwardingPlan<D> {
    fn default() -> Self {
        Self {
            local_subscribers: Vec::new(),
            groups: ArrayVec::new(),
            remote_routes: Vec::new(),
            reverse_route: None,
        }
    }
}

pub(crate) type VideoPlan = ForwardingPlan<DownstreamSlotKey>;
pub(crate) type AudioPlan = ForwardingPlan<()>;
pub(crate) type StreamPlan = ForwardingPlan<ChannelId>;

#[derive(Debug, Clone)]
pub(crate) struct TrackDescriptor {
    pub id: TrackId,
    pub origin_key: ParticipantKey,
    pub participant: Option<ParticipantKey>,
    pub encodings: Vec<Option<Rid>>,
    pub states: crate::track::TrackStates,
    pub publication: crate::track::Track,
    pub audience: Vec<ParticipantKey>,
}

#[derive(Debug)]
pub(crate) struct ForwardingImage<K: slotmap::Key, D> {
    plans: SecondaryMap<K, ForwardingPlan<D>>,
}

impl<K: slotmap::Key, D> Default for ForwardingImage<K, D> {
    fn default() -> Self {
        Self {
            plans: SecondaryMap::new(),
        }
    }
}

impl<K: slotmap::Key, D: Clone> ForwardingImage<K, D> {
    pub fn resolve(&self, key: K) -> Option<&ForwardingPlan<D>> {
        self.plans.get(key)
    }

    fn upsert(&mut self, key: K, plan: ForwardingPlan<D>) {
        let _ = self.plans.insert(key, plan);
    }

    fn remove(&mut self, key: K) {
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
        plan: VideoPlan,
    },
    RemoveTrackPlan {
        key: TrackKey,
    },
    SetAudioPlan {
        key: TrackKey,
        plan: AudioPlan,
    },
    AudioGroupInsert {
        group: GroupId,
        key: ParticipantKey,
    },
    AudioGroupRemove {
        group: GroupId,
        key: ParticipantKey,
    },
    DataGroupInsert {
        group: GroupId,
        key: ParticipantKey,
        channel: ChannelId,
    },
    DataGroupRemove {
        group: GroupId,
        key: ParticipantKey,
    },
    RemoveAudioPlan {
        key: TrackKey,
    },
    SetDataPlan {
        key: DataStreamKey,
        plan: StreamPlan,
    },
    RemoveDataPlan {
        key: DataStreamKey,
    },
    SetReliablePlan {
        key: ReliableStreamKey,
        plan: StreamPlan,
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
                ViewOp::AudioGroupInsert { group, key } => view.audio_groups.insert(group, key, ()),
                ViewOp::AudioGroupRemove { group, key } => view.audio_groups.remove(group, key),
                ViewOp::DataGroupInsert {
                    group,
                    key,
                    channel,
                } => view.data_groups.insert(group, key, channel),
                ViewOp::DataGroupRemove { group, key } => view.data_groups.remove(group, key),
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
                self.backlog = Some(Self::bound_backlog(delta));
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
            Self::trim_backlog(backlog);
        } else {
            self.backlog = Some(Self::bound_backlog(delta));
        }
    }

    fn bound_backlog(mut delta: Box<ShardViewDelta>) -> Box<ShardViewDelta> {
        Self::trim_backlog(&mut delta);
        delta
    }

    fn trim_backlog(delta: &mut ShardViewDelta) {
        let excess = delta
            .ops
            .len()
            .saturating_sub(crate::shard::worker::SHARD_VIEW_BACKLOG_OP_CAPACITY);
        if excess == 0 {
            return;
        }
        delta.ops.drain(..excess);
        metrics::counter!("view_backlog_shed").increment(excess as u64);
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_routing_counter("view_backlog_shed");
        debug_assert!(delta.ops.len() <= crate::shard::worker::SHARD_VIEW_BACKLOG_OP_CAPACITY);
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

    /// Aborting a generation must leave nothing behind for the next one to publish.
    ///
    /// `transact` unwinds by calling `abort` on every writer, so a staged op that survived would
    /// be published by whichever generation committed next - carrying a route the allocator has
    /// already taken back.
    #[test]
    fn aborting_a_generation_discards_what_it_staged() {
        let shard = ShardId::new(0);
        let (mut writer, mut rx) = new_shard_view(shard);
        let mut keys = slotmap::SlotMap::<ParticipantKey, ()>::with_key();

        writer.stage(
            1,
            ViewOp::InsertParticipant {
                key: keys.insert(()),
            },
        );
        writer.abort();

        assert_eq!(
            writer.publish(),
            None,
            "an aborted generation has nothing to publish"
        );
        assert!(rx.try_recv().is_err(), "and nothing reached the shard");
    }

    /// A published generation arrives whole, tagged with the generation that produced it.
    #[test]
    fn a_published_generation_reaches_the_shard_intact() {
        let shard = ShardId::new(3);
        let (mut writer, mut rx) = new_shard_view(shard);
        let mut keys = slotmap::SlotMap::<ParticipantKey, ()>::with_key();

        writer.stage(
            7,
            ViewOp::InsertParticipant {
                key: keys.insert(()),
            },
        );
        writer.stage(
            7,
            ViewOp::InsertParticipant {
                key: keys.insert(()),
            },
        );
        assert_eq!(writer.publish(), Some(7));

        let delta = rx.try_recv().expect("the delta was sent");
        assert_eq!(delta.shard, shard, "and to its own shard");
        assert_eq!(delta.generation, 7);
        assert_eq!(delta.ops.len(), 2, "with both staged ops");
    }

    /// A shard that has gone away yields no generation, which is what `transact` aborts on.
    ///
    /// Committing anyway would retire the slot of a route the surviving shards still believe in.
    #[test]
    fn publishing_to_a_departed_shard_yields_no_generation() {
        let (mut writer, rx) = new_shard_view(ShardId::new(0));
        let mut keys = slotmap::SlotMap::<ParticipantKey, ()>::with_key();
        drop(rx);

        writer.stage(
            1,
            ViewOp::InsertParticipant {
                key: keys.insert(()),
            },
        );
        assert_eq!(
            writer.publish(),
            None,
            "a closed receiver is indistinguishable from an empty generation to the caller, and \
             both mean the same thing: this generation did not land"
        );
    }

    #[test]
    fn stale_route_epoch_is_rejected_after_slot_reuse() {
        let route = RouteId::new(ShardId::new(0), 7);
        let mut track_keys = slotmap::SlotMap::<TrackKey, ()>::with_key();
        let key = track_keys.insert(());
        let mut image = RouteImage::default();
        image.install(
            route,
            RouteBinding {
                epoch: 3,
                action: RouteAction::Video { local_track: key },
            },
        );
        image.install(
            route,
            RouteBinding {
                epoch: 4,
                action: RouteAction::Video { local_track: key },
            },
        );

        assert!(image.resolve(route, 3).is_none());
        assert!(image.resolve(route, 4).is_some());
        image.retire(route, 3);
        assert!(image.resolve(route, 4).is_some());
    }
}
