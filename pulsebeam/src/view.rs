#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::manual_find, clippy::manual_flatten)]

use crate::entity::TrackId;
use crate::id::ShardId;
use crate::keys::{
    AudioTrackKey, DownstreamSlotKey, ParticipantKey, TrackKey, TrackRuntimeKey, VideoTrackKey,
};
use crate::route::{RouteAction, RouteHandle, TransportHandle};
use crate::shard::router::{ReliableStreamKey, UnreliableStreamKey};
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
    pub tracks: ForwardingImage<TrackKey, DownstreamSlotKey>,
    pub audio: ForwardingImage<TrackKey, ()>,
    pub unreliable: ForwardingImage<UnreliableStreamKey, ChannelId>,
    pub reliable: ForwardingImage<ReliableStreamKey, ChannelId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteRoutePlan {
    pub handle: RouteHandle,
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
    pub recipients: Vec<(ParticipantKey, D)>,
    pub remote_routes: Vec<RemoteRoutePlan>,
    pub reverse_route: Option<RemoteRoutePlan>,
}

impl<D> Default for ForwardingPlan<D> {
    fn default() -> Self {
        Self {
            recipients: Vec::new(),
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

impl<K: slotmap::Key, D> ForwardingImage<K, D> {
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

    fn install(&mut self, binding: RouteBinding) {
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

    fn retire(&mut self, handle: RouteHandle) {
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

    fn install(&mut self, binding: TransportBinding) {
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

    fn retire(&mut self, handle: TransportHandle) {
        let Some(slot) = self.slots.get_mut(handle.route.index()) else {
            return;
        };
        if slot.is_some_and(|binding| binding.handle == handle) {
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
pub(crate) enum PlanUpdate {
    Video {
        key: VideoTrackKey,
        plan: VideoPlan,
    },
    Audio {
        key: AudioTrackKey,
        plan: AudioPlan,
    },
    Unreliable {
        key: UnreliableStreamKey,
        plan: StreamPlan,
    },
    Reliable {
        key: ReliableStreamKey,
        plan: StreamPlan,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanRemoval {
    Video(VideoTrackKey),
    Audio(AudioTrackKey),
    Unreliable(UnreliableStreamKey),
    Reliable(ReliableStreamKey),
}

#[derive(Debug, Clone)]
pub(crate) enum ViewOp {
    SetPlan {
        update: PlanUpdate,
    },
    RemovePlan {
        target: PlanRemoval,
    },
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
    InsertParticipant {
        key: ParticipantKey,
    },
    RemoveParticipant {
        key: ParticipantKey,
    },
    InsertTrackRuntime {
        key: TrackRuntimeKey,
        descriptor: TrackDescriptor,
    },
    AnnounceTrack {
        publication: Box<crate::track::Track>,
    },
    WithdrawTrack {
        id: TrackId,
        room_id: crate::entity::RoomId,
    },
    RemoveTrackRuntime {
        key: TrackRuntimeKey,
    },
    InsertUnreliableRuntime {
        key: UnreliableStreamKey,
        id: crate::shard::router::DataStreamId,
        publisher: Option<ParticipantKey>,
    },
    RemoveUnreliableRuntime {
        key: UnreliableStreamKey,
    },
    InsertReliableRuntime {
        key: ReliableStreamKey,
        id: crate::shard::router::DataStreamId,
        publisher: Option<ParticipantKey>,
    },
    RemoveReliableRuntime {
        key: ReliableStreamKey,
    },
    /// A participant now consumes this track here. Stated by the control plane
    /// rather than inferred by the shard from successive plan replacements.
    BindSubscribedTrack {
        participant: ParticipantKey,
        track: TrackId,
        fanout: TrackKey,
    },
    UnbindSubscribedTrack {
        participant: ParticipantKey,
        track: TrackId,
        fanout: TrackKey,
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

    pub(crate) fn is_valid_for(&self, shard: ShardId, generation: u64) -> bool {
        self.shard == shard
            && self.generation > generation
            && self.ops.iter().all(|op| op.is_owned_by(shard))
    }

    pub fn apply(self, view: &mut ShardView) {
        if !self.is_valid_for(view.shard, view.generation) {
            debug_assert_eq!(self.shard, view.shard, "delta applied to its owner");
            debug_assert!(
                self.generation > view.generation,
                "view generations are monotonic"
            );
            debug_assert!(
                self.ops.iter().all(|op| op.is_owned_by(view.shard)),
                "a view op must target its owning shard"
            );
            return;
        }
        for op in self.ops {
            match op {
                ViewOp::InstallRoute { binding } => {
                    debug_assert_eq!(binding.handle.shard(), view.shard);
                    view.routes.install(binding);
                }
                ViewOp::RetireRoute { handle } => {
                    debug_assert_eq!(handle.shard(), view.shard);
                    view.routes.retire(handle);
                }
                ViewOp::InstallTransport { binding } => {
                    debug_assert_eq!(binding.handle.shard(), view.shard);
                    view.transports.install(binding);
                }
                ViewOp::RetireTransport { handle } => {
                    debug_assert_eq!(handle.shard(), view.shard);
                    view.transports.retire(handle);
                }
                ViewOp::InsertParticipant { key } => {
                    let _ = key;
                }
                ViewOp::RemoveParticipant { .. }
                | ViewOp::InsertTrackRuntime { .. }
                | ViewOp::RemoveTrackRuntime { .. }
                | ViewOp::AnnounceTrack { .. }
                | ViewOp::WithdrawTrack { .. }
                | ViewOp::InsertUnreliableRuntime { .. }
                | ViewOp::RemoveUnreliableRuntime { .. }
                | ViewOp::InsertReliableRuntime { .. }
                | ViewOp::RemoveReliableRuntime { .. }
                | ViewOp::BindSubscribedTrack { .. }
                | ViewOp::UnbindSubscribedTrack { .. } => {}
                ViewOp::SetPlan { update } => match update {
                    PlanUpdate::Video { key, plan } => view.tracks.upsert(key.raw(), plan),
                    PlanUpdate::Audio { key, plan } => view.audio.upsert(key.raw(), plan),
                    PlanUpdate::Unreliable { key, plan } => view.unreliable.upsert(key, plan),
                    PlanUpdate::Reliable { key, plan } => view.reliable.upsert(key, plan),
                },
                ViewOp::RemovePlan { target } => match target {
                    PlanRemoval::Video(key) => view.tracks.remove(key.raw()),
                    PlanRemoval::Audio(key) => view.audio.remove(key.raw()),
                    PlanRemoval::Unreliable(key) => view.unreliable.remove(key),
                    PlanRemoval::Reliable(key) => view.reliable.remove(key),
                },
            }
        }
        view.generation = self.generation;
    }
}

pub(crate) struct ShardViewWriter {
    shard: ShardId,
    tx: mailbox::Sender<Box<ShardViewDelta>>,
    staged: Option<Box<ShardViewDelta>>,
    backlog: Option<Box<ShardViewDelta>>,
    closed: bool,
}

impl ShardViewWriter {
    pub fn stage(&mut self, generation: u64, op: ViewOp) {
        if !op.is_owned_by(self.shard) {
            pulsebeam_runtime::fatal!("a view op must target its owning shard");
        }
        let delta = self
            .staged
            .get_or_insert_with(|| Box::new(ShardViewDelta::new(self.shard, generation)));
        if delta.generation != generation {
            pulsebeam_runtime::fatal!("a shard view writer cannot mix lifecycle generations");
        }
        delta.ops.push(op);
    }

    pub fn abort(&mut self) {
        self.staged = None;
    }

    pub fn has_staged(&self) -> bool {
        self.staged.as_ref().is_some_and(|delta| !delta.is_empty())
    }

    pub fn publish(&mut self) -> Option<u64> {
        if self.closed {
            if let Some(delta) = self.staged.take()
                && !delta.is_empty()
            {
                self.coalesce(delta);
            }
            return None;
        }
        let delta = self.staged.take()?;
        if delta.is_empty() {
            return None;
        }
        let generation = delta.generation;
        let _ = self.flush_backlog();
        if self.closed {
            self.coalesce(delta);
            return None;
        }
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
            Err(mailbox::TrySendError::Closed(delta)) => {
                self.backlog = Some(delta);
                self.closed = true;
                None
            }
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
        if self.closed {
            return false;
        }
        let Some(delta) = self.backlog.take() else {
            return true;
        };
        match self.tx.try_send(delta) {
            Ok(()) => true,
            Err(mailbox::TrySendError::Closed(delta)) => {
                self.backlog = Some(delta);
                self.closed = true;
                false
            }
            Err(mailbox::TrySendError::Full(delta)) => {
                self.backlog = Some(delta);
                false
            }
        }
    }
}

impl ViewOp {
    pub(crate) fn is_owned_by(&self, shard: ShardId) -> bool {
        match self {
            Self::InstallRoute { binding } => binding.handle.shard() == shard,
            Self::RetireRoute { handle } => handle.shard() == shard,
            Self::InstallTransport { binding } => binding.handle.shard() == shard,
            Self::RetireTransport { handle } => handle.shard() == shard,
            Self::SetPlan { .. }
            | Self::RemovePlan { .. }
            | Self::InsertParticipant { .. }
            | Self::RemoveParticipant { .. }
            | Self::InsertTrackRuntime { .. }
            | Self::RemoveTrackRuntime { .. }
            | Self::AnnounceTrack { .. }
            | Self::WithdrawTrack { .. }
            | Self::InsertUnreliableRuntime { .. }
            | Self::RemoveUnreliableRuntime { .. }
            | Self::InsertReliableRuntime { .. }
            | Self::RemoveReliableRuntime { .. }
            | Self::BindSubscribedTrack { .. }
            | Self::UnbindSubscribedTrack { .. } => true,
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
            closed: false,
        },
        rx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::RouteId;

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
    fn a_closed_view_writer_never_discards_an_undelivered_delta() {
        let (mut writer, rx) = new_shard_view(ShardId::new(0));
        let mut keys = slotmap::SlotMap::<ParticipantKey, ()>::with_key();
        drop(rx);

        for generation in 1..=2 {
            writer.stage(
                generation,
                ViewOp::InsertParticipant {
                    key: keys.insert(()),
                },
            );
            assert_eq!(writer.publish(), None);
        }

        assert!(!writer.flush_backlog());
        assert_eq!(
            writer.backlog.as_ref().map(|delta| delta.ops.len()),
            Some(2)
        );
    }

    #[test]
    fn a_full_view_mailbox_preserves_every_control_operation() {
        let shard = ShardId::new(0);
        let (mut writer, mut rx) = new_shard_view(shard);
        let mut keys = slotmap::SlotMap::<ParticipantKey, ()>::with_key();
        let operation_count = 20_000;

        for generation in 1..=operation_count {
            writer.stage(
                generation as u64,
                ViewOp::InsertParticipant {
                    key: keys.insert(()),
                },
            );
            assert_eq!(writer.publish(), Some(generation as u64));
        }

        let mut received = 0;
        while let Ok(delta) = rx.try_recv() {
            received += delta.ops.len();
        }
        assert!(writer.flush_backlog());
        while let Ok(delta) = rx.try_recv() {
            received += delta.ops.len();
        }
        assert_eq!(received, operation_count);
    }

    #[test]
    fn stale_route_epoch_is_rejected_after_slot_reuse() {
        let route = RouteId::new(ShardId::new(0), 7);
        let mut track_keys = slotmap::SlotMap::<TrackKey, ()>::with_key();
        let key = track_keys.insert(());
        let handle = RouteHandle::new(route, 3);
        let mut image = RouteImage::default();
        image.install(RouteBinding {
            handle,
            action: RouteAction::Video {
                local_track: VideoTrackKey::new(key),
            },
        });
        image.install(RouteBinding {
            handle: RouteHandle::new(route, 4),
            action: RouteAction::Video {
                local_track: VideoTrackKey::new(key),
            },
        });

        assert!(image.resolve(handle).is_none());
        let current = RouteHandle::new(route, 4);
        assert!(image.resolve(current).is_some());
        image.retire(handle);
        assert!(image.resolve(current).is_some());
    }
}
