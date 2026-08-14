use std::collections::VecDeque;

use indexmap::IndexSet;
use pulsebeam_runtime::rand;
use str0m::media::{KeyframeRequestKind, Rid};

use super::control::{ControlPlane, DataStreamId, ParticipantShardMeta};
use super::events::{AudioRtpEvent, ParticipantControlEvent};
use super::participants::ParticipantKey;
use super::plan::{
    DataPlane, DataStreamRoute, ReliableStream, RoomFanout, TrackReverseTarget, TrackRoute,
};
use slotmap::new_key_type;

use crate::clock::WallAnchor;
use crate::entity::{AudioOrigin, ParticipantId, RoomId, TrackId, TrackKind};
use crate::id::{AudioSelectorSlotId, ShardId};
use crate::rtp::{RtpPacket, cache::TrackStreamCache};
use crate::track::{Topic, Track, TrackMeta};
use tokio::time::Instant;

use super::worker::{MediaPayload, Reverse, RouteRequestId, ShardEvent, ShardFrame, Topology};
use crate::route::{
    ImportEffect, MediaEnvelope, RemoteRoute, RouteAction, RouteHandle, ReverseTarget, TransportHandle,
    RouteEnvelope, RouteId, RouteNames,
};

pub(crate) type FastIndexSet<T> = IndexSet<T, ahash::RandomState>;

pub(crate) fn fast_set<T>() -> FastIndexSet<T> {
    IndexSet::with_hasher(ahash::RandomState::default())
}

/// The seam between the data plane and whatever carries it between shards.
///
/// The two lanes are deliberately separate methods, because they become
/// different transports cross-node: media is disposable and packet-rate and
/// becomes a UDP datagram, while semantic control is low-rate and
/// correctness-critical and becomes a reliable gRPC call. Swapping in UDP means
/// reimplementing `send_media` alone.
/// The one way a shard reaches another shard.
///
/// Both methods are best-effort — that is the whole contract of this lane.
/// Anything that must not be dropped goes to the controller instead, which is
/// why there is no third method here.
pub(crate) trait ShardTransport {
    /// Route-addressed payload. Split out from [`Self::send_frame`] only to
    /// keep the per-packet path from building an enum it would immediately
    /// destructure.
    fn send_media(&self, dst: ShardId, env: MediaEnvelope, payload: MediaPayload);

    fn send_frame(&self, dst: ShardId, frame: ShardFrame);
}

pub(crate) trait RoutingContext: ShardTransport {
    fn forward_video_rtp(
        &mut self,
        subscriber: ParticipantKey,
        track_id: TrackId,
        pkt: &RtpPacket,
        cache: Option<&TrackStreamCache>,
    );
    /// Hand a subscriber a track's latest measurements.
    ///
    /// Pushed when the snapshot changes rather than carried on packets: an
    /// allocation pass that lands between a new snapshot and the next arriving
    /// packet would otherwise decide against the previous one.
    fn update_layer_states(
        &mut self,
        subscriber: ParticipantKey,
        track_id: TrackId,
        states: &crate::track::TrackStates,
    );
    fn forward_audio_rtp(
        &mut self,
        subscriber: ParticipantKey,
        slot_idx: AudioSelectorSlotId,
        origin: AudioOrigin,
        pkt: &RtpPacket,
    );
    fn forward_sctp(
        &mut self,
        subscriber: ParticipantKey,
        origin: ParticipantId,
        topic: &Topic,
        pkt: &[u8],
    );
    fn notify_tracks_published(&mut self, participant: ParticipantKey, tracks: &[Track]);
    fn notify_tracks_unpublished(&mut self, participant: ParticipantKey, track_ids: &[TrackId]);
    fn notify_keyframe_request(
        &mut self,
        participant_id: ParticipantId,
        track_id: TrackId,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    );
    fn is_local(&self, id: &ParticipantId) -> bool;

    /// The node's NTP↔`Instant` mapping, for stamping outbound envelopes.
    fn wall(&self) -> &WallAnchor;

    fn forward_reliable_sctp(
        &mut self,
        subscriber: ParticipantKey,
        origin: ParticipantId,
        topic: &Topic,
        frame: &[u8],
    );
    fn deliver_reliable_control(&mut self, publisher: ParticipantId, topic: &Topic, bytes: &[u8]);
}

new_key_type! {
    /// A track's fanout on this shard. The compiled plan's video handle: dense,
    /// `Copy`, and meaningless outside the shard that issued it — which is the
    /// point, since a name that means something everywhere is a name every hop
    /// has to hash.
    pub(crate) struct LocalTrackKey;

    /// A room's fanout on this shard, dense and `Copy` for the same reason.
    pub(crate) struct RoomKey;

    /// A realtime data stream's fanout on this shard, dense and `Copy` for the
    /// same reason.
    pub(crate) struct DataStreamKey;

    /// A reliable data stream's fanout on this shard, dense and `Copy` for the
    /// same reason.
    pub(crate) struct ReliableStreamKey;
}

/// Whether an arriving frame's publisher lives on this shard, decided once by
/// the caller instead of re-derived per packet.
///
/// Every dispatch function that used to compute this itself did so by
/// hashing the publisher's `ParticipantId` into the registry
/// (`ctx.is_local`) — despite every call site already knowing the answer
/// statically: `on_media_frame` only ever dispatches frames that arrived
/// from another shard (always `Remote`), and the local pipeline only ever
/// dispatches this shard's own participants (always `Local`). Passing the
/// fact instead of re-deriving it removes that hash from the forwarding path
/// entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Local,
    Remote,
}

impl Origin {
    fn is_local(self) -> bool {
        matches!(self, Origin::Local)
    }
}

/// Pure pub/sub state for a shard: which participants are in which rooms,
/// which shards subscribe to which tracks, and where remote participants
/// live.
///
/// Split so a dispatch function can only reach what it borrows: [`DataPlane`]
/// (`shard/plan.rs`) is everything a frame touches on the forwarding path;
/// [`ControlPlane`] (`shard/control.rs`) is names-to-keys bookkeeping, read on
/// publish/subscribe/teardown and never per packet.
pub(crate) struct ShardRoutingTable {
    pub data: DataPlane,
    pub control: ControlPlane,
    /// Routes this shard needs and has not been granted yet.
    ///
    /// The shard does not allocate: it builds the local object a route will
    /// point at, states the need, and waits. The `ImportTable` states already
    /// modelled this — `Installing` until `on_installed` — so what changes is
    /// only who fills the gap.
    pending_routes: ahash::HashMap<RouteRequestId, PendingRoute>,
    pending_names: ahash::HashMap<RouteRequestId, RouteNames>,
    outbound: std::collections::VecDeque<RouteWork>,
    next_request: u64,
    /// Stands in for the control plane's allocator in unit tests, so a test
    /// can drive a subscribe to completion without a controller. Production
    /// addresses always come from the control plane.
    #[cfg(test)]
    test_alloc: crate::route::SlotAllocator,
}

/// What the shard should finish once a route it asked for is granted.
#[derive(Debug, Clone)]
enum PendingRoute {
    Video(TrackMeta),
    Audio {
        meta: TrackMeta,
        room_id: RoomId,
    },
    /// The subscriber travels with the request so a refused grant can undo
    /// its membership. `was_empty` is what decides whether a request is made
    /// at all, so leaving a subscriber behind on a failure would make every
    /// later one look like local churn and never ask again.
    Data {
        id: DataStreamId,
        subscriber: Option<ParticipantKey>,
    },
    Reliable(DataStreamId),
    /// The descriptor waits on its reverse route: a track is announced with
    /// somewhere to send keyframe requests, or not announced at all.
    ReverseTrack(Box<Track>),
    ReverseTopic(DataStreamId),
}

/// Route lifecycle work for the control plane, drained by the shard each turn.
#[derive(Debug, Clone)]
pub(crate) enum RouteWork {
    Need {
        request: RouteRequestId,
        action: RouteAction,
    },
    Release {
        handle: RouteHandle,
    },
}

impl ShardRoutingTable {
    /// State a need and record what to do about the answer. Nothing resolves
    /// until the control plane publishes the route into this shard's view.
    fn request_route(
        &mut self,
        action: RouteAction,
        names: RouteNames,
        pending: PendingRoute,
    ) -> RouteRequestId {
        let request = RouteRequestId(self.next_request);
        self.next_request = self.next_request.saturating_add(1);
        self.pending_names.insert(request, names);
        self.pending_routes.insert(request, pending);
        self.outbound.push_back(RouteWork::Need { request, action });
        request
    }

    /// Give up a route. It leaves the published view before its slot can be
    /// reused, so the release goes through the control plane rather than
    /// being applied here.
    fn release_route(&mut self, handle: RouteHandle) {
        self.data.routes.retire(handle);
        self.outbound.push_back(RouteWork::Release { handle });
    }

    pub fn drain_route_work(&mut self) -> impl Iterator<Item = RouteWork> + '_ {
        self.outbound.drain(..)
    }

    /// Run the control plane's half of every outstanding route request, in
    /// one call. Only for unit tests: production grants arrive as commands
    /// after a view publication and a generation barrier.
    #[cfg(test)]
    pub(crate) fn settle_routes(&mut self, now: Instant, wall: &WallAnchor) -> Vec<ShardEvent> {
        let mut events = Vec::new();
        for _ in 0..8 {
            let work: Vec<_> = self.outbound.drain(..).collect();
            if work.is_empty() {
                break;
            }
            for item in work {
                match item {
                    RouteWork::Need { request, .. } => {
                        let granted = self.test_alloc.allocate(now).ok().map(|(slot, epoch)| {
                            RouteHandle::new(RouteId::new(self.data.routes.shard_id(), slot), epoch)
                        });
                        if let Some(ev) = self.on_route_granted(request, granted, wall) {
                            events.push(ev);
                        }
                    }
                    RouteWork::Release { handle } => {
                        self.test_alloc.retire(handle.route.slot(), now);
                    }
                }
            }
        }
        events
    }

    /// The routes this shard currently has accounting for — what a test used
    /// to read off the old route table.
    #[cfg(test)]
    pub(crate) fn live_routes(&self) -> usize {
        self.data.routes.len()
    }

    #[cfg(test)]
    pub(crate) fn route_accounting(
        &self,
        handle: RouteHandle,
    ) -> Option<&crate::route::RouteRuntimeEntry> {
        self.data.routes.entry(handle)
    }

    /// Make every subsequent grant fail, standing in for a control plane that
    /// has run out of address space.
    #[cfg(test)]
    pub(crate) fn starve_routes(&mut self) {
        self.test_alloc = crate::route::SlotAllocator::with_max_slots(
            self.data.routes.shard_id(),
            0,
        );
    }

    #[cfg(test)]
    pub(crate) fn unstarve_routes(&mut self) {
        self.test_alloc = crate::route::SlotAllocator::with_max_slots(
            self.data.routes.shard_id(),
            crate::route::PackedRoute::MAX_SLOT.saturating_add(1),
        );
    }

    /// Apply a grant, completing whatever asked for it.
    ///
    /// A `None` handle means the control plane could not produce a route, so
    /// the import is cancelled and whatever was built for it is released —
    /// the same unwind the inline install used to do on an allocator error.
    pub fn on_route_granted(
        &mut self,
        request: RouteRequestId,
        granted: Option<RouteHandle>,
        wall: &WallAnchor,
    ) -> Option<ShardEvent> {
        let pending = self.pending_routes.remove(&request)?;
        let names = self.pending_names.remove(&request);
        let Some(handle) = granted else {
            self.cancel_pending(pending);
            return None;
        };
        if let Some(names) = names {
            self.data.routes.install(handle, names, wall.ntp());
        }
        self.complete_pending(pending, handle)
    }

    fn cancel_pending(&mut self, pending: PendingRoute) {
        match pending {
            PendingRoute::Video(meta) => {
                self.control.imports.cancel_install(&meta.id);
            }
            PendingRoute::Audio { meta, .. } => {
                self.control.imports.cancel_install(&meta.id);
                self.release_fanout_if_idle(&meta.id);
            }
            PendingRoute::Data { id, subscriber } => {
                self.control.data_imports.cancel_install(&id);
                if let Some(subscriber) = subscriber {
                    self.drop_data_subscriber(
                        id.room_id,
                        id.publisher_id,
                        &id.topic,
                        subscriber,
                    );
                }
            }
            PendingRoute::Reliable(id) => {
                self.control.reliable_imports.cancel_install(&id);
            }
            PendingRoute::ReverseTrack { .. } | PendingRoute::ReverseTopic(_) => {}
        }
    }

    fn complete_pending(
        &mut self,
        pending: PendingRoute,
        handle: RouteHandle,
    ) -> Option<ShardEvent> {
        match pending {
            PendingRoute::Video(track) => {
                self.control
                    .imports
                    .on_installed(&track.id, handle.route, handle.epoch);
                Some(ShardEvent::Relay(Topology::TrackSubscribed {
                    track,
                    route: handle.route,
                    epoch: handle.epoch,
                }))
            }
            PendingRoute::Audio { meta, room_id } => {
                self.control
                    .imports
                    .on_installed(&meta.id, handle.route, handle.epoch);
                if let Some(room) = self.room_mut(&room_id) {
                    room.audio_imports.insert(meta.id);
                }
                Some(ShardEvent::Relay(Topology::TrackSubscribed {
                    track: meta,
                    route: handle.route,
                    epoch: handle.epoch,
                }))
            }
            PendingRoute::Data { id, .. } => {
                self.control
                    .data_imports
                    .on_installed(&id, handle.route, handle.epoch);
                Some(ShardEvent::Relay(Topology::DataTopicSubscribed {
                    room_id: id.room_id,
                    topic: id.topic,
                    publisher: Some(id.publisher_id),
                    route: Some(handle.route),
                    epoch: handle.epoch,
                }))
            }
            PendingRoute::Reliable(id) => {
                self.control
                    .reliable_imports
                    .on_installed(&id, handle.route, handle.epoch);
                Some(ShardEvent::Relay(Topology::ReliableTopicSubscribed {
                    room_id: id.room_id,
                    topic: id.topic,
                    publisher: Some(id.publisher_id),
                    route: Some(handle.route),
                    epoch: handle.epoch,
                }))
            }
            PendingRoute::ReverseTrack(track) => {
                let mut track = *track;
                if let Some(key) = self.control.track_keys.get(&track.meta.id).copied()
                    && let Some(entry) = self.data.tracks.get_mut(key)
                {
                    entry.published = true;
                    entry.reverse_route = Some(handle);
                }
                track.reverse = Some(handle);
                Some(ShardEvent::TrackPublished(track))
            }
            PendingRoute::ReverseTopic(id) => {
                if let Some(key) = self.control.reliable_stream_keys.get(&id).copied()
                    && let Some(entry) = self.data.reliable_streams.get_mut(key)
                {
                    entry.reverse_route = Some(handle);
                }
                Some(ShardEvent::Relay(Topology::ReliableTopicPublished {
                    room_id: id.room_id,
                    publisher: id.publisher_id,
                    topic: id.topic,
                    reverse: Some(handle),
                }))
            }
        }
    }

    /// The fanout for a track name, creating it if this shard has not seen the
    /// track before. Control path only.
    fn fanout_key(&mut self, track_id: TrackId, origin: ParticipantId) -> LocalTrackKey {
        if let Some(&key) = self.control.track_keys.get(&track_id) {
            debug_assert_eq!(
                self.data.tracks.get(key).map(|r| r.origin),
                Some(origin),
                "a track's publisher must not change identity across calls"
            );
            return key;
        }
        let key = self.data.tracks.insert(TrackRoute::new(track_id, origin));
        self.control.track_keys.insert(track_id, key);
        key
    }

    /// The key for a track this shard already knows about.
    pub fn fanout_of(&self, track_id: &TrackId) -> Option<LocalTrackKey> {
        self.control.track_keys.get(track_id).copied()
    }

    /// A published track's own name and encoding order, resolved by its
    /// fanout key rather than carried inline on `ReverseTarget::Track` — the
    /// same reason `TrackRoute::track_id` is never hashed to find this
    /// object in the first place.
    pub(crate) fn track_descriptor(&self, key: LocalTrackKey) -> Option<(TrackId, &[Option<Rid>])> {
        let route = self.data.tracks.get(key)?;
        Some((route.track_id, &route.encodings))
    }

    /// A track's publisher, resolved the same way — off `RouteAction::Audio`
    /// and `RouteAction::Reverse`'s `LocalTrackKey` instead of carried inline.
    pub(crate) fn track_origin(&self, key: LocalTrackKey) -> Option<ParticipantId> {
        self.data.tracks.get(key).map(|route| route.origin)
    }

    /// Drop a track's packet cache once nothing local or remote consumes it,
    /// and release the whole fanout entry once nothing needs it at all.
    ///
    /// The two are different thresholds on purpose: a `TrackStreamCache` is a
    /// 512-slot ring per encoding holding whole packets, real memory that a
    /// departed audience should stop costing immediately — but the ~64-byte
    /// descriptor around it (track id, publisher, reverse target) has to
    /// survive as long as the track is *published* or *addressed*, even with
    /// zero subscribers right now, or a late subscriber and a keyframe
    /// request both find nothing here.
    fn release_fanout_if_idle(&mut self, track_id: &TrackId) {
        let Some(&key) = self.control.track_keys.get(track_id) else {
            return;
        };
        let Some(route) = self.data.tracks.get_mut(key) else {
            self.control.track_keys.remove(track_id);
            return;
        };
        if route.subscribers.is_empty() && route.remote_routes.is_empty() {
            route.cache = None;
        }
        if route.is_unused() {
            self.data.tracks.remove(key);
            self.control.track_keys.remove(track_id);
        }
    }

    pub fn new(shard_id: ShardId) -> Self {
        Self {
            data: DataPlane::new(shard_id),
            control: ControlPlane::new(),
            pending_routes: ahash::HashMap::default(),
            pending_names: ahash::HashMap::default(),
            outbound: std::collections::VecDeque::new(),
            next_request: 0,
            #[cfg(test)]
            test_alloc: crate::route::SlotAllocator::with_max_slots(
                shard_id,
                crate::route::PackedRoute::MAX_SLOT.saturating_add(1),
            ),
        }
    }

    /// The fanout for a room name. Control path only, like [`Self::fanout_of`].
    pub(crate) fn room(&self, room_id: &RoomId) -> Option<&RoomFanout> {
        let key = *self.control.room_keys.get(room_id)?;
        self.data.rooms.get(key)
    }

    fn room_mut(&mut self, room_id: &RoomId) -> Option<&mut RoomFanout> {
        let key = *self.control.room_keys.get(room_id)?;
        self.data.rooms.get_mut(key)
    }

    pub(crate) fn has_room(&self, room_id: &RoomId) -> bool {
        self.control.room_keys.contains_key(room_id)
    }

    /// The room name a `RoomKey` was minted for. The data path's only use of
    /// this is to fill in a value a downstream consumer still wants by name
    /// (`AudioRtpEvent::room_id`) — resolving it stays a key lookup, not a
    /// hash, so it costs nothing an index-addressed path wasn't already
    /// paying.
    pub(crate) fn room_id_of(&self, room: RoomKey) -> Option<RoomId> {
        self.data.rooms.get(room).map(|r| r.room_id)
    }

    /// The fanout for a room name, creating it if this shard has not seen the
    /// room before. Control path only.
    fn room_or_insert(
        &mut self,
        room_id: RoomId,
        rng: &mut impl rand::RngCore,
    ) -> (RoomKey, &mut RoomFanout) {
        let Self { data, control, .. } = self;
        let key = *control
            .room_keys
            .entry(room_id)
            .or_insert_with(|| data.rooms.insert(RoomFanout::new(room_id, rng)));
        let Some(room) = data.rooms.get_mut(key) else {
            pulsebeam_runtime::fatal!("room_keys and rooms are created together")
        };
        (key, room)
    }

    fn remove_room(&mut self, room_id: &RoomId) {
        if let Some(key) = self.control.room_keys.remove(room_id) {
            self.data.rooms.remove(key);
        }
    }

    /// The key for a realtime data stream name, if this shard already knows
    /// it. The one control-path lookup a locally originated frame still pays
    /// — the pipeline event that reaches it carries a name, not a key — to
    /// join the same key-addressed dispatch a cross-shard arrival uses.
    pub(crate) fn data_stream_key(&self, id: &DataStreamId) -> Option<DataStreamKey> {
        self.control.data_stream_keys.get(id).copied()
    }

    pub(crate) fn reliable_stream_key(&self, id: &DataStreamId) -> Option<ReliableStreamKey> {
        self.control.reliable_stream_keys.get(id).copied()
    }

    /// The fanout for a realtime data stream name. Control path only.
    fn data_stream(&self, id: &DataStreamId) -> Option<&DataStreamRoute> {
        let key = *self.control.data_stream_keys.get(id)?;
        self.data.data_streams.get(key)
    }

    fn data_stream_mut(&mut self, id: &DataStreamId) -> Option<&mut DataStreamRoute> {
        let key = *self.control.data_stream_keys.get(id)?;
        self.data.data_streams.get_mut(key)
    }

    /// The fanout for a realtime data stream name, creating it (and
    /// registering it against `room_id`'s bookkeeping set) if this shard has
    /// not seen the stream before. Control path only.
    fn data_stream_or_insert(&mut self, room_id: RoomId, id: DataStreamId) -> &mut DataStreamRoute {
        let key = self.data_stream_key_or_insert(room_id, id);
        let Some(route) = self.data.data_streams.get_mut(key) else {
            pulsebeam_runtime::fatal!("data_stream_keys and data_streams are created together")
        };
        route
    }

    fn data_stream_key_or_insert(&mut self, room_id: RoomId, id: DataStreamId) -> DataStreamKey {
        let Self { data, control, .. } = self;
        let key = {
            let id_for_route = id.clone();
            *control
                .data_stream_keys
                .entry(id)
                .or_insert_with(|| data.data_streams.insert(DataStreamRoute::new(id_for_route)))
        };
        if let Some(&room_key) = control.room_keys.get(&room_id)
            && let Some(room) = data.rooms.get_mut(room_key)
        {
            room.data_stream_keys.insert(key);
        }
        key
    }

    fn remove_data_stream(&mut self, room_id: &RoomId, id: &DataStreamId) {
        let Some(key) = self.control.data_stream_keys.remove(id) else {
            return;
        };
        self.data.data_streams.remove(key);
        if let Some(room) = self.room_mut(room_id) {
            room.data_stream_keys.swap_remove(&key);
        }
    }

    fn release_data_stream_if_unused(&mut self, room_id: &RoomId, id: &DataStreamId) {
        if self.data_stream(id).is_some_and(DataStreamRoute::is_unused) {
            self.remove_data_stream(room_id, id);
        }
    }

    /// Every data stream key this shard knows about in `room_id`. Control
    /// path only — the owning room's own bookkeeping set, copied out so the
    /// caller can resolve each key against `DataPlane::data_streams` without
    /// holding the room borrow open.
    fn room_data_stream_keys(&self, room_id: &RoomId) -> Vec<DataStreamKey> {
        self.room(room_id)
            .map(|room| room.data_stream_keys.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The key for a reliable stream name, creating its arena entry if this
    /// shard has not seen the stream before — as a publisher (the reverse
    /// route) or as a destination (the forward route), whichever comes
    /// first. Control path only — mints and retires alongside the route it
    /// backs, so a key handed to `RouteAction` or `ReverseTarget` always
    /// resolves.
    fn reliable_stream_key_or_insert(
        &mut self,
        room_id: RoomId,
        id: DataStreamId,
    ) -> ReliableStreamKey {
        let Self { data, control, .. } = self;
        // Every caller reaches here only after confirming the room exists
        // (`register_reliable_data_publisher`, `on_remote_reliable_publisher`,
        // and their callers all check first) — resolved once, here, so the
        // new stream entry can carry a dense `RoomKey` instead of hashing
        // `room_id` again on every dispatched frame.
        let Some(&room_key) = control.room_keys.get(&room_id) else {
            pulsebeam_runtime::fatal!(
                "reliable_stream_key_or_insert called for a room that does not exist"
            )
        };
        let key = *control
            .reliable_stream_keys
            .entry(id.clone())
            .or_insert_with(|| {
                data.reliable_streams
                    .insert(ReliableStream::new(id, room_key))
            });
        if let Some(room) = data.rooms.get_mut(room_key) {
            room.reliable_stream_keys.insert(key);
        }
        key
    }

    pub(crate) fn reliable_stream(&self, key: ReliableStreamKey) -> Option<&ReliableStream> {
        self.data.reliable_streams.get(key)
    }

    fn reliable_stream_or_insert(
        &mut self,
        room_id: RoomId,
        id: DataStreamId,
    ) -> &mut ReliableStream {
        let key = self.reliable_stream_key_or_insert(room_id, id);
        let Some(stream) = self.data.reliable_streams.get_mut(key) else {
            pulsebeam_runtime::fatal!(
                "reliable_stream_keys and reliable_streams are created together"
            )
        };
        stream
    }

    fn reliable_stream_mut(&mut self, id: &DataStreamId) -> Option<&mut ReliableStream> {
        let key = *self.control.reliable_stream_keys.get(id)?;
        self.data.reliable_streams.get_mut(key)
    }

    fn remove_reliable_stream(&mut self, room_id: &RoomId, id: &DataStreamId) {
        let Some(key) = self.control.reliable_stream_keys.remove(id) else {
            return;
        };
        self.data.reliable_streams.remove(key);
        if let Some(room) = self.room_mut(room_id) {
            room.reliable_stream_keys.swap_remove(&key);
        }
    }

    fn release_reliable_stream_if_unused(&mut self, room_id: &RoomId, id: &DataStreamId) {
        let Some(&key) = self.control.reliable_stream_keys.get(id) else {
            return;
        };
        if self
            .data
            .reliable_streams
            .get(key)
            .is_some_and(ReliableStream::is_unused)
        {
            self.remove_reliable_stream(room_id, id);
        }
    }

    /// Every reliable stream key this shard knows about in `room_id`. Same
    /// shape as [`Self::room_data_stream_keys`].
    fn room_reliable_stream_keys(&self, room_id: &RoomId) -> Vec<ReliableStreamKey> {
        self.room(room_id)
            .map(|room| room.reliable_stream_keys.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Publishers this shard already serves on `topic` within `room_id`, so a
    /// destination that subscribes after they appeared still gets routes for
    /// them.
    fn published_reliable_on(&self, room_id: &RoomId, topic: &Topic) -> Vec<ParticipantId> {
        self.room_reliable_stream_keys(room_id)
            .into_iter()
            .filter_map(|key| self.data.reliable_streams.get(key))
            .filter(|stream| stream.published && stream.id.topic == *topic)
            .map(|stream| stream.id.publisher_id)
            .collect()
    }

    // -- local room membership -------------------------------------------

    /// Returns the room's compiled key so the caller can hand it to the
    /// participant. A participant that knows its own `RoomKey` never makes
    /// the packet path hash a `RoomId` to find its room.
    pub fn add_local_member(
        &mut self,
        handle: ParticipantKey,
        room_id: RoomId,
        rng: &mut impl rand::RngCore,
    ) -> RoomKey {
        let (key, room) = self.room_or_insert(room_id, rng);
        room.members.insert_unique(handle);
        key
    }

    /// Removes a local participant from its room and evicts its audio
    /// tracks from the room's selector. Cleans up the room entry if it's
    /// now empty of both local and remote members.
    pub fn remove_local_member(
        &mut self,
        participant_id: &ParticipantId,
        removed_handle: ParticipantKey,
        room_id: RoomId,
        audio_track_ids: impl IntoIterator<Item = TrackId>,
        now: Instant,
    ) {
        let stream_keys = self.room_data_stream_keys(&room_id);
        for &key in &stream_keys {
            let Some(route) = self.data.data_streams.get_mut(key) else {
                continue;
            };
            route.local_subscribers.remove_value(&removed_handle);
            // And retire whatever they were publishing. `is_unused` deliberately keeps a route
            // that is still published, so without this the route outlives the publisher - and a
            // reconnect keeps the participant id on purpose, so the returning participant
            // collides with its own stale route. In a debug build that trips the assertion in
            // `register_data_publisher`; in release it leaves a route published by somebody who
            // is not there.
            if route.id.publisher_id == *participant_id {
                route.published = false;
            }
        }
        for &key in &stream_keys {
            let Some(id) = self
                .data
                .data_streams
                .get(key)
                .map(|route| route.id.clone())
            else {
                continue;
            };
            self.release_data_stream_if_unused(&room_id, &id);
        }

        // Same rule on the reliable lane: a reconnect must not leave a route
        // published by somebody who is not there.
        let reliable_stream_keys = self.room_reliable_stream_keys(&room_id);
        for &key in &reliable_stream_keys {
            let Some(stream) = self.data.reliable_streams.get_mut(key) else {
                continue;
            };
            if stream.id.publisher_id == *participant_id {
                stream.published = false;
            }
        }
        for &key in &reliable_stream_keys {
            let Some(id) = self
                .data
                .reliable_streams
                .get(key)
                .map(|stream| stream.id.clone())
            else {
                continue;
            };
            self.release_reliable_stream_if_unused(&room_id, &id);
        }

        let audio_track_keys: Vec<LocalTrackKey> = audio_track_ids
            .into_iter()
            .filter_map(|id| self.fanout_of(&id))
            .collect();
        let Some(room) = self.room_mut(&room_id) else {
            return;
        };
        room.members.remove_value(&removed_handle);
        for subscribers in room.all_publisher_subscriptions.local_by_topic.values_mut() {
            subscribers.remove_value(&removed_handle);
        }
        room.all_publisher_subscriptions
            .local_by_topic
            .retain(|_, subscribers| !subscribers.is_empty());
        room.reliable.remove_participant(removed_handle);
        for key in audio_track_keys {
            room.audio_selector.remove_track((key, None));
        }
        // With nobody left to deliver to, the shard stops being a destination
        // for this room's audio.
        if room.members.is_empty() {
            self.retire_room_audio_routes(room_id, now);
        }
        let Some(room) = self.room(&room_id) else {
            return;
        };
        if room.members.is_empty() && room.remote_shards.is_empty() {
            self.remove_room(&room_id);
        }
    }

    /// Open the reverse path for a track this shard publishes, returning the
    /// handle to stamp on the descriptor the control plane distributes.
    ///
    /// One route per track, not per subscribing shard: every subscriber sends
    /// its requests to the same id, which keeps the reverse direction's share
    /// of the 32-bit space proportional to streams rather than streams x
    /// shards. The encoding list travels into the entry so a frame can name a
    /// layer by index instead of carrying a rid.
    /// Ask for the reverse path a published track needs, and hold the
    /// descriptor until it exists. The announcement carries the handle, so a
    /// subscriber never learns about a track it cannot ask for a keyframe on.
    pub fn open_track_reverse_route(&mut self, track: Track) {
        let key = self.fanout_key(track.meta.id, track.meta.origin);
        let Some(entry) = self.data.tracks.get_mut(key) else {
            pulsebeam_runtime::fatal!("fanout_key returned a key the track table does not hold")
        };
        entry.encodings = track.layers.iter().map(|l| l.rid).collect();
        let names = RouteNames {
            room_id: None,
            origin: track.meta.origin,
            track_id: Some(track.meta.id),
            topic: None,
        };
        self.request_reverse_route(
            ReverseTarget::Track { track: key },
            names,
            PendingRoute::ReverseTrack(Box::new(track)),
        );
    }

    /// The same, for a reliable data topic this shard publishes. Its reverse
    /// traffic is the application's own retransmission protocol.
    pub fn open_topic_reverse_route(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    ) {
        let key = DataStreamId::new(room_id, publisher, topic.clone());
        // Minted before the route is requested, alongside the reverse route
        // it will point at: `RouteAction::Reverse` never resolves to a key
        // this shard hasn't already registered.
        let stream = self.reliable_stream_key_or_insert(room_id, key.clone());
        self.request_reverse_route(
            ReverseTarget::Topic { stream },
            RouteNames {
                room_id: Some(room_id),
                origin: publisher,
                track_id: None,
                topic: Some(topic),
            },
            PendingRoute::ReverseTopic(key),
        );
    }

    fn request_reverse_route(
        &mut self,
        target: ReverseTarget,
        names: RouteNames,
        pending: PendingRoute,
    ) {
        self.request_route(RouteAction::Reverse { target }, names, pending);
    }

    /// Close a track's reverse path when its publisher goes away.
    pub fn close_track_reverse_route(&mut self, track_id: &TrackId, now: Instant) {
        let Some(&key) = self.control.track_keys.get(track_id) else {
            return;
        };
        let Some(entry) = self.data.tracks.get_mut(key) else {
            return;
        };
        entry.published = false;
        let reverse = entry.reverse_route.take();
        if let Some(handle) = reverse {
            self.release_route(handle);
        }
        self.release_fanout_if_idle(track_id);
    }

    pub fn close_topic_reverse_route(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        now: Instant,
    ) {
        let key = DataStreamId::new(room_id, publisher, topic.clone());
        let Some(entry) = self.reliable_stream_mut(&key) else {
            return;
        };
        let reverse = entry.reverse_route.take();
        if let Some(handle) = reverse {
            self.release_route(handle);
        }
    }

    /// The reverse handle this shard opened for a track it publishes.
    #[cfg(test)]
    pub(crate) fn track_reverse_handle(&self, track_id: &TrackId) -> Option<RouteHandle> {
        let key = self.control.track_keys.get(track_id).copied()?;
        self.data.tracks.get(key)?.reverse_route
    }

    /// The reverse handle this shard opened for a topic it publishes, so a
    /// late-arriving subscriber can be told about it.
    pub fn topic_reverse_handle(
        &self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
    ) -> Option<RouteHandle> {
        let key =
            self.reliable_stream_key(&DataStreamId::new(room_id, publisher, topic.clone()))?;
        self.reliable_stream(key)?.reverse_route
    }

    /// Learn where to send acks for a topic another shard publishes.
    pub fn learn_topic_reverse_target(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        reverse: Option<RouteHandle>,
    ) {
        let key = DataStreamId::new(room_id, publisher, topic.clone());
        let Some(entry) = self.reliable_stream_mut(&key) else {
            debug_assert!(false, "learning a reverse target for an unknown stream");
            return;
        };
        entry.reverse_target = reverse;
    }

    /// Resolve an arriving feedback frame to the publisher it is aimed at.
    ///
    /// The epoch check is what makes a recycled slot safe: a request in flight
    /// when a track was unpublished must not land on whatever took its place.
    /// The action arrives already resolved: the caller reads it from the
    /// published view under one guard, so this only turns a target into the
    /// publisher it belongs to.
    pub fn resolve_reverse(
        &self,
        action: RouteAction,
    ) -> Option<(ParticipantId, ReverseTarget)> {
        let target = match action {
            RouteAction::Reverse { target } => target,
            other => {
                debug_assert!(false, "a reverse frame arrived on a {other:?} route");
                return None;
            }
        };
        let origin = match target {
            ReverseTarget::Track { track } => self.data.tracks.get(track).map(|r| r.origin),
            ReverseTarget::Topic { stream } => self
                .data
                .reliable_streams
                .get(stream)
                .map(|s| s.id.publisher_id),
        };
        let Some(origin) = origin else {
            debug_assert!(
                false,
                "a reverse target's key must resolve to its publisher"
            );
            return None;
        };
        Some((origin, target))
    }

    /// Where this shard sends reverse traffic for a track it subscribes to,
    /// and the index it must use to name `rid` — both from the descriptor the
    /// control plane handed it.
    pub fn track_reverse_target(
        &self,
        track_id: &TrackId,
        rid: Option<Rid>,
    ) -> Option<(RouteHandle, u8)> {
        let key = self.fanout_of(track_id)?;
        let target = self.data.tracks.get(key)?.reverse_target.as_ref()?;
        let layer = target.encodings.iter().position(|r| *r == rid)?;
        Some((target.route, u8::try_from(layer).ok()?))
    }

    /// Record the measurements a publisher's shard sent for a track this shard
    /// receives. Wholesale, because a snapshot only means anything intact.
    pub fn apply_stats(
        &mut self,
        fanout: LocalTrackKey,
        stats: crate::track::TrackStates,
        ctx: &mut impl RoutingContext,
    ) {
        let Some(route) = self.data.tracks.get_mut(fanout) else {
            return;
        };
        route.layer_states = stats;
        let track_id = route.track_id;
        for &subscriber in &route.subscribers {
            ctx.update_layer_states(subscriber, track_id, &route.layer_states);
        }
    }

    /// Refresh a locally published track's measurements, and hand back the
    /// destinations that need telling.
    ///
    /// The publisher's shard is the only one that measures; every other shard
    /// learns by message. That is what lets the measurements be a plain value
    /// rather than shared atomics.
    pub fn publish_stats(
        &mut self,
        track_id: TrackId,
        stats: crate::track::TrackStates,
        ctx: &mut impl RoutingContext,
    ) -> Vec<(ShardId, RouteEnvelope)> {
        let Some(&key) = self.control.track_keys.get(&track_id) else {
            return Vec::new();
        };
        let Some(route) = self.data.tracks.get_mut(key) else {
            return Vec::new();
        };
        route.layer_states = stats;
        for &subscriber in &route.subscribers {
            ctx.update_layer_states(subscriber, track_id, &route.layer_states);
        }
        route
            .remote_routes
            .iter()
            .map(|remote| {
                (
                    remote.shard_id,
                    RouteEnvelope::telemetry(RouteHandle {
                        route: remote.route,
                        epoch: remote.epoch,
                    }),
                )
            })
            .collect()
    }

    /// A local participant published a track: register its measurement handles
    /// on the node so any shard that later subscribes can resolve them.
    /// Returns the track's compiled fanout so the caller can bind it to the
    /// publishing participant. RTP arrives at packet rate; resolving the
    /// fanout by name there would be the single hottest hash on the node.
    pub fn publish_local_track(
        &mut self,
        track_id: TrackId,
        origin: ParticipantId,
        states: crate::track::TrackStates,
    ) -> LocalTrackKey {
        let key = self.fanout_key(track_id, origin);
        let Some(entry) = self.data.tracks.get_mut(key) else {
            pulsebeam_runtime::fatal!("fanout_key returned a key the track table does not hold")
        };
        entry.layer_states = states;
        key
    }

    pub fn unpublish_local_track(&mut self, track_id: &TrackId) {
        self.release_fanout_if_idle(track_id);
    }

    // -- remote participant membership (refcounted per room/shard) -------

    /// Idempotent: re-registering a participant with the same (room, shard)
    /// it's already registered under is a no-op and does NOT bump the
    /// refcount. This matters — a duplicate/redelivered register message
    /// must not desync the count from the number of real registrations,
    /// or `unregister` can never bring it back to zero.
    pub fn register_remote_participant(
        &mut self,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard_id: ShardId,
        rng: &mut impl rand::RngCore,
    ) {
        let meta = ParticipantShardMeta { shard_id, room_id };

        if self
            .control
            .participant_shards
            .get(&participant_id)
            .copied()
            == Some(meta)
        {
            return;
        }

        if let Some(previous) = self.control.participant_shards.remove(&participant_id) {
            self.release_remote_count(previous);
        }

        self.control.participant_shards.insert(participant_id, meta);
        let (_, room) = self.room_or_insert(room_id, rng);
        room.insert_remote_shard(shard_id);
        room.increment_remote_participant_count(shard_id);
    }

    pub fn unregister_remote_participant(
        &mut self,
        participant_id: ParticipantId,
        expected: ParticipantShardMeta,
    ) {
        let Some(current) = self
            .control
            .participant_shards
            .get(&participant_id)
            .copied()
        else {
            return;
        };
        if current != expected {
            tracing::warn!(
                %participant_id,
                current_shard = %current.shard_id,
                current_room = %current.room_id,
                expected_shard = %expected.shard_id,
                expected_room = %expected.room_id,
                "ignoring stale remote participant unregister"
            );
            return;
        }
        self.control.participant_shards.remove(&participant_id);
        self.release_remote_count(current);
    }

    fn release_remote_count(&mut self, meta: ParticipantShardMeta) {
        let Some(room) = self.room_mut(&meta.room_id) else {
            return;
        };
        if !room.decrement_remote_participant_count(meta.shard_id) {
            return;
        }

        room.remove_remote_shard(meta.shard_id);
        if room.members.is_empty() && room.remote_shards.is_empty() {
            self.remove_room(&meta.room_id);
        }
    }

    // -- track subscription topology (local subscribers) -----------------

    /// Registers a local subscriber for `track`. Returns a `ShardEvent` iff
    /// this is the *first* subscriber, so the caller can notify the
    /// publisher shard to start forwarding.
    pub fn register_subscriber(
        &mut self,
        handle: ParticipantKey,
        track: TrackMeta,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ShardEvent> {
        // Resolve the publisher's handles from the node rather than waiting for
        // them to be sent: they are ready before any subscribe can happen, so
        // the fanout is never briefly live with no measurements behind it.
        // Measurements arrive by message from the publisher's shard, so a fresh
        // fanout simply starts empty and fills on the next snapshot. Nothing is
        // read out of another shard's memory to seed it.
        let key = self.fanout_key(track.id, track.origin);
        let Some(entry) = self.data.tracks.get_mut(key) else {
            pulsebeam_runtime::fatal!("fanout_key returned a key the track table does not hold")
        };
        if !entry.subscribers.insert(handle) {
            return None;
        }

        // The local fanout object (`TrackRoute`) exists before the route is
        // installed, so an installed route always resolves to something.
        if self.control.imports.subscribe(track.id) != ImportEffect::Install {
            return None;
        }
        self.request_route(
            RouteAction::Video { local_track: key },
            RouteNames {
                room_id: None,
                origin: track.origin,
                track_id: Some(track.id),
                topic: None,
            },
            PendingRoute::Video(track),
        );
        None
    }

    /// Returns a `ShardEvent` iff this was the *last* local subscriber, so
    /// the caller can tell the publisher shard to stop forwarding.
    pub fn unregister_subscriber(
        &mut self,
        handle: ParticipantKey,
        track: TrackMeta,
        now: Instant,
    ) -> Option<ShardEvent> {
        let entry = self
            .data
            .tracks
            .get_mut(self.control.track_keys.get(&track.id).copied()?)?;
        if !entry.subscribers.remove(&handle) {
            return None;
        }

        // Retire the destination-side route only when the last local consumer
        // leaves; everything before that is churn the cluster never sees. The
        // retired incarnation is named in the unsubscribe so the publisher can
        // tell it apart from a resubscription that overtook it.
        let retired = match self.control.imports.unsubscribe(&track.id) {
            ImportEffect::Retire { route, epoch } => {
                self.release_route(RouteHandle::new(route, epoch));
                self.control.imports.on_retired(&track.id);
                Some((route, epoch))
            }
            _ => None,
        };
        let Some((route, epoch)) = retired else {
            self.release_fanout_if_idle(&track.id);
            return None;
        };
        self.release_fanout_if_idle(&track.id);
        Some(ShardEvent::Relay(Topology::TrackUnsubscribed {
            track,
            route,
            epoch,
        }))
    }

    /// Returns the compiled stream so the caller can bind it to the
    /// publisher's channel — that is what keeps the packet path from
    /// reassembling the stream's identity on every frame.
    pub fn register_data_publisher(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    ) -> Option<DataStreamKey> {
        let room = self.room(&room_id)?;
        let all_publisher_subscribers = room
            .all_publisher_subscriptions
            .local_by_topic
            .get(&topic)
            .cloned()
            .unwrap_or_default();
        let id = DataStreamId::new(room_id, publisher, topic);
        let key = self.data_stream_key_or_insert(room_id, id);
        let Some(route) = self.data.data_streams.get_mut(key) else {
            pulsebeam_runtime::fatal!("data_stream_key_or_insert returned an absent key")
        };
        debug_assert!(!route.published);
        route.published = true;
        for subscriber in all_publisher_subscribers {
            route.local_subscribers.insert_unique(subscriber);
        }
        // Remote wildcard subscribers are not attached here: a destination must
        // allocate its own route, so it is announced to and hands a handle back.
        Some(key)
    }

    pub fn unregister_data_publisher(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
    ) {
        let key = DataStreamId::new(room_id, publisher, topic.clone());
        let subscribers = self
            .room(&room_id)
            .and_then(|room| room.all_publisher_subscriptions.local_by_topic.get(topic))
            .cloned()
            .unwrap_or_default();
        let Some(route) = self.data_stream_mut(&key) else {
            debug_assert!(false, "unregistering an unknown data stream");
            return;
        };
        debug_assert!(route.published);
        route.published = false;
        for subscriber in &subscribers {
            route.local_subscribers.remove_value(subscriber);
        }
        self.release_data_stream_if_unused(&room_id, &key);
    }

    pub fn register_data_subscriber(
        &mut self,
        room_id: RoomId,
        handle: ParticipantKey,
        topic: Topic,
        publisher: Option<ParticipantId>,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ShardEvent> {
        self.room(&room_id)?;
        match publisher {
            Some(publisher) => {
                let route = self
                    .data_stream_or_insert(room_id, DataStreamId::new(room_id, publisher, topic.clone()));
                let was_empty = route.local_subscribers.is_empty();
                route.local_subscribers.insert_unique(handle);
                if !was_empty {
                    return None;
                }
                // The local fanout entry exists before the route is
                // requested. The subscription is announced when the control
                // plane grants the route.
                self.install_data_route(room_id, publisher, &topic, Some(handle));
                None
            }
            None => {
                let was_empty = {
                    let room = self.room_mut(&room_id)?;
                    let subscribers = room
                        .all_publisher_subscriptions
                        .local_by_topic
                        .entry(topic.clone())
                        .or_default();
                    let was_empty = subscribers.is_empty();
                    let inserted = subscribers.insert_unique(handle);
                    debug_assert!(inserted);
                    was_empty
                };
                // Every stream already known on this topic, whether published
                // here or imported from another shard. An imported one is only
                // announced to the shard once, when its first wildcard
                // subscriber arrives, so a later subscriber that skipped this
                // would never join the fanout and would receive nothing for the
                // life of the stream. `unregister_data_subscriber` detaches on
                // the same terms.
                for key in self.room_data_stream_keys(&room_id) {
                    let Some(route) = self.data.data_streams.get_mut(key) else {
                        continue;
                    };
                    if route.id.topic == topic {
                        route.local_subscribers.insert_unique(handle);
                    }
                }
                was_empty.then_some(ShardEvent::Relay(Topology::DataTopicSubscribed {
                    room_id,
                    topic,
                    publisher: None,
                    route: None,
                    epoch: 0,
                }))
            }
        }
    }

    pub fn unregister_data_subscriber(
        &mut self,
        room_id: RoomId,
        handle: ParticipantKey,
        topic: &Topic,
        publisher: Option<ParticipantId>,
        now: Instant,
    ) -> bool {
        if self.room(&room_id).is_none() {
            return false;
        }
        // Publishers whose destination route this shard no longer needs. Held
        // until the arena is done being iterated, since retiring touches the
        // route table.
        let mut orphaned: Vec<ParticipantId> = Vec::new();
        let was_one = match publisher {
            Some(publisher) => {
                let key = DataStreamId::new(room_id, publisher, topic.clone());
                let Some(route) = self.data_stream_mut(&key) else {
                    return false;
                };
                let was_one =
                    route.local_subscribers.len() == 1 && route.local_subscribers.contains(&handle);
                route.local_subscribers.remove_value(&handle);
                if route.is_unused() {
                    orphaned.push(publisher);
                    self.remove_data_stream(&room_id, &key);
                }
                was_one
            }
            None => {
                let was_one = {
                    let Some(room) = self.room_mut(&room_id) else {
                        return false;
                    };
                    let Some(subscribers) = room
                        .all_publisher_subscriptions
                        .local_by_topic
                        .get_mut(topic)
                    else {
                        return false;
                    };
                    let was_one = subscribers.len() == 1 && subscribers.contains(&handle);
                    subscribers.remove_value(&handle);
                    if subscribers.is_empty() {
                        room.all_publisher_subscriptions
                            .local_by_topic
                            .remove(topic);
                    }
                    was_one
                };
                // A wildcard resolved into one concrete route per publisher, so
                // dropping it can orphan several at once.
                for key in self.room_data_stream_keys(&room_id) {
                    let Some(route) = self.data.data_streams.get_mut(key) else {
                        continue;
                    };
                    if route.id.topic != *topic {
                        continue;
                    }
                    route.local_subscribers.remove_value(&handle);
                    let unused = route.is_unused();
                    let id = route.id.clone();
                    if unused {
                        orphaned.push(id.publisher_id);
                        self.remove_data_stream(&room_id, &id);
                    }
                }
                was_one
            }
        };

        // Losing the last local consumer is what retires the cluster route —
        // without this the import stays Active and its slot is never reusable,
        // so a later subscription cannot allocate a fresh route and epoch.
        for publisher in orphaned {
            self.retire_data_route(room_id, publisher, topic, now);
        }
        was_one
    }

    /// Record a destination's handle for a concrete stream, or register a
    /// wildcard destination.
    ///
    /// Returns publishers this shard already serves on `topic`, which the
    /// caller announces so a newly-arrived wildcard destination can install
    /// routes for them. Without this, a wildcard subscription that arrives
    /// after the publisher would never receive anything.
    pub fn register_remote_data_subscriber_shard(
        &mut self,
        room_id: RoomId,
        from_shard_id: ShardId,
        topic: Topic,
        publisher: Option<ParticipantId>,
        remote: Option<RemoteRoute>,
    ) -> Vec<ParticipantId> {
        if self.room(&room_id).is_none() {
            return Vec::new();
        }
        match publisher {
            Some(publisher) => {
                let Some(remote) = remote else {
                    debug_assert!(false, "a concrete data subscription needs a route handle");
                    return Vec::new();
                };
                debug_assert_eq!(remote.shard_id, from_shard_id);
                let route =
                    self.data_stream_or_insert(room_id, DataStreamId::new(room_id, publisher, topic));
                route.attach_remote_subscriber_shard(remote);
                Vec::new()
            }
            None => {
                let Some(room) = self.room_mut(&room_id) else {
                    return Vec::new();
                };
                let shards = room
                    .all_publisher_subscriptions
                    .remote_by_topic
                    .entry(topic.clone())
                    .or_default();
                if shards.contains(&from_shard_id) {
                    return Vec::new();
                }
                shards.push(from_shard_id);
                self.room_data_stream_keys(&room_id)
                    .into_iter()
                    .filter_map(|key| self.data.data_streams.get(key))
                    .filter(|route| route.published && route.id.topic == topic)
                    .map(|route| route.id.publisher_id)
                    .collect()
            }
        }
    }

    pub fn unregister_remote_data_subscriber_shard(
        &mut self,
        room_id: RoomId,
        from_shard_id: ShardId,
        topic: &Topic,
        publisher: Option<ParticipantId>,
    ) {
        match publisher {
            Some(publisher) => {
                let key = DataStreamId::new(room_id, publisher, topic.clone());
                let Some(route) = self.data_stream_mut(&key) else {
                    return;
                };
                route.detach_remote_subscriber_shard(from_shard_id);
                self.release_data_stream_if_unused(&room_id, &key);
            }
            None => {
                let Some(room) = self.room_mut(&room_id) else {
                    return;
                };
                let removed = if let Some(shards) = room
                    .all_publisher_subscriptions
                    .remote_by_topic
                    .get_mut(topic)
                {
                    let removed = if let Some(pos) = shards.iter().position(|&s| s == from_shard_id)
                    {
                        shards.swap_remove(pos);
                        true
                    } else {
                        false
                    };
                    if shards.is_empty() {
                        room.all_publisher_subscriptions
                            .remote_by_topic
                            .remove(topic);
                    }
                    removed
                } else {
                    false
                };
                if !removed {
                    return;
                }
                for key in self.room_data_stream_keys(&room_id) {
                    let Some(route) = self.data.data_streams.get_mut(key) else {
                        continue;
                    };
                    if route.published && route.id.topic == *topic {
                        route.detach_remote_subscriber_shard(from_shard_id);
                    }
                }
            }
        }
    }

    // -- track subscription topology (remote shards) ---------------------

    /// Record the destination's acknowledged handle. Idempotent: a redelivered
    /// subscribe must not install a second handle for the same shard, which
    /// would double every frame.
    pub fn register_remote_subscriber_shard(&mut self, remote: RemoteRoute, track: TrackMeta) {
        let key = self.fanout_key(track.id, track.origin);
        let Some(route) = self.data.tracks.get_mut(key) else {
            pulsebeam_runtime::fatal!("fanout_key returned a key the track table does not hold")
        };
        if let Some(existing) = route
            .remote_routes
            .iter_mut()
            .find(|r| r.shard_id == remote.shard_id)
        {
            // A reinstall at the destination supersedes the old incarnation.
            if existing.route != remote.route || existing.epoch != remote.epoch {
                *existing = remote;
            }
            return;
        }
        route.remote_routes.push(remote);
    }

    /// Drop the sender handle for a destination that has retired its route.
    ///
    /// Matches on the route incarnation, not just the shard: an unsubscribe can
    /// be overtaken by a resubscription from the same shard, and removing by
    /// shard alone would tear down the new handle and silently stop forwarding.
    pub fn unregister_remote_subscriber_shard(
        &mut self,
        from_shard_id: ShardId,
        track: TrackMeta,
        route: RouteId,
        epoch: u16,
    ) {
        let Some(entry) = self
            .control
            .track_keys
            .get(&track.id)
            .copied()
            .and_then(|k| self.data.tracks.get_mut(k))
        else {
            return;
        };
        let Some(idx) = entry
            .remote_routes
            .iter()
            .position(|r| r.shard_id == from_shard_id)
        else {
            return;
        };
        let Some(held) = entry.remote_routes.get(idx).copied() else {
            debug_assert!(false, "position() returned an index the list does not hold");
            return;
        };
        if held.route != route || held.epoch != epoch {
            tracing::debug!(
                %from_shard_id,
                held = %held.route,
                stale = %route,
                "ignoring an unsubscribe for a superseded route"
            );
            return;
        }
        entry.remote_routes.swap_remove(idx);
    }

    // -- track publish / unpublish ----------------------------------------

    pub fn publish_track(
        &mut self,
        track: Track,
        room_id: RoomId,
        // The publisher's own key, if it is local to this shard — resolved
        // once by the caller instead of hashed here.
        publisher_key: Option<ParticipantKey>,
        now: Instant,
        wall: &WallAnchor,
        ctx: &mut impl RoutingContext,
    ) -> Option<ShardEvent> {
        let publisher = track.meta.origin;
        let Some(_room) = self.room(&room_id) else {
            tracing::debug!(%room_id, "publish_track: room missing on this shard");
            return None;
        };
        self.adopt_track_reverse_target(&track);
        let room = self.room(&room_id)?;
        let tracks = std::slice::from_ref(&track);
        let has_members = !room.members.is_empty();
        for &participant in &room.members {
            if Some(participant) == publisher_key {
                continue;
            }
            ctx.notify_tracks_published(participant, tracks);
        }

        // Audio has no explicit subscribe: membership in the room is the
        // subscription, so the destination installs its route here.
        if track.meta.id.kind() != TrackKind::Audio || ctx.is_local(&publisher) || !has_members {
            return None;
        }
        self.install_audio_route(track.meta, room_id, now, wall)
    }

    /// Retire a destination-side audio route. Idempotent, so a redelivered
    /// unpublish or a room teardown that races it cannot desync the table.
    fn retire_audio_route(&mut self, room_id: RoomId, track_id: TrackId, now: Instant) {
        if let Some(room) = self.room_mut(&room_id)
            && !room.audio_imports.swap_remove(&track_id)
        {
            return;
        }
        if let ImportEffect::Retire { route, epoch } = self.control.imports.unsubscribe(&track_id) {
            self.release_route(RouteHandle::new(route, epoch));
            self.control.imports.on_retired(&track_id);
            self.release_fanout_if_idle(&track_id);
        }
    }

    /// Every audio route this shard installed for `room_id`, retired together
    /// when the room no longer has local members.
    fn retire_room_audio_routes(&mut self, room_id: RoomId, now: Instant) {
        let Some(room) = self.room_mut(&room_id) else {
            return;
        };
        let tracks: Vec<TrackId> = room.audio_imports.iter().copied().collect();
        room.audio_imports.clear();
        for track_id in tracks {
            if let ImportEffect::Retire { route, epoch } =
                self.control.imports.unsubscribe(&track_id)
            {
                self.release_route(RouteHandle::new(route, epoch));
                self.control.imports.on_retired(&track_id);
                self.release_fanout_if_idle(&track_id);
            }
        }
    }

    /// Install audio routes for tracks already published when this shard's
    /// first member joins the room — publish-then-join is as common as
    /// join-then-publish, and only the latter goes through `publish_track`.
    pub fn adopt_known_tracks(
        &mut self,
        room_id: RoomId,
        tracks: &[Track],
        local: &dyn Fn(&ParticipantId) -> bool,
        now: Instant,
        wall: &WallAnchor,
    ) -> Vec<ShardEvent> {
        // Everything `publish_track` would have established, for tracks that
        // predate this shard's first member in the room. Previously only the
        // audio routes were replayed, so a keyframe request for a video track
        // published before the subscriber arrived had nowhere to go.
        for track in tracks {
            self.adopt_track_reverse_target(track);
        }
        tracks
            .iter()
            .filter(|t| t.meta.id.kind() == TrackKind::Audio && !local(&t.meta.origin))
            .filter_map(|t| self.install_audio_route(t.meta.clone(), room_id, now, wall))
            .collect()
    }

    /// Record where keyframe requests for `track` are addressed.
    ///
    /// The only place this is established, so both the announcement path and
    /// the late-join path go through it. Splitting them is what allowed one to
    /// drift into replaying part of the other's work.
    fn adopt_track_reverse_target(&mut self, track: &Track) {
        let Some(route) = track.reverse else {
            // A locally published track: its keyframe requests are dispatched
            // in-process and never addressed.
            return;
        };
        // Minted here rather than assumed to exist: this is also the
        // late-join path, which never runs `publish_track` and so would
        // otherwise have nowhere to record the target at all.
        let key = self.fanout_key(track.meta.id, track.meta.origin);
        let Some(entry) = self.data.tracks.get_mut(key) else {
            pulsebeam_runtime::fatal!("fanout_key returned a key the track table does not hold")
        };
        entry.reverse_target = Some(TrackReverseTarget {
            route,
            encodings: track.layers.iter().map(|l| l.rid).collect(),
        });
    }

    /// Install a destination route for a concrete data stream. Returns the
    /// handle to hand back to the publisher's shard.
    /// Ask for the route an imported data stream needs. The announcement is
    /// emitted when the grant lands, not here.
    fn install_data_route(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        subscriber: Option<ParticipantKey>,
    ) {
        let key = DataStreamId::new(room_id, publisher, topic.clone());
        if self.control.data_imports.subscribe(key.clone()) != ImportEffect::Install {
            return;
        }
        // The local fanout entry exists before the route is requested, so a
        // granted route always resolves to something.
        let Some(&stream) = self.control.data_stream_keys.get(&key) else {
            debug_assert!(false, "requesting a data route with no local fanout entry");
            self.control.data_imports.cancel_install(&key);
            return;
        };
        self.request_route(
            RouteAction::Data { stream },
            RouteNames {
                room_id: Some(room_id),
                origin: publisher,
                track_id: None,
                topic: Some(topic.clone()),
            },
            PendingRoute::Data {
                id: key,
                subscriber,
            },
        );
    }

    /// Remove a local data subscriber without touching the cluster route,
    /// dropping the stream entry if that leaves it referencing nothing.
    fn drop_data_subscriber(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        handle: ParticipantKey,
    ) {
        let key = DataStreamId::new(room_id, publisher, topic.clone());
        let Some(route) = self.data_stream_mut(&key) else {
            return;
        };
        route.local_subscribers.remove_value(&handle);
        self.release_data_stream_if_unused(&room_id, &key);
    }

    fn retire_data_route(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        now: Instant,
    ) {
        let key = DataStreamId::new(room_id, publisher, topic.clone());
        if let ImportEffect::Retire { route, epoch } = self.control.data_imports.unsubscribe(&key) {
            self.release_route(RouteHandle::new(route, epoch));
            self.control.data_imports.on_retired(&key);
        }
    }

    /// A publisher was announced to the room. A destination holding a wildcard
    /// subscription for the topic resolves it into a concrete route here.
    pub fn on_remote_data_publisher(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ShardEvent> {
        let room = self.room(&room_id)?;
        let wildcard_subscribers = room
            .all_publisher_subscriptions
            .local_by_topic
            .get(topic)
            .filter(|s| !s.is_empty())?
            .clone();

        // Materialise the shard-local fanout *before* installing the route, so
        // an installed route always resolves to something that exists. The
        // wildcard named a topic, not a stream, so its subscribers live under
        // `all_publisher_subscriptions` until a publisher makes the stream
        // concrete — which is now. Without this the route resolves to an empty
        // fanout and the frame is delivered to nobody, silently.
        let fanout =
            self.data_stream_or_insert(room_id, DataStreamId::new(room_id, publisher, topic.clone()));
        for subscriber in wildcard_subscribers {
            fanout.local_subscribers.insert_unique(subscriber);
        }

        self.install_data_route(room_id, publisher, topic, None);
        None
    }

    fn install_reliable_route(&mut self, room_id: RoomId, publisher: ParticipantId, topic: &Topic) {
        let key = DataStreamId::new(room_id, publisher, topic.clone());
        if self.control.reliable_imports.subscribe(key.clone()) != ImportEffect::Install {
            return;
        }
        // Minted before the route install, same rule as the reverse route:
        // an installed route always resolves to something that exists.
        let stream = self.reliable_stream_key_or_insert(room_id, key.clone());
        self.request_route(
            RouteAction::Reliable { stream },
            RouteNames {
                room_id: Some(room_id),
                origin: publisher,
                track_id: None,
                topic: Some(topic.clone()),
            },
            PendingRoute::Reliable(key),
        );
    }

    /// A reliable publisher was announced. A destination with local subscribers
    /// on the topic resolves it into a concrete route.
    pub fn on_remote_reliable_publisher(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ShardEvent> {
        let room = self.room(&room_id)?;
        if !room.reliable.has_local_subscribers(topic) {
            return None;
        }
        self.install_reliable_route(room_id, publisher, topic);
        let id = DataStreamId::new(room_id, publisher, topic.clone());
        if let Some(stream) = self.reliable_stream_mut(&id) {
            stream.imported = true;
        }
        None
    }

    /// Record a destination's handle for a reliable stream, or register its
    /// interest in the topic. Returns publishers this shard already serves on
    /// the topic so the caller can announce them.
    pub fn register_remote_reliable_subscriber_shard(
        &mut self,
        room_id: RoomId,
        from_shard_id: ShardId,
        topic: Topic,
        publisher: Option<ParticipantId>,
        remote: Option<RemoteRoute>,
    ) -> Vec<ParticipantId> {
        if self.room(&room_id).is_none() {
            return Vec::new();
        }
        match (publisher, remote) {
            (Some(publisher), Some(remote)) => {
                debug_assert_eq!(remote.shard_id, from_shard_id);
                let stream =
                    self.reliable_stream_or_insert(room_id, DataStreamId::new(room_id, publisher, topic));
                match stream
                    .remote_routes
                    .iter_mut()
                    .find(|r| r.shard_id == remote.shard_id)
                {
                    Some(existing) => *existing = remote,
                    None => stream.remote_routes.push(remote),
                }
                Vec::new()
            }
            (Some(_), None) => {
                debug_assert!(false, "a concrete reliable subscription needs a handle");
                Vec::new()
            }
            (None, _) => self.published_reliable_on(&room_id, &topic),
        }
    }

    pub fn unregister_remote_reliable_subscriber_shard(
        &mut self,
        room_id: RoomId,
        from_shard_id: ShardId,
        topic: &Topic,
        publisher: Option<ParticipantId>,
    ) {
        if self.room(&room_id).is_none() {
            return;
        }
        let publishers = match publisher {
            Some(publisher) => vec![publisher],
            None => self.published_reliable_on(&room_id, topic),
        };
        for publisher in publishers {
            let id = DataStreamId::new(room_id, publisher, topic.clone());
            if let Some(stream) = self.reliable_stream_mut(&id) {
                stream.remote_routes.retain(|r| r.shard_id != from_shard_id);
            }
            self.release_reliable_stream_if_unused(&room_id, &id);
        }
    }

    fn install_audio_route(
        &mut self,
        meta: TrackMeta,
        room_id: RoomId,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ShardEvent> {
        if self.control.imports.subscribe(meta.id) != ImportEffect::Install {
            return None;
        }
        let Some(&room) = self.control.room_keys.get(&room_id) else {
            debug_assert!(false, "installing an audio route for an unknown room");
            self.control.imports.cancel_install(&meta.id);
            return None;
        };
        // Gives this imported track the same dense fanout entry a video
        // subscription gets, so `RouteAction::Audio` can resolve origin and
        // track_id off it instead of carrying them inline. Never populates
        // `subscribers`/`remote_routes` — audio's own liveness is the import
        // table, not this entry — so `retire_audio_route` releases it
        // directly rather than through the emptiness check video relies on.
        let track = self.fanout_key(meta.id, meta.origin);
        self.request_route(
            RouteAction::Audio { room, track },
            RouteNames {
                room_id: Some(room_id),
                origin: meta.origin,
                track_id: Some(meta.id),
                topic: None,
            },
            PendingRoute::Audio { meta, room_id },
        );
        None
    }

    pub fn unpublish_tracks(
        &mut self,
        room_id: RoomId,
        track_ids: &[TrackId],
        now: Instant,
        ctx: &mut impl RoutingContext,
    ) {
        let track_keys: Vec<LocalTrackKey> = track_ids
            .iter()
            .filter_map(|id| self.fanout_of(id))
            .collect();
        if let Some(room) = self.room_mut(&room_id) {
            for key in track_keys {
                room.audio_selector.remove_track((key, None));
            }
        }
        for &track_id in track_ids {
            self.retire_audio_route(room_id, track_id, now);
            if let Some(key) = self.fanout_of(&track_id)
                && let Some(entry) = self.data.tracks.get_mut(key)
            {
                entry.reverse_target = None;
            }
            self.release_fanout_if_idle(&track_id);
        }
        let Some(room) = self.room(&room_id) else {
            tracing::debug!(%room_id, "unpublish_tracks: room missing on this shard");
            return;
        };
        for &participant in &room.members {
            ctx.notify_tracks_unpublished(participant, track_ids);
        }
    }

    // -- hot-path packet fanout --------------------------------------------

    #[inline]
    /// Fan a packet out to a track's local subscribers and remote destinations.
    ///
    /// Addressed by [`LocalTrackKey`], not by name: a cross-shard arrival gets
    /// the key straight out of its route entry, so the whole path from wire to
    /// subscriber is index lookups. Hashing a `TrackId` here measured 29.8ns
    /// against 6.6ns for an index — per packet, before any fanout.
    pub fn route_video(
        &mut self,
        fanout: LocalTrackKey,
        pkt: RtpPacket,
        ctx: &mut impl RoutingContext,
    ) {
        let Some(route) = self.data.tracks.get_mut(fanout) else {
            // Unreachable unless the arenas have desynced: a fanout key only
            // ever comes from a route the control plane installed, and
            // `track_keys`/`tracks` are created and removed together. A
            // silent drop here is vanishing media.
            debug_assert!(false, "a video fanout key must resolve to a track");
            return;
        };
        let track_id = route.track_id;

        // Hand the packet to the cache and read it back rather than cloning it
        // in: the cache stores every packet anyway, so a clone here is a second
        // copy of the same bytes — and an `RtpPacket` clone heap-allocates,
        // because str0m's `ExtensionValues` carries a type-keyed map. Created
        // lazily here rather than at `TrackRoute::new`, matching the fact that
        // it is also dropped as soon as nothing consumes it.
        let (rid, seq) = (pkt.ext_vals.rid, pkt.seq_no);
        let cache = route.cache.get_or_insert_with(TrackStreamCache::new);
        let too_old = cache.push(pkt);
        let cache: &TrackStreamCache = cache;
        let Some(pkt) = too_old
            .as_ref()
            .or_else(|| cache.encoding(rid).and_then(|c| c.get(seq)))
        else {
            debug_assert!(false, "a stored packet must be readable back");
            return;
        };

        for &subscriber in &route.subscribers {
            ctx.forward_video_rtp(subscriber, track_id, pkt, Some(cache));
        }
        let playout = ctx.wall().to_ntp(pkt.playout_time);
        let transit: Vec<(ShardId, MediaEnvelope)> = route
            .remote_routes
            .iter_mut()
            .map(|remote| (remote.shard_id, remote.next_envelope(playout)))
            .collect();
        for (shard_id, env) in transit {
            ctx.send_media(shard_id, env, MediaPayload::Video(pkt.to_transit()));
        }
    }

    #[inline]
    pub fn route_audio(
        &mut self,
        track_key: LocalTrackKey,
        origin: Origin,
        mut ev: AudioRtpEvent,
        ctx: &mut impl RoutingContext,
    ) {
        let room = ev.room;
        let origin_key = ev.origin_key;
        debug_assert!(
            origin.is_local() || origin_key.is_none(),
            "a remote origin must never carry a local key"
        );
        tracing::trace!(
            target: crate::log::TARGET_AUDIO,
            room = ?room,
            origin = %ev.origin,
            stream_id = %ev.stream_id.0,
            seq_no = %ev.pkt.seq_no,
            "audio packet entered shard audio fanout"
        );

        // Split the borrow: the room owns the selector and members, while the
        // per-stream sender handles live in `tracks`.
        let Self { data, .. } = self;
        let DataPlane { rooms, tracks, .. } = data;
        let Some(room) = rooms.get_mut(room) else {
            tracing::warn!(target: crate::log::TARGET_AUDIO, "audio packet dropped: room missing");
            return;
        };

        if origin.is_local()
            && let Some(track) = tracks.get_mut(track_key)
        {
            for remote in &mut track.remote_routes {
                let env = remote.next_envelope(ctx.wall().to_ntp(ev.pkt.playout_time));
                ctx.send_media(
                    remote.shard_id,
                    env,
                    MediaPayload::Audio(ev.pkt.to_transit()),
                );
            }
        }

        let Some(slot_idx) = room
            .audio_selector
            .filter((track_key, ev.stream_id.1), &mut ev.pkt)
        else {
            return;
        };
        for &participant in &room.members {
            if Some(participant) == origin_key {
                continue;
            }
            ctx.forward_audio_rtp(
                participant,
                slot_idx,
                AudioOrigin {
                    participant: ev.origin,
                    track: ev.stream_id.0,
                },
                &ev.pkt,
            );
        }
    }

    #[inline]
    pub fn route_data(
        &mut self,
        stream: DataStreamKey,
        origin: Origin,
        pkt: &[u8],
        ctx: &mut impl RoutingContext,
    ) {
        let Some(route) = self.data.data_streams.get_mut(stream) else {
            debug_assert!(false, "a data fanout key must resolve to a stream");
            return;
        };
        let publisher = route.id.publisher_id;
        for &subscriber in &route.local_subscribers {
            ctx.forward_sctp(subscriber, publisher, &route.id.topic, pkt);
        }

        if origin.is_local() {
            let playout = ctx.wall().ntp();
            for entry in &mut route.remote_subscriber_shards {
                let env = entry.remote.next_envelope(playout);
                ctx.send_media(entry.remote.shard_id, env, MediaPayload::Data(pkt.to_vec()));
            }
        }
    }

    /// Register a local reliable publisher and open the reverse path
    /// subscribers use to drive the application's retransmission protocol.
    pub fn register_reliable_data_publisher(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    ) -> Option<ReliableStreamKey> {
        self.room(&room_id)?;
        let id = DataStreamId::new(room_id, publisher, topic.clone());
        let key = self.reliable_stream_key_or_insert(room_id, id);
        if let Some(stream) = self.data.reliable_streams.get_mut(key) {
            debug_assert!(!stream.published);
            stream.published = true;
        }
        self.open_topic_reverse_route(room_id, publisher, topic);
        Some(key)
    }

    pub fn unregister_reliable_data_publisher(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        now: Instant,
    ) {
        let id = DataStreamId::new(room_id, publisher, topic.clone());
        if let Some(stream) = self.reliable_stream_mut(&id) {
            debug_assert!(stream.published);
            stream.published = false;
        }
        self.close_topic_reverse_route(room_id, publisher, topic, now);
        self.release_reliable_stream_if_unused(&room_id, &id);
    }

    pub fn register_reliable_data_subscriber(
        &mut self,
        room_id: RoomId,
        handle: ParticipantKey,
        topic: Topic,
    ) -> Option<ShardEvent> {
        let room = self.room_mut(&room_id)?;
        let was_empty = room.reliable.subscribe_local(handle, topic.clone());
        // A reliable subscription names only a topic, so there is no stream to
        // install a route for yet; publishers announce themselves in response.
        was_empty.then_some(ShardEvent::Relay(Topology::ReliableTopicSubscribed {
            room_id,
            topic,
            publisher: None,
            route: None,
            epoch: 0,
        }))
    }

    pub fn unregister_reliable_data_subscriber(
        &mut self,
        room_id: RoomId,
        handle: ParticipantKey,
        topic: &Topic,
        now: Instant,
    ) -> bool {
        let Some(room) = self.room_mut(&room_id) else {
            return false;
        };
        let was_last = room.reliable.unsubscribe_local(handle, topic);
        if !was_last {
            return false;
        }
        // Nothing left to deliver to on this topic, so every destination route
        // the shard installed for it retires.
        let imported: Vec<ParticipantId> = self
            .room_reliable_stream_keys(&room_id)
            .into_iter()
            .filter_map(|key| self.data.reliable_streams.get(key))
            .filter(|stream| stream.imported && stream.id.topic == *topic)
            .map(|stream| stream.id.publisher_id)
            .collect();
        for publisher in imported {
            let id = DataStreamId::new(room_id, publisher, topic.clone());
            if let Some(stream) = self.reliable_stream_mut(&id) {
                stream.imported = false;
            }
            self.release_reliable_stream_if_unused(&room_id, &id);
            if let ImportEffect::Retire { route, epoch } =
                self.control.reliable_imports.unsubscribe(&id)
            {
                self.release_route(RouteHandle::new(route, epoch));
                self.control.reliable_imports.on_retired(&id);
            }
        }
        true
    }

    pub fn route_reliable_data(
        &mut self,
        stream: ReliableStreamKey,
        origin: Origin,
        frame: &[u8],
        ctx: &mut impl RoutingContext,
    ) {
        debug_assert!(!frame.is_empty());
        let Some(entry) = self.data.reliable_streams.get_mut(stream) else {
            debug_assert!(false, "a reliable fanout key must resolve to a stream");
            return;
        };
        let room_key = entry.room;
        let publisher = entry.id.publisher_id;
        let topic = entry.id.topic.clone();
        let local_origin = origin.is_local();
        // `published` marks the shard that hosts the publisher, and only that
        // shard sets it. A destination reaches here too — with a route it
        // installed for the stream and a publisher that is not its own — so the
        // flag tracks locality rather than being universally true.
        debug_assert!(
            !local_origin || entry.published,
            "the published flag must mean 'this shard hosts the publisher'"
        );
        if local_origin {
            let playout = ctx.wall().ntp();
            let frames: Vec<(ShardId, MediaEnvelope)> = entry
                .remote_routes
                .iter_mut()
                .map(|remote| (remote.shard_id, remote.next_envelope(playout)))
                .collect();
            for (shard_id, env) in frames {
                ctx.send_media(shard_id, env, MediaPayload::Data(frame.to_vec()));
            }
        }
        let Some(room) = self.data.rooms.get(room_key) else {
            debug_assert!(false, "a reliable stream's room key must resolve to a room");
            return;
        };
        if let Some(subscribers) = room.reliable.local_subscribers(&topic) {
            for &subscriber in subscribers {
                ctx.forward_reliable_sctp(subscriber, publisher, &topic, frame);
            }
        }
    }

    pub fn route_reliable_control(
        &self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        bytes: &[u8],
        ctx: &mut impl RoutingContext,
    ) {
        if ctx.is_local(&publisher) {
            ctx.deliver_reliable_control(publisher, topic, bytes);
            return;
        }
        let key = DataStreamId::new(room_id, publisher, topic.clone());
        let target = self
            .reliable_stream_key(&key)
            .and_then(|stream| self.reliable_stream(stream))
            .and_then(|entry| entry.reverse_target);
        let Some(target) = target else {
            // The handle arrives with the publisher announcement, so a
            // subscription cannot predate it.
            debug_assert!(false, "no reverse route for a remote reliable publisher");
            return;
        };
        // The reverse route's own id carries its destination shard, so there
        // is nothing left to look up.
        ctx.send_frame(
            target.route.shard(),
            ShardFrame::Reverse {
                env: RouteEnvelope::feedback(target),
                body: Reverse::DataAck(bytes.to_vec()),
            },
        );
    }
}

// -- participant-originated control-event routing -------------------------

/// Queues an event a participant raised about itself for the controller.
///
/// Everything reaching here is topology, so it all goes to the control plane.
/// Anything a shard must send another shard directly — feedback, media — is
/// handled by the caller and never appears in this match.
pub(crate) fn route_participant_control_event(
    ev: ParticipantControlEvent,
    shard_events: &mut VecDeque<ShardEvent>,
) {
    match ev {
        ParticipantControlEvent::TrackPublished(track, _states) => {
            shard_events.push_back(ShardEvent::TrackPublished(track));
        }
        ParticipantControlEvent::TrackUnpublished { origin, track_id } => {
            shard_events.push_back(ShardEvent::TrackUnpublished { origin, track_id });
        }
        ParticipantControlEvent::KeyframeRequested(_)
        | ParticipantControlEvent::TrackStatsUpdated { .. } => {
            debug_assert!(
                false,
                "handled by shard core, never routed to the controller"
            );
        }
        ParticipantControlEvent::DataTopicPublished { .. }
        | ParticipantControlEvent::DataTopicUnpublished { .. } => {
            debug_assert!(false, "data stream lifecycle must be handled by shard core");
        }
        ParticipantControlEvent::DataTopicSubscribed {
            room_id,
            subscriber,
            topic,
            publisher: _,
        } => {
            tracing::trace!(
                room_id = %room_id,
                subscriber = %subscriber,
                topic = %topic.as_ref(),
                "data topic subscribe is handled directly in shard core"
            );
        }
        ParticipantControlEvent::DataTopicUnsubscribed {
            room_id,
            subscriber,
            topic,
            publisher: _,
        } => {
            tracing::trace!(
                room_id = %room_id,
                subscriber = %subscriber,
                topic = %topic.as_ref(),
                "data topic unsubscribe is handled directly in shard core"
            );
        }
        ParticipantControlEvent::ReliableDataTopicPublished { .. }
        | ParticipantControlEvent::ReliableDataTopicUnpublished { .. }
        | ParticipantControlEvent::ReliableDataTopicSubscribed { .. }
        | ParticipantControlEvent::ReliableDataTopicUnsubscribed { .. }
        | ParticipantControlEvent::ReliableControlReceived { .. } => {
            debug_assert!(
                false,
                "reliable data channel events must be handled by shard core"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    // A fixture that overflows should fail the test, not clamp into a pass.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use slotmap::SlotMap;
    use std::cell::RefCell;
    use std::collections::HashSet as StdHashSet;

    use crate::entity::ExternalRoomId;
    use crate::shard::participants::ParticipantKey;

    thread_local! {
        static PARTICIPANT_KEYS: RefCell<SlotMap<ParticipantKey, ()>> =
            RefCell::new(SlotMap::with_key());
    }

    /// A fresh, distinct participant key. Every test-fixture site used to mint
    /// one from its own throwaway `SlotMap`, which happened to work only
    /// because `ParticipantHandle` carried a `ParticipantId` alongside the key
    /// and comparisons went by that — the key itself collided across calls,
    /// silently. Now the key *is* the handle, so this shares one map for the
    /// whole test module the way `add_local_member`'s caller in production
    /// always does (one registry, minting keys that never repeat).
    fn new_participant_key() -> ParticipantKey {
        PARTICIPANT_KEYS.with(|slots| slots.borrow_mut().insert(()))
    }

    /// A `RoutingContext` fake that just records calls. No `ParticipantCore`,
    /// no tracing spans, no `ShardCore` — this is the whole point of the
    /// trait boundary.
    fn now() -> Instant {
        Instant::now()
    }

    fn wall() -> WallAnchor {
        WallAnchor::new(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            Instant::now(),
        )
    }

    struct RecordingCtx {
        wall: WallAnchor,
        local: StdHashSet<ParticipantId>,
        sent: RefCell<Vec<(ShardId, ShardFrame)>>,
        forwarded_video: RefCell<Vec<ParticipantKey>>,
        forwarded_audio: RefCell<Vec<(ParticipantKey, AudioSelectorSlotId)>>,
        forwarded_sctp: RefCell<Vec<ParticipantKey>>,
        published: RefCell<Vec<ParticipantKey>>,
        unpublished: RefCell<Vec<ParticipantKey>>,
        keyframed: RefCell<Vec<ParticipantId>>,
    }

    impl Default for RecordingCtx {
        fn default() -> Self {
            Self {
                wall: wall(),
                local: StdHashSet::default(),
                sent: RefCell::default(),
                forwarded_video: RefCell::default(),
                forwarded_audio: RefCell::default(),
                forwarded_sctp: RefCell::default(),
                published: RefCell::default(),
                unpublished: RefCell::default(),
                keyframed: RefCell::default(),
            }
        }
    }

    impl ShardTransport for RecordingCtx {
        fn send_media(&self, dst: ShardId, env: MediaEnvelope, payload: MediaPayload) {
            self.sent
                .borrow_mut()
                .push((dst, ShardFrame::Media { env, payload }));
        }

        fn send_frame(&self, dst: ShardId, ev: ShardFrame) {
            self.sent.borrow_mut().push((dst, ev));
        }
    }

    impl RoutingContext for RecordingCtx {
        fn wall(&self) -> &WallAnchor {
            &self.wall
        }

        fn update_layer_states(
            &mut self,
            _subscriber: ParticipantKey,
            _track_id: TrackId,
            _states: &crate::track::TrackStates,
        ) {
        }

        fn forward_video_rtp(
            &mut self,
            subscriber: ParticipantKey,
            _track_id: TrackId,
            _pkt: &RtpPacket,
            _cache: Option<&TrackStreamCache>,
        ) {
            self.forwarded_video.borrow_mut().push(subscriber);
        }
        fn forward_audio_rtp(
            &mut self,
            subscriber: ParticipantKey,
            slot_idx: AudioSelectorSlotId,
            _origin: AudioOrigin,
            _pkt: &RtpPacket,
        ) {
            self.forwarded_audio
                .borrow_mut()
                .push((subscriber, slot_idx));
        }
        fn forward_sctp(
            &mut self,
            subscriber: ParticipantKey,
            _origin: ParticipantId,
            _topic: &Topic,
            _pkt: &[u8],
        ) {
            self.forwarded_sctp.borrow_mut().push(subscriber);
        }
        fn notify_tracks_published(&mut self, participant: ParticipantKey, _tracks: &[Track]) {
            self.published.borrow_mut().push(participant);
        }
        fn notify_tracks_unpublished(
            &mut self,
            participant: ParticipantKey,
            _track_ids: &[TrackId],
        ) {
            self.unpublished.borrow_mut().push(participant);
        }
        fn notify_keyframe_request(
            &mut self,
            participant_id: ParticipantId,
            _track_id: TrackId,
            _rid: Option<Rid>,
            _kind: KeyframeRequestKind,
        ) {
            self.keyframed.borrow_mut().push(participant_id);
        }
        fn is_local(&self, id: &ParticipantId) -> bool {
            self.local.contains(id)
        }

        fn forward_reliable_sctp(
            &mut self,
            subscriber: ParticipantKey,
            _origin: ParticipantId,
            _topic: &Topic,
            _frame: &[u8],
        ) {
            self.forwarded_sctp.borrow_mut().push(subscriber);
        }

        fn deliver_reliable_control(
            &mut self,
            _publisher: ParticipantId,
            _topic: &Topic,
            _bytes: &[u8],
        ) {
        }
    }

    /// Run the control plane's half of any route the call just asked for and
    /// return what the shard announces once it lands. Registration is a
    /// request now, so a test that wants the announcement has to let the
    /// grant happen.
    fn settled(table: &mut ShardRoutingTable) -> Option<ShardEvent> {
        table.settle_routes(now(), &wall()).into_iter().next()
    }

    fn room_id(s: &str) -> RoomId {
        RoomId::from_external(&ExternalRoomId::new(s).unwrap())
    }

    /// A single-encoding descriptor for `meta`, enough to open a reverse route.
    fn video_track_with(meta: &TrackMeta) -> Track {
        Track {
            meta: meta.clone(),
            layers: vec![crate::track::TrackLayer {
                meta: meta.clone(),
                rid: None,
                quality: crate::track::LayerQuality::High,
            }],
            reverse: None,
        }
    }

    /// Tests still speak in names; production does not.
    fn fanout<'a>(table: &'a ShardRoutingTable, track_id: &TrackId) -> &'a TrackRoute {
        &table.data.tracks[table
            .fanout_of(track_id)
            .expect("track known to this shard")]
    }

    fn pid() -> ParticipantId {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        ParticipantId::new(&mut pulsebeam_runtime::rand::seeded_rng(
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ))
    }

    fn add_local_subscriber(
        _table: &mut ShardRoutingTable,
        _participant_id: ParticipantId,
    ) -> ParticipantKey {
        new_participant_key()
    }

    fn replace_local_subscriber(
        _table: &mut ShardRoutingTable,
        _participant_id: ParticipantId,
    ) -> ParticipantKey {
        new_participant_key()
    }

    // -- the bug this refactor exists to prevent recurring ------------------

    #[test]
    fn duplicate_register_remote_participant_does_not_leak_refcount() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let participant = pid();
        let room = room_id("r1");
        let shard = ShardId::new(1);

        table.register_remote_participant(participant, room, shard, &mut rng);
        // Redelivered / duplicate register for the exact same (room, shard).
        table.register_remote_participant(participant, room, shard, &mut rng);

        // A single unregister must be enough to fully release the shard —
        // if the duplicate register above had bumped the refcount, this
        // would leave a phantom `remote_shards` entry forever.
        table.unregister_remote_participant(
            participant,
            ParticipantShardMeta {
                shard_id: shard,
                room_id: room,
            },
        );

        assert!(
            !table.has_room(&room),
            "room must be fully cleaned up after one register + one unregister"
        );
    }

    #[test]
    fn moving_remote_participant_releases_the_old_shard() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let participant = pid();
        let room = room_id("r2");
        let old_shard = ShardId::new(1);
        let new_shard = ShardId::new(2);

        table.register_remote_participant(participant, room, old_shard, &mut rng);
        table.register_remote_participant(participant, room, new_shard, &mut rng);

        assert!(
            !table
                .room(&room)
                .unwrap()
                .remote_shards
                .contains(&old_shard)
        );
        assert!(
            table
                .room(&room)
                .unwrap()
                .remote_shards
                .contains(&new_shard)
        );
    }

    // -- topology ------------------------------------------------------------

    #[test]
    fn first_subscriber_notifies_publisher_shard() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let track = TrackMeta {
            shard_id: ShardId::new(1),
            id: pid().derive_track_id(TrackKind::Video, "v"),
            origin: pid(),
        };

        let first = pid();
        let second = pid();
        let first_key = add_local_subscriber(&mut table, first);
        let second_key = add_local_subscriber(&mut table, second);

        table.register_subscriber(first_key, track.clone(), now(), &wall());
        let ev = settled(&mut table);
        assert!(
            matches!(ev, Some(ShardEvent::Relay(Topology::TrackSubscribed { track: t, .. })) if t == track),
            "the first subscriber asks for a route and hands over the handle once granted"
        );

        table.register_subscriber(second_key, track, now(), &wall());
        assert!(
            settled(&mut table).is_none(),
            "second subscriber must not re-notify"
        );
    }

    /// A reconnect (same `ParticipantId`, a fresh `ParticipantKey`) must not
    /// leave the old connection's key behind in a track's subscriber list.
    ///
    /// Route and key now share a lifetime by construction — "created and
    /// removed together" — so the connection that owned the old key must be
    /// torn down (`unregister_subscriber`) before the new one takes its place
    /// in `local_participants`, the same order `ShardCore::add_participant`
    /// already enforces by removing the old participant first. This is what
    /// makes the fanout resolvable-by-key safe: there is no name left on the
    /// entry for `register_subscriber` to deduplicate by, so nothing but
    /// teardown-before-replace can prevent two keys for one person.
    #[test]
    fn a_reconnect_only_leaves_the_new_key_in_the_fanout() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let subscriber = pid();
        let track = TrackMeta {
            shard_id: ShardId::new(1),
            id: pid().derive_track_id(TrackKind::Video, "v"),
            origin: pid(),
        };
        let subscriber_key = add_local_subscriber(&mut table, subscriber);
        table.register_subscriber(subscriber_key, track.clone(), now(), &wall());
        assert!(settled(&mut table).is_some());

        assert!(
            table
                .unregister_subscriber(subscriber_key, track.clone(), now())
                .is_some(),
            "the old connection must be torn down before the new one replaces it"
        );
        let replacement = replace_local_subscriber(&mut table, subscriber);
        table.register_subscriber(replacement, track.clone(), now(), &wall());
        assert!(
            settled(&mut table).is_some(),
            "the new connection is a fresh subscriber, not a churn no-op"
        );

        assert_eq!(fanout(&table, &track.id).subscribers.as_slice(), [replacement]);
        assert!(
            table
                .unregister_subscriber(replacement, track, now())
                .is_some()
        );
    }

    // -- fanout ---------------------------------------------------------------

    /// A reliable subscription names only a topic, so it installs nothing until
    /// a publisher on that topic is announced, then retires with the last
    /// local subscriber.
    #[test]
    fn a_reliable_subscription_resolves_on_publisher_announcement() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(13);
        let room = room_id("reliable-room");
        let handle = new_participant_key();
        table.add_local_member(handle, room, &mut rng);

        let topic = Topic::for_test("chat");
        let ev = table.register_reliable_data_subscriber(room, handle, topic.clone());
        assert!(
            matches!(
                ev,
                Some(ShardEvent::Relay(Topology::ReliableTopicSubscribed {
                    publisher: None,
                    route: None,
                    ..
                }))
            ),
            "a topic-only subscription has no stream to route yet"
        );
        assert_eq!(table.live_routes(), 0);

        let publisher = pid();
        table.on_remote_reliable_publisher(room, publisher, &topic, now(), &wall());
        let resolved = settled(&mut table);
        assert!(
            matches!(
                resolved,
                Some(ShardEvent::Relay(Topology::ReliableTopicSubscribed {
                    publisher: Some(_),
                    route: Some(_),
                    ..
                }))
            ),
            "the announcement resolves it into a concrete route"
        );
        assert_eq!(table.live_routes(), 1);

        assert!(table.unregister_reliable_data_subscriber(room, handle, &topic, now()));
        assert_eq!(
            table.live_routes(),
            0,
            "the last subscriber leaving retires the imported route"
        );
    }

    // ── Room-scoped data streams ─────────────────────────────────────────

    /// A data stream is `(publisher, topic)` *within a room*. Two rooms using
    /// the same names are two streams, and traffic in one must not reach the
    /// other — a route being unique across the node is an addressing fact, not
    /// an authorization one.
    #[test]
    fn the_same_publisher_and_topic_in_two_rooms_are_two_streams() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(21);
        let first = room_id("room-a");
        let second = room_id("room-b");
        let topic = Topic::for_test("chat");
        let publisher = pid();

        let a = new_participant_key();
        let b = new_participant_key();
        table.add_local_member(a, first, &mut rng);
        table.add_local_member(b, second, &mut rng);

        table.register_data_subscriber(first, a, topic.clone(), None, now(), &wall());
        table.on_remote_data_publisher(first, publisher, &topic, now(), &wall());
        table.register_data_subscriber(second, b, topic.clone(), None, now(), &wall());
        table.on_remote_data_publisher(second, publisher, &topic, now(), &wall());

        let in_first = table
            .data_stream(&DataStreamId::new(first, publisher, topic.clone()))
            .map(|route| route.local_subscribers.clone())
            .expect("room-a has its own stream");
        let in_second = table
            .data_stream(&DataStreamId::new(second, publisher, topic.clone()))
            .map(|route| route.local_subscribers.clone())
            .expect("room-b has its own stream");

        assert!(in_first.contains(&a), "room-a's subscriber is in room-a's fanout");
        assert!(
            !in_first.contains(&b),
            "room-b's subscriber must not appear in room-a's fanout"
        );
        assert!(in_second.contains(&b));
        assert!(!in_second.contains(&a));
    }

    /// A lookup aimed at a room the stream does not belong to must miss.
    /// The room is part of the key precisely so this cannot be forgotten at a
    /// call site.
    #[test]
    fn a_lookup_in_the_wrong_room_finds_nothing() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(22);
        let owned = room_id("owned");
        let other = room_id("other");
        let topic = Topic::for_test("chat");
        let publisher = pid();

        let subscriber = new_participant_key();
        table.add_local_member(subscriber, owned, &mut rng);
        table.register_data_subscriber(owned, subscriber, topic.clone(), None, now(), &wall());
        table.on_remote_data_publisher(owned, publisher, &topic, now(), &wall());

        assert!(
            table
                .data_stream(&DataStreamId::new(owned, publisher, topic.clone()))
                .is_some()
        );
        assert!(
            table
                .data_stream(&DataStreamId::new(other, publisher, topic.clone()))
                .is_none(),
            "a forged room must not resolve another room's stream"
        );
    }

    /// Tearing a topic down in one room must leave the identically named
    /// stream in another room untouched.
    #[test]
    fn a_teardown_in_one_room_cannot_retire_another_room_s_stream() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(23);
        let first = room_id("tear-a");
        let second = room_id("tear-b");
        let topic = Topic::for_test("chat");
        let publisher = pid();

        let a = new_participant_key();
        let b = new_participant_key();
        table.add_local_member(a, first, &mut rng);
        table.add_local_member(b, second, &mut rng);

        table.register_data_subscriber(first, a, topic.clone(), None, now(), &wall());
        table.on_remote_data_publisher(first, publisher, &topic, now(), &wall());
        table.register_data_subscriber(second, b, topic.clone(), None, now(), &wall());
        table.on_remote_data_publisher(second, publisher, &topic, now(), &wall());

        table.unregister_data_subscriber(first, a, &topic, Some(publisher), now());

        assert!(
            table
                .data_stream(&DataStreamId::new(second, publisher, topic.clone()))
                .is_some_and(|route| route.local_subscribers.contains(&b)),
            "room-b's stream must survive room-a's teardown"
        );
    }

    /// The reliable lane carries its own arena and its own key index, so it
    /// needs the same isolation proved separately.
    #[test]
    fn reliable_streams_are_room_scoped_too() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(24);
        let first = room_id("rel-a");
        let second = room_id("rel-b");
        let topic = Topic::for_test("chat");
        let publisher = pid();

        let a = new_participant_key();
        let b = new_participant_key();
        table.add_local_member(a, first, &mut rng);
        table.add_local_member(b, second, &mut rng);

        table.register_reliable_data_subscriber(first, a, topic.clone());
        table.on_remote_reliable_publisher(first, publisher, &topic, now(), &wall());
        table.register_reliable_data_subscriber(second, b, topic.clone());
        table.on_remote_reliable_publisher(second, publisher, &topic, now(), &wall());

        let in_first = table.reliable_stream_key(&DataStreamId::new(first, publisher, topic.clone()));
        let in_second =
            table.reliable_stream_key(&DataStreamId::new(second, publisher, topic.clone()));

        assert!(in_first.is_some());
        assert!(in_second.is_some());
        assert_ne!(
            in_first, in_second,
            "the same names in two rooms must not collapse onto one arena entry"
        );
    }

    /// A wildcard subscriber that arrives after a remote publisher is already
    /// known still joins that stream's fanout.
    ///
    /// The shard is announced to only once per topic, when its first wildcard
    /// subscriber arrives, and the announcement snapshots the subscribers that
    /// existed at that moment. So a second subscriber has to attach itself to
    /// streams already imported. Missing that is silent: it subscribes, the
    /// stream flows, and it alone receives nothing.
    #[test]
    fn a_late_wildcard_subscriber_joins_an_imported_stream() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(13);
        let room = room_id("wildcard-late");
        let topic = Topic::for_test("chat");
        let mut slots = SlotMap::<ParticipantKey, ()>::with_key();

        let mut subscribers = Vec::new();
        for _ in 0..2 {
            let id = pid();
            let handle = slots.insert(());
            table.add_local_member(handle, room, &mut rng);
            subscribers.push((id, handle));
        }
        let publisher = pid();

        table.register_data_subscriber(room, subscribers[0].1, topic.clone(), None, now(), &wall());
        table.on_remote_data_publisher(room, publisher, &topic, now(), &wall());
        table.register_data_subscriber(room, subscribers[1].1, topic.clone(), None, now(), &wall());

        let fanout = table
            .data_stream(&DataStreamId::new(room, publisher, topic.clone()))
            .map(|route| route.local_subscribers.clone())
            .expect("the imported stream should exist once its publisher is announced");

        for (n, (_, handle)) in subscribers.iter().enumerate() {
            assert!(
                fanout.contains(handle),
                "wildcard subscriber {n} is not in the imported stream's fanout, so it \
                 receives nothing while the other subscriber is served"
            );
        }
    }

    /// Every local subscriber on a topic receives a remote publisher's frame,
    /// not just the one that happened to subscribe first.
    ///
    /// The shard announces its interest in a topic only when the first local
    /// subscriber arrives, because later ones need no new route. That makes the
    /// fan-out at delivery the only thing standing between a second subscriber
    /// and silence, which is what this pins.
    #[test]
    fn every_local_subscriber_receives_a_remote_publishers_frame() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(13);
        let room = room_id("reliable-fanout");
        let topic = Topic::for_test("chat");
        let mut slots = SlotMap::<ParticipantKey, ()>::with_key();

        let mut ctx = RecordingCtx::default();
        let mut subscribers = Vec::new();
        for _ in 0..2 {
            let id = pid();
            let handle = slots.insert(());
            table.add_local_member(handle, room, &mut rng);
            subscribers.push((id, handle));
        }

        let publisher = pid();
        table.register_reliable_data_subscriber(room, subscribers[0].1, topic.clone());
        table.on_remote_reliable_publisher(room, publisher, &topic, now(), &wall());
        table.register_reliable_data_subscriber(room, subscribers[1].1, topic.clone());

        let stream = table
            .reliable_stream_key(&DataStreamId::new(room, publisher, topic.clone()))
            .expect("the imported stream should exist once its publisher is announced");
        table.route_reliable_data(stream, Origin::Remote, b"hello", &mut ctx);

        let delivered = ctx.forwarded_sctp.borrow().clone();
        for (n, (_, handle)) in subscribers.iter().enumerate() {
            assert!(
                delivered.contains(handle),
                "subscriber {n} received nothing; a topic fan-out that serves only \
                 some of its subscribers loses data silently (delivered to {delivered:?})"
            );
        }
    }

    /// An explicit `publisher: Some(..)` data subscription knows its stream, so
    /// the destination installs a route immediately and hands back the handle.
    #[test]
    fn an_explicit_data_subscription_installs_a_route() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(11);
        let room = room_id("data-room");
        let handle = new_participant_key();
        table.add_local_member(handle, room, &mut rng);

        let publisher = pid();
        let topic = Topic::for_test("chat");
        table.register_data_subscriber(
            room,
            handle,
            topic.clone(),
            Some(publisher),
            now(),
            &wall(),
        );
        assert!(
            matches!(
                settled(&mut table),
                Some(ShardEvent::Relay(Topology::DataTopicSubscribed {
                    route: Some(_),
                    ..
                }))
            ),
            "the first subscriber asks for a route and hands over the handle once granted"
        );
        assert_eq!(table.live_routes(), 1);

        let h2 = new_participant_key();
        table.add_local_member(h2, room, &mut rng);
        assert!(
            table
                .register_data_subscriber(room, h2, topic, Some(publisher), now(), &wall())
                .is_none(),
            "local churn must not touch the cluster route"
        );
        assert_eq!(table.live_routes(), 1);
    }

    /// An install can fail — today only at the table's cap, cross-node whenever
    /// the peer is unreachable — and the import must come back to Absent when
    /// it does.
    ///
    /// The failure mode this pins is silent and permanent: `subscribe` moves to
    /// `Installing` before a route exists, and `Installing` absorbs every later
    /// subscribe and ignores every unsubscribe. Without the rollback the stream
    /// is undeliverable on this shard for the process's lifetime, so recovery
    /// once capacity returns is the property, not the error itself.
    #[test]
    fn a_failed_install_leaves_the_stream_installable_again() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        table.starve_routes();
        let mut rng = rand::seeded_rng(23);
        let room = room_id("data-room");
        let handle = new_participant_key();
        table.add_local_member(handle, room, &mut rng);

        let publisher = pid();
        let topic = Topic::for_test("chat");
        table.register_data_subscriber(room, handle, topic.clone(), Some(publisher), now(), &wall());
        assert!(
            settled(&mut table).is_none(),
            "the grant failed, so there is nothing to announce"
        );
        assert_eq!(table.live_routes(), 0);

        table.unstarve_routes();
        let h2 = new_participant_key();
        table.add_local_member(h2, room, &mut rng);
        table.register_data_subscriber(room, h2, topic, Some(publisher), now(), &wall());
        assert!(
            matches!(
                settled(&mut table),
                Some(ShardEvent::Relay(Topology::DataTopicSubscribed {
                    route: Some(_),
                    ..
                }))
            ),
            "the next subscriber asks again rather than joining a pending request"
        );
        assert_eq!(table.live_routes(), 1);
    }

    /// A destination's measurements arrive whole, by message, and replace what
    /// was there. There is no handle to go stale: a republished track under the
    /// same `TrackId` simply gets the next snapshot, and until it does the
    /// fanout has none rather than the previous incarnation's.
    #[test]
    fn a_stats_snapshot_replaces_what_the_fanout_held() {
        use crate::rtp::monitor::StreamStats;

        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(77);
        let room = room_id("republish");
        let publisher = pid();
        let track = TrackMeta {
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(TrackKind::Video, "v"),
            origin: publisher,
        };
        let handle = new_participant_key();
        table.add_local_member(handle, room, &mut rng);
        table.register_subscriber(handle, track.clone(), now(), &wall());

        let fanout_key = table.fanout_of(&track.id).expect("subscribing creates it");
        assert!(
            fanout(&table, &track.id).state_for(None).is_none(),
            "a fresh fanout has no measurements until a snapshot arrives"
        );

        table.apply_stats(
            fanout_key,
            vec![(None, StreamStats::new(false, 100_000, 0))],
            &mut RecordingCtx::default(),
        );
        assert_eq!(
            fanout(&table, &track.id)
                .state_for(None)
                .expect("snapshot applied")
                .bitrate_bps(),
            100_000.0
        );

        // A later snapshot replaces it wholesale — no field survives from the
        // previous one, which is what makes the view coherent.
        table.apply_stats(
            fanout_key,
            vec![(None, StreamStats::new(false, 4_321, 0))],
            &mut RecordingCtx::default(),
        );
        assert_eq!(
            fanout(&table, &track.id)
                .state_for(None)
                .expect("snapshot applied")
                .bitrate_bps(),
            4_321.0
        );
    }

    /// A track that has ended must not keep its fanout, and with it a
    /// `TrackStreamCache` — a 512-slot ring per encoding holding whole packets.
    /// Retaining one per departed publisher is a leak measured in hundreds of
    /// kilobytes each, and it grows for as long as the shard runs.
    #[test]
    fn an_ended_track_releases_its_fanout() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(53);
        let room = room_id("fanout-release");
        let publisher = pid();
        let track = TrackMeta {
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(TrackKind::Video, "v"),
            origin: publisher,
        };
        let handle = new_participant_key();
        table.add_local_member(handle, room, &mut rng);

        table.register_subscriber(handle, track.clone(), now(), &wall());
        assert_eq!(table.data.tracks.len(), 1, "subscribing creates the fanout");

        table.unregister_subscriber(handle, track.clone(), now());
        assert_eq!(
            table.data.tracks.len(),
            0,
            "losing the last consumer must release the fanout and its packet rings"
        );
        assert!(
            table.fanout_of(&track.id).is_none(),
            "the name index must be released with it"
        );
    }

    /// A published track's descriptor must outlive its last local subscriber
    /// — losing all consumers is not the same as being unpublished, and a
    /// late subscriber or a keyframe request both need the entry to still be
    /// there. Only the packet cache, which is real memory, is released early.
    #[test]
    fn a_published_track_keeps_its_descriptor_but_drops_its_cache_when_idle() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(54);
        let room = room_id("publish-survives-idle");
        let publisher = pid();
        let track_meta = TrackMeta {
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(TrackKind::Video, "v"),
            origin: publisher,
        };
        let track = video_track_with(&track_meta);

        table.open_track_reverse_route(track);
        assert!(
            !table.settle_routes(now(), &wall()).is_empty(),
            "publishing opens the reverse route and announces the track"
        );
        let fanout_key = table
            .fanout_of(&track_meta.id)
            .expect("publishing creates the fanout entry");

        let handle = new_participant_key();
        table.add_local_member(handle, room, &mut rng);
        table.register_subscriber(handle, track_meta.clone(), now(), &wall());
        table.route_video(
            fanout_key,
            RtpPacket::default(),
            &mut RecordingCtx::default(),
        );
        assert!(
            table.data.tracks[fanout_key].cache.is_some(),
            "a delivered packet must populate the cache"
        );

        table.unregister_subscriber(handle, track_meta.clone(), now());
        assert_eq!(
            table.data.tracks.len(),
            1,
            "a published track with zero subscribers must not be collected"
        );
        assert!(
            table.data.tracks[fanout_key].cache.is_none(),
            "losing every subscriber must still free the packet cache"
        );
        assert!(
            table.fanout_of(&track_meta.id).is_some(),
            "the name index must still resolve while the track is published"
        );

        table.close_track_reverse_route(&track_meta.id, now());
        table.unpublish_local_track(&track_meta.id);
        assert_eq!(
            table.data.tracks.len(),
            0,
            "unpublishing an idle track must release its descriptor too"
        );
    }

    /// A reliable topic gets a reverse route on the same terms as a track: one
    /// per published stream, resolving to the publisher and topic so an ack
    /// names neither.
    #[test]
    fn a_reliable_topic_gets_one_reverse_route() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(91);
        let room = room_id("reliable-reverse");
        let publisher = pid();
        let topic = Topic::for_test("chat");
        let handle = new_participant_key();
        table.add_local_member(handle, room, &mut rng);

        table.register_reliable_data_publisher(room, publisher, topic.clone());
        table.settle_routes(now(), &wall());
        let target = table
            .topic_reverse_handle(room, publisher, &topic)
            .expect("publishing a reliable topic opens its reverse route");
        assert_eq!(table.live_routes(), 1);

        let stream_id = DataStreamId::new(room, publisher, topic.clone());
        let stream_key = table
            .control
            .reliable_stream_keys
            .get(&stream_id)
            .copied()
            .expect("publishing a reliable topic must mint its arena key");
        assert!(
            matches!(
                table.resolve_reverse(RouteAction::Reverse {
                    target: ReverseTarget::Topic { stream: stream_key }
                }),
                Some((origin, ReverseTarget::Topic { stream }))
                    if origin == publisher && stream == stream_key
            ),
            "an ack resolves to its publisher and topic through the route alone"
        );
        assert_eq!(
            table.reliable_stream(stream_key).map(|s| &s.id),
            Some(&stream_id),
            "the arena entry must resolve back to its own name"
        );

        table.unregister_reliable_data_publisher(room, publisher, &topic, now());
        assert_eq!(
            table.live_routes(),
            0,
            "unpublishing must free the reverse route"
        );
        assert!(
            table.reliable_stream(stream_key).is_none(),
            "unpublishing must free the reliable stream key too"
        );
    }

    /// A shard that gains its first room member after a track was published can
    /// still address keyframe requests for it.
    ///
    /// The controller announces a track only to the shards holding room members
    /// at the time, so a shard joining later never runs `publish_track` for it
    /// and learns of it through the joining participant's known-track list
    /// instead. That path replayed the audio routes and nothing else, so the
    /// reverse target was never recorded and every keyframe request for the
    /// track was discarded for as long as it existed - silently, because the
    /// only thing noticing was a `debug_assert!` that release builds drop.
    #[test]
    fn a_late_joining_shard_can_address_keyframe_requests() {
        let mut publisher_shard = ShardRoutingTable::new(ShardId::new(0));
        let publisher = pid();
        let meta = TrackMeta {
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(TrackKind::Video, "v"),
            origin: publisher,
        };

        // The publishing shard stamps the descriptor, exactly as it does on
        // `ParticipantControlEvent::TrackPublished`.
        let mut track = video_track_with(&meta);
        publisher_shard.open_track_reverse_route(track.clone());
        publisher_shard.settle_routes(now(), &wall());
        track.reverse = publisher_shard.track_reverse_handle(&track.meta.id);
        assert!(
            track.reverse.is_some(),
            "publishing must open a reverse route"
        );

        // A different shard learns of the track only by a participant joining
        // after the fact, never through an announcement.
        let mut late_shard = ShardRoutingTable::new(ShardId::new(0));
        let room = room_id("late-join");
        assert!(
            late_shard.track_reverse_target(&meta.id, None).is_none(),
            "a shard that has not seen the track cannot address it yet"
        );

        late_shard.adopt_known_tracks(room, &[track], &|_| false, now(), &wall());

        assert!(
            late_shard.track_reverse_target(&meta.id, None).is_some(),
            "the late-joining shard cannot address keyframe requests for a track that was \
             already published, so every request it makes is dropped"
        );
    }

    /// The reverse direction must cost one route per track, not one per
    /// (track x subscribing shard). Route ids are 32 bits and the forward
    /// direction already pays per destination; letting feedback do the same
    /// would make it the largest consumer in the table for no benefit, since
    /// it is latest-wins and keeps no per-link state.
    #[test]
    fn feedback_costs_one_route_per_track_regardless_of_subscribers() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let publisher = pid();
        let track = TrackMeta {
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(TrackKind::Video, "v"),
            origin: publisher,
        };

        table.open_track_reverse_route(video_track_with(&track));
        table.settle_routes(now(), &wall());
        let target = table
            .track_reverse_handle(&track.id)
            .expect("publishing opens the reverse route");
        assert_eq!(table.live_routes(), 1);
        assert_eq!(
            fanout(&table, &track.id).encodings,
            vec![None],
            "opening the reverse route must also stamp the track's own descriptor"
        );

        let fanout_key = table
            .fanout_of(&track.id)
            .expect("track known to this shard");
        // Every subscribing shard addresses the same id.
        for shard in 1..8u8 {
            let _ = shard;
            assert!(
                matches!(
                    table.resolve_reverse(RouteAction::Reverse {
                        target: ReverseTarget::Track { track: fanout_key }
                    }),
                    Some((origin, ReverseTarget::Track { track }))
                        if origin == publisher && track == fanout_key
                ),
                "every subscriber resolves through the one route"
            );
        }
        assert_eq!(
            table.live_routes(),
            1,
            "subscriber count must not grow the reverse table"
        );

        table.close_track_reverse_route(&track.id, now());
        assert_eq!(
            table.live_routes(),
            0,
            "unpublishing must free the reverse route"
        );
    }

    /// A request already in flight when the track was unpublished must not land
    /// on whatever later takes that slot.
    #[test]
    fn feedback_on_a_retired_route_is_dropped() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let publisher = pid();
        let track = TrackMeta {
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(TrackKind::Video, "v"),
            origin: publisher,
        };

        table.open_track_reverse_route(video_track_with(&track));
        table.settle_routes(now(), &wall());
        let stale = table
            .track_reverse_handle(&track.id)
            .expect("publishing opens the reverse route");

        table.close_track_reverse_route(&track.id, now());

        assert!(
            table.track_reverse_handle(&track.id).is_none(),
            "an unpublished track keeps no reverse handle"
        );
        assert_eq!(
            table.live_routes(),
            0,
            "and its accounting goes with it, so a frame for {stale} has nothing to land on"
        );
    }

    /// Teardown must be idempotent under reordering. An unsubscribe names the
    /// route incarnation it is retiring, so one overtaken by a resubscription
    /// from the same shard is ignored rather than silently stopping the media
    /// the new subscription just asked for.
    #[test]
    fn a_stale_unsubscribe_does_not_tear_down_a_newer_route() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let publisher = pid();
        let track = TrackMeta {
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(TrackKind::Video, "v"),
            origin: publisher,
        };
        let subscriber_shard = ShardId::new(1);

        let stale = RouteId::from_raw(7);
        let fresh = RouteId::from_raw(9);

        // The destination resubscribed on a new route before its old
        // unsubscribe reached us.
        table.register_remote_subscriber_shard(
            RemoteRoute::new(subscriber_shard, fresh, 1),
            track.clone(),
        );
        assert_eq!(fanout(&table, &track.id).remote_routes.len(), 1);

        table.unregister_remote_subscriber_shard(subscriber_shard, track.clone(), stale, 0);
        assert_eq!(
            fanout(&table, &track.id).remote_routes.len(),
            1,
            "an unsubscribe naming a superseded route must be ignored"
        );

        table.unregister_remote_subscriber_shard(subscriber_shard, track.clone(), fresh, 1);
        assert!(
            fanout(&table, &track.id).remote_routes.is_empty(),
            "an unsubscribe naming the live route must retire it"
        );
    }

    /// Losing the last local consumer must retire the cluster route, not just
    /// drop the local fanout. Leaving the import Active pins the slot forever,
    /// so a later subscription can never allocate a fresh route and epoch —
    /// and the publisher keeps a handle for a destination that wants nothing.
    #[test]
    fn the_last_data_unsubscribe_retires_the_route() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(41);
        let room = room_id("data-retire");
        let publisher = pid();
        let topic = Topic::for_test("chat");

        let subscribe = |table: &mut ShardRoutingTable, rng: &mut _| -> ParticipantKey {
            let handle = new_participant_key();
            table.add_local_member(handle, room, rng);
            table.register_data_subscriber(
                room,
                handle,
                topic.clone(),
                Some(publisher),
                now(),
                &wall(),
            );
            table.settle_routes(now(), &wall());
            handle
        };

        let a = subscribe(&mut table, &mut rng);
        let b = subscribe(&mut table, &mut rng);
        assert_eq!(
            table.live_routes(),
            1,
            "one route serves both subscribers"
        );

        table.unregister_data_subscriber(room, a, &topic, Some(publisher), now());
        assert_eq!(
            table.live_routes(),
            1,
            "churn with a subscriber remaining must not touch the cluster route"
        );

        table.unregister_data_subscriber(room, b, &topic, Some(publisher), now());
        assert_eq!(
            table.live_routes(),
            0,
            "the last unsubscribe must retire the route"
        );

        // And the slot must be usable again: resubscribing installs a new one.
        subscribe(&mut table, &mut rng);
        assert_eq!(
            table.live_routes(),
            1,
            "a later subscription must be able to install a fresh route"
        );
    }

    /// A wildcard subscription cannot name a stream, so it installs nothing
    /// until a publisher is announced — then it resolves to a concrete route.
    #[test]
    fn a_wildcard_data_subscription_resolves_on_publisher_announcement() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(12);
        let room = room_id("data-wildcard");
        let handle = new_participant_key();
        table.add_local_member(handle, room, &mut rng);

        let topic = Topic::for_test("chat");
        let ev = table.register_data_subscriber(room, handle, topic.clone(), None, now(), &wall());
        assert!(
            matches!(
                ev,
                Some(ShardEvent::Relay(Topology::DataTopicSubscribed {
                    publisher: None,
                    route: None,
                    ..
                }))
            ),
            "a wildcard subscription has no stream to install a route for yet"
        );
        assert_eq!(table.live_routes(), 0);

        let publisher = pid();
        table.on_remote_data_publisher(room, publisher, &topic, now(), &wall());
        let resolved = settled(&mut table);
        assert!(
            matches!(
                resolved,
                Some(ShardEvent::Relay(Topology::DataTopicSubscribed {
                    publisher: Some(_),
                    route: Some(_),
                    ..
                }))
            ),
            "the announcement resolves the wildcard into a concrete route"
        );
        assert_eq!(table.live_routes(), 1);
    }

    /// Audio gets one route per (stream, destination). Membership in the room
    /// is the subscription, so the destination installs on learning the track
    /// exists and retires when it has nobody left to deliver to.
    #[test]
    fn an_audio_route_is_installed_per_stream_and_retired_with_the_room() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(7);
        let room = room_id("audio-room");
        let local = pid();
        let handle = new_participant_key();
        table.add_local_member(handle, room, &mut rng);

        let remote_origin = pid();
        let audio = TrackMeta {
            shard_id: ShardId::new(1),
            id: remote_origin.derive_track_id(TrackKind::Audio, "a"),
            origin: remote_origin,
        };
        let track = Track {
            meta: audio.clone(),
            layers: Vec::new(),
            reverse: None,
        };

        let mut ctx = RecordingCtx {
            ..Default::default()
        };
        table.publish_track(track, room, None, now(), &wall(), &mut ctx);
        let ev = settled(&mut table);
        assert!(
            matches!(ev, Some(ShardEvent::Relay(Topology::TrackSubscribed { track: t, .. })) if t == audio),
            "a remote audio publish gets a destination route"
        );
        assert_eq!(table.live_routes(), 1);

        table.remove_local_member(&local, handle, room, std::iter::empty(), now());
        assert_eq!(
            table.live_routes(),
            0,
            "no members left means nothing to deliver to"
        );
    }

    /// A publisher who leaves and returns under the same id does not collide with itself.
    ///
    /// This is what a reconnect is: the participant id is deliberately kept stable so everyone
    /// else sees a recovery rather than a departure and a stranger. Anything the SFU keys on that
    /// id therefore has to be gone by the time they come back. Data routes were not: `is_unused`
    /// keeps a route that is still marked published, so it outlived its publisher, and the
    /// returning participant re-registering the same topic tripped
    /// `debug_assert!(!route.published)` - or, in release, left a route published by somebody who
    /// was not in the room.
    #[test]
    fn a_publisher_can_return_under_the_same_id() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(7);
        let room = room_id("rejoin-room");
        let participant = pid();
        let topic = Topic::for_test("chat");

        // Somebody has to stay, or the room empties and takes its routes with it - which would
        // make this pass for the wrong reason.
        let bystander_handle = new_participant_key();
        table.add_local_member(bystander_handle, room, &mut rng);

        for _ in 0..2 {
            let handle = new_participant_key();
            table.add_local_member(handle, room, &mut rng);
            // The same participant id comes back, exactly as a reconnect does.
            table.register_data_publisher(room, participant, topic.clone());
            table.remove_local_member(&participant, handle, room, std::iter::empty(), now());
        }
    }

    #[test]
    fn a_locally_published_audio_track_installs_no_route() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let mut rng = rand::seeded_rng(7);
        let room = room_id("audio-room-local");
        let origin = pid();
        let handle = new_participant_key();
        table.add_local_member(handle, room, &mut rng);

        let audio = TrackMeta {
            shard_id: ShardId::new(0),
            id: origin.derive_track_id(TrackKind::Audio, "a"),
            origin,
        };
        let mut ctx = RecordingCtx {
            local: StdHashSet::from_iter([origin]),
            ..Default::default()
        };
        let ev = table.publish_track(
            Track {
                meta: audio,
                layers: Vec::new(),
                reverse: None,
            },
            room,
            Some(handle),
            now(),
            &wall(),
            &mut ctx,
        );
        assert!(ev.is_none(), "a local publisher needs no cluster route");
        assert_eq!(table.live_routes(), 0);
    }

    /// The destination allocates a route, the publisher receives the handle,
    /// and only then does media flow — addressed by route, not by track id.
    #[test]
    fn a_route_is_installed_once_and_retired_with_the_last_subscriber() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let track = TrackMeta {
            shard_id: ShardId::new(1),
            id: pid().derive_track_id(TrackKind::Video, "v"),
            origin: pid(),
        };
        let (first, second) = (pid(), pid());
        let first_key = add_local_subscriber(&mut table, first);
        let second_key = add_local_subscriber(&mut table, second);

        table.register_subscriber(first_key, track.clone(), now(), &wall());
        let Some(ShardEvent::Relay(Topology::TrackSubscribed { route, epoch, .. })) =
            settled(&mut table)
        else {
            panic!("the first subscriber must get a route");
        };
        assert_eq!(table.live_routes(), 1);

        table.register_subscriber(second_key, track.clone(), now(), &wall());
        assert!(
            settled(&mut table).is_none(),
            "local churn must not touch the cluster route"
        );
        assert_eq!(table.live_routes(), 1, "exactly one route installation");

        assert!(
            table
                .unregister_subscriber(first_key, track.clone(), now())
                .is_none()
        );
        assert_eq!(table.live_routes(), 1, "still one consumer left");

        assert!(
            table
                .unregister_subscriber(second_key, track, now())
                .is_some(),
            "the last subscriber leaving tells the publisher to stop"
        );
        // A frame still in flight for the retired incarnation must not land:
        // the accounting behind `(route, epoch)` is gone, and the published
        // view no longer carries it either — see `view::tests`.
        assert_eq!(table.live_routes(), 0, "the route is retired");
        assert!(
            table
                .route_accounting(RouteHandle::new(route, epoch))
                .is_none()
        );
    }

    #[test]
    fn route_video_forwards_to_subscribers_and_remote_shards() {
        let mut table = ShardRoutingTable::new(ShardId::new(0));
        let publisher = pid();
        let track_id = publisher.derive_track_id(TrackKind::Video, "v");
        let subscriber = pid();
        let subscriber_key = add_local_subscriber(&mut table, subscriber);

        table.register_subscriber(
            subscriber_key,
            TrackMeta {
                shard_id: ShardId::new(0),
                id: track_id,
                origin: publisher,
            },
            now(),
            &wall(),
        );
        // Stand in for a destination shard that installed a route and had its
        // handle acknowledged back to this publisher.
        table.register_remote_subscriber_shard(
            RemoteRoute::new(ShardId::new(3), RouteId::from_raw(0), 0),
            TrackMeta {
                shard_id: ShardId::new(0),
                id: track_id,
                origin: publisher,
            },
        );

        let mut ctx = RecordingCtx {
            wall: wall(),
            ..Default::default()
        };
        let fanout_key = table
            .fanout_of(&track_id)
            .expect("published track has a fanout");
        table.route_video(fanout_key, RtpPacket::default(), &mut ctx);

        assert_eq!(ctx.forwarded_video.borrow().as_slice(), &[subscriber_key]);
        assert_eq!(ctx.sent.borrow().len(), 1);
    }
}
