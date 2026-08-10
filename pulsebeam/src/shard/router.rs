use std::collections::VecDeque;

use ahash::{HashMap, HashMapExt};
use indexmap::IndexSet;
use pulsebeam_runtime::rand;
use str0m::media::{KeyframeRequestKind, Rid};

use super::events::{AudioRtpEvent, ParticipantControlEvent, ParticipantTopologyEvent};
use super::participants::ParticipantHandle;
use super::reliable::ReliableRoutes;
use slotmap::{SlotMap, new_key_type};

use crate::audio_selector::TopNAudioSelector;
use crate::clock::WallAnchor;
use crate::entity::{ParticipantId, RoomId, TrackId, TrackKind};
use crate::id::{AudioSelectorSlotId, ShardId};
use crate::rtp::{RtpPacket, cache::TrackStreamCache};
use crate::track::{DataLane, Topic, Track, TrackMeta};
use tokio::time::Instant;

use super::worker::{MediaPayload, Reverse, ShardEvent, ShardFrame, Topology};
use crate::route::{
    ImportEffect, ImportTable, MediaEnvelope, RemoteRoute, ReverseRoute, ReverseTarget,
    RouteAction, RouteEnvelope, RouteId, RouteNames, RouteTable,
};

type FastIndexSet<T> = IndexSet<T, ahash::RandomState>;

fn fast_set<T>() -> FastIndexSet<T> {
    IndexSet::with_hasher(ahash::RandomState::default())
}

fn fast_set_with_capacity<T>(cap: usize) -> FastIndexSet<T> {
    IndexSet::with_capacity_and_hasher(cap, ahash::RandomState::default())
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
        subscriber: ParticipantHandle,
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
        subscriber: ParticipantHandle,
        track_id: TrackId,
        states: &crate::track::TrackStates,
    );
    fn forward_audio_rtp(
        &mut self,
        subscriber: ParticipantHandle,
        slot_idx: AudioSelectorSlotId,
        pkt: &RtpPacket,
    );
    fn forward_sctp(
        &mut self,
        subscriber: ParticipantHandle,
        origin: ParticipantId,
        topic: &Topic,
        pkt: &[u8],
    );
    fn notify_tracks_published(&mut self, participant_id: ParticipantId, tracks: &[Track]);
    fn notify_tracks_unpublished(&mut self, participant_id: ParticipantId, track_ids: &[TrackId]);
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
        subscriber: ParticipantHandle,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParticipantShardMeta {
    pub shard_id: ShardId,
    pub room_id: RoomId,
}

pub(crate) struct ShardRoomContext {
    pub members: FastIndexSet<ParticipantHandle>,
    pub remote_shards: FastIndexSet<ShardId>,
    /// Audio tracks this shard has installed a destination route for, so they
    /// can be retired when the room goes away.
    pub audio_imports: FastIndexSet<TrackId>,
    pub audio_selector: TopNAudioSelector,
    pub data_streams: HashMap<DataStreamId, DataStreamRoute>,
    pub all_publisher_subscriptions: AllPublisherSubscriptions,
    reliable: ReliableRoutes,
}

impl ShardRoomContext {
    fn new(rng: &mut impl rand::RngCore) -> Self {
        Self {
            members: fast_set(),
            remote_shards: fast_set(),
            audio_imports: fast_set(),
            audio_selector: TopNAudioSelector::new(rng),
            data_streams: HashMap::default(),
            all_publisher_subscriptions: AllPublisherSubscriptions::new(),
            reliable: ReliableRoutes::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DataStreamId {
    publisher_id: ParticipantId,
    topic: Topic,
}

impl DataStreamId {
    fn new(publisher_id: ParticipantId, topic: Topic) -> Self {
        Self {
            publisher_id,
            topic,
        }
    }
}

pub(crate) struct AllPublisherSubscriptions {
    local_by_topic: HashMap<Topic, FastIndexSet<ParticipantHandle>>,
    remote_by_topic: HashMap<Topic, FastIndexSet<ShardId>>,
}

impl AllPublisherSubscriptions {
    fn new() -> Self {
        Self {
            local_by_topic: HashMap::default(),
            remote_by_topic: HashMap::default(),
        }
    }
}

/// A destination's acknowledged handle plus how many local subscriptions
/// (explicit and wildcard) reference it.
struct RemoteDataSubscriber {
    remote: RemoteRoute,
    refs: usize,
}

pub(crate) struct DataStreamRoute {
    published: bool,
    local_subscribers: FastIndexSet<ParticipantHandle>,
    remote_subscriber_shards: HashMap<ShardId, RemoteDataSubscriber>,
}

impl DataStreamRoute {
    fn new() -> Self {
        Self {
            published: false,
            local_subscribers: fast_set_with_capacity(256),
            remote_subscriber_shards: HashMap::default(),
        }
    }

    fn is_unused(&self) -> bool {
        !self.published
            && self.local_subscribers.is_empty()
            && self.remote_subscriber_shards.is_empty()
    }

    fn attach_remote_subscriber_shard(&mut self, remote: RemoteRoute) {
        match self.remote_subscriber_shards.get_mut(&remote.shard_id) {
            Some(existing) => {
                existing.refs += 1;
                debug_assert!(existing.refs <= 2);
                // A reinstall at the destination supersedes the old incarnation.
                if existing.remote.route != remote.route || existing.remote.epoch != remote.epoch {
                    existing.remote = remote;
                }
            }
            None => {
                self.remote_subscriber_shards
                    .insert(remote.shard_id, RemoteDataSubscriber { remote, refs: 1 });
            }
        }
    }

    fn detach_remote_subscriber_shard(&mut self, shard_id: ShardId) {
        let Some(entry) = self.remote_subscriber_shards.get_mut(&shard_id) else {
            debug_assert!(false, "detaching an unknown remote subscriber shard");
            return;
        };
        debug_assert!(entry.refs > 0, "refcount underflow would leak this route");
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs == 0 {
            self.remote_subscriber_shards.remove(&shard_id);
        }
    }
}

pub(crate) struct TrackRoute {
    /// The track this fanout serves. Carried for the downstream slot match and
    /// for logs — never hashed to find this object, which is the whole point of
    /// addressing it by key.
    pub track_id: TrackId,
    pub subscribers: Vec<ParticipantHandle>,
    /// Measurement handles for the publisher's encodings. Reaches this shard
    /// along the media path — from the local publisher, or from the publisher's
    /// shard on subscribe — never through the controller.
    layer_states: crate::track::TrackStates,
    /// Acknowledged sender handles, one per destination shard. A destination
    /// only appears here once it has installed its route, so the presence of a
    /// handle is what permits media to flow.
    pub remote_routes: Vec<RemoteRoute>,
    /// Where to send keyframe requests for this track, when it is published on
    /// another shard. `None` for a locally published track: its requests are
    /// dispatched in-process and never addressed.
    reverse: Option<ReverseRoute>,
    /// Encoding order for this track, so a reverse frame can name one by index.
    encodings: Vec<Option<Rid>>,
    cache: TrackStreamCache,
}

impl TrackRoute {
    #[cfg(test)]
    fn state_for(&self, rid: Option<Rid>) -> Option<&crate::rtp::monitor::StreamStats> {
        self.layer_states
            .iter()
            .find(|(r, _)| *r == rid)
            .map(|(_, s)| s)
    }

    fn new(track_id: TrackId) -> Self {
        Self {
            track_id,
            subscribers: Vec::with_capacity(256),
            layer_states: Vec::new(),
            reverse: None,
            encodings: Vec::new(),
            remote_routes: Vec::new(),
            cache: TrackStreamCache::new(),
        }
    }
}

/// Pure pub/sub state for a shard: which participants are in which rooms,
/// which shards subscribe to which tracks, and where remote participants
/// live.
pub(crate) struct ShardRoutingTable {
    pub rooms: HashMap<RoomId, ShardRoomContext>,
    /// Fanout objects, addressed densely. Arrivals resolve to a key, never a
    /// name: a `TrackId` is a 17-byte value to hash, a key is an index.
    pub tracks: SlotMap<LocalTrackKey, TrackRoute>,
    /// Names to keys. The control plane's index — read when a track is
    /// published, subscribed or torn down, never per packet.
    track_keys: HashMap<TrackId, LocalTrackKey>,
    // Invariant: `track_keys` and `tracks` are created and removed together, so
    // a key handed to a route always resolves.
    /// Routes this shard has installed as a *destination*, indexed by the id it
    /// handed out. Frames arriving from other shards resolve here.
    pub routes: RouteTable,
    /// Lifecycle of each stream imported from another shard, deciding when a
    /// cluster route is installed and retired.
    pub imports: ImportTable<TrackId>,
    pub data_imports: ImportTable<DataStreamId>,
    /// Separate from `data_imports`: the same (publisher, topic) can exist on
    /// both the realtime and reliable lanes and needs its own route.
    pub reliable_imports: ImportTable<DataStreamId>,
    participant_shards: HashMap<ParticipantId, ParticipantShardMeta>,
    local_participants: HashMap<ParticipantId, ParticipantHandle>,
    remote_participant_counts: HashMap<(RoomId, ShardId), usize>,
    /// Reverse routes this shard opened for the streams it publishes, so they
    /// can be retired when those streams go away.
    track_reverse_routes: HashMap<TrackId, RouteId>,
    topic_reverse_routes: HashMap<DataStreamId, ReverseRoute>,
    /// Handles for reverse routes *other* shards opened, learned from publisher
    /// announcements — the addresses this shard sends acks to.
    topic_reverse_targets: HashMap<DataStreamId, ReverseRoute>,
}

impl ShardRoutingTable {
    /// The fanout for a track name, creating it if this shard has not seen the
    /// track before. Control path only.
    fn fanout_key(&mut self, track_id: TrackId) -> LocalTrackKey {
        if let Some(&key) = self.track_keys.get(&track_id) {
            return key;
        }
        let key = self.tracks.insert(TrackRoute::new(track_id));
        self.track_keys.insert(track_id, key);
        key
    }

    /// The key for a track this shard already knows about.
    pub fn fanout_of(&self, track_id: &TrackId) -> Option<LocalTrackKey> {
        self.track_keys.get(track_id).copied()
    }

    /// Release a track's fanout once nothing consumes it.
    ///
    /// Worth doing rather than leaving to the map: a `TrackRoute` owns a
    /// `TrackStreamCache`, which is a 512-slot ring per encoding holding whole
    /// packets. A track that has ended pins that until the shard does, so
    /// leaving them behind is hundreds of kilobytes per departed publisher.
    fn release_fanout_if_idle(&mut self, track_id: &TrackId) {
        let Some(&key) = self.track_keys.get(track_id) else {
            return;
        };
        let Some(route) = self.tracks.get(key) else {
            self.track_keys.remove(track_id);
            return;
        };
        if route.subscribers.is_empty() && route.remote_routes.is_empty() {
            self.tracks.remove(key);
            self.track_keys.remove(track_id);
        }
    }

    pub fn new() -> Self {
        Self {
            rooms: HashMap::new(),
            tracks: SlotMap::with_key(),
            track_keys: HashMap::new(),
            routes: RouteTable::new(),
            imports: ImportTable::new(),
            data_imports: ImportTable::new(),
            reliable_imports: ImportTable::new(),
            participant_shards: HashMap::new(),
            local_participants: HashMap::new(),
            remote_participant_counts: HashMap::new(),
            track_reverse_routes: HashMap::new(),
            topic_reverse_routes: HashMap::new(),
            topic_reverse_targets: HashMap::new(),
        }
    }

    // -- local room membership -------------------------------------------

    pub fn add_local_member(
        &mut self,
        participant_id: ParticipantId,
        handle: ParticipantHandle,
        room_id: RoomId,
        rng: &mut impl rand::RngCore,
    ) {
        debug_assert_eq!(participant_id, handle.participant_id());
        let previous = self.local_participants.insert(participant_id, handle);
        debug_assert!(previous.is_none(), "duplicate local participant route");
        self.rooms
            .entry(room_id)
            .or_insert_with(|| ShardRoomContext::new(rng))
            .members
            .insert(handle);
    }

    /// Removes a local participant from its room and evicts its audio
    /// tracks from the room's selector. Cleans up the room entry if it's
    /// now empty of both local and remote members.
    pub fn remove_local_member(
        &mut self,
        participant_id: &ParticipantId,
        room_id: RoomId,
        audio_track_ids: impl IntoIterator<Item = TrackId>,
        now: Instant,
    ) {
        let removed_handle = self.local_participants.remove(participant_id);
        debug_assert!(removed_handle.is_some());
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        let Some(removed_handle) = removed_handle else {
            return;
        };
        room.members.swap_remove(&removed_handle);
        for subscribers in room.all_publisher_subscriptions.local_by_topic.values_mut() {
            subscribers.swap_remove(&removed_handle);
        }
        room.all_publisher_subscriptions
            .local_by_topic
            .retain(|_, subscribers| !subscribers.is_empty());
        for route in room.data_streams.values_mut() {
            route.local_subscribers.swap_remove(&removed_handle);
        }
        room.data_streams.retain(|_, route| !route.is_unused());
        room.reliable.remove_participant(removed_handle);
        for id in audio_track_ids {
            room.audio_selector.remove_track((id, None));
        }
        // With nobody left to deliver to, the shard stops being a destination
        // for this room's audio.
        if room.members.is_empty() {
            self.retire_room_audio_routes(room_id, now);
        }
        let Some(room) = self.rooms.get(&room_id) else {
            return;
        };
        if room.members.is_empty() && room.remote_shards.is_empty() {
            self.rooms.remove(&room_id);
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
    pub fn open_track_reverse_route(
        &mut self,
        track: &Track,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ReverseRoute> {
        let handle = self.open_reverse_route(
            track.meta.origin,
            ReverseTarget::Track {
                track_id: track.meta.id,
                encodings: track.layers.iter().map(|l| l.rid).collect(),
            },
            RouteNames {
                room_id: None,
                origin: track.meta.origin,
                track_id: Some(track.meta.id),
                topic: None,
            },
            now,
            wall,
        )?;
        self.track_reverse_routes
            .insert(track.meta.id, handle.route);
        Some(handle)
    }

    /// The same, for a reliable data topic this shard publishes. Its reverse
    /// traffic is the application's own retransmission protocol.
    pub fn open_topic_reverse_route(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ReverseRoute> {
        let key = DataStreamId::new(publisher, topic.clone());
        let handle = self.open_reverse_route(
            publisher,
            ReverseTarget::Topic {
                room_id,
                topic: topic.clone(),
            },
            RouteNames {
                room_id: Some(room_id),
                origin: publisher,
                track_id: None,
                topic: Some(topic),
            },
            now,
            wall,
        )?;
        self.topic_reverse_routes.insert(key, handle);
        Some(handle)
    }

    fn open_reverse_route(
        &mut self,
        origin: ParticipantId,
        target: ReverseTarget,
        names: RouteNames,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ReverseRoute> {
        let (route, epoch) = self
            .routes
            .install(
                RouteAction::Reverse { origin, target },
                names,
                wall.ntp(),
                now,
            )
            .inspect_err(|err| tracing::error!(?err, "reverse route install failed"))
            .ok()?;
        Some(ReverseRoute { route, epoch })
    }

    /// Close a track's reverse path when its publisher goes away.
    pub fn close_track_reverse_route(&mut self, track_id: &TrackId, now: Instant) {
        if let Some(route) = self.track_reverse_routes.remove(track_id) {
            self.routes.retire(route, now);
        }
    }

    pub fn close_topic_reverse_route(
        &mut self,
        publisher: ParticipantId,
        topic: &Topic,
        now: Instant,
    ) {
        let key = DataStreamId::new(publisher, topic.clone());
        if let Some(handle) = self.topic_reverse_routes.remove(&key) {
            self.routes.retire(handle.route, now);
        }
    }

    /// The reverse handle this shard opened for a topic it publishes, so a
    /// late-arriving subscriber can be told about it.
    pub fn topic_reverse_handle(
        &self,
        publisher: ParticipantId,
        topic: &Topic,
    ) -> Option<ReverseRoute> {
        self.topic_reverse_routes
            .get(&DataStreamId::new(publisher, topic.clone()))
            .copied()
    }

    /// Learn where to send acks for a topic another shard publishes.
    pub fn learn_topic_reverse_target(
        &mut self,
        publisher: ParticipantId,
        topic: &Topic,
        reverse: Option<ReverseRoute>,
    ) {
        let key = DataStreamId::new(publisher, topic.clone());
        match reverse {
            Some(handle) => {
                self.topic_reverse_targets.insert(key, handle);
            }
            None => {
                self.topic_reverse_targets.remove(&key);
            }
        }
    }

    /// Resolve an arriving feedback frame to the publisher it is aimed at.
    ///
    /// The epoch check is what makes a recycled slot safe: a request in flight
    /// when a track was unpublished must not land on whatever took its place.
    pub fn resolve_reverse(
        &self,
        route: RouteId,
        epoch: u16,
    ) -> Option<(ParticipantId, &ReverseTarget)> {
        match self.routes.resolve_action(route, epoch)? {
            RouteAction::Reverse { origin, target } => Some((*origin, target)),
            other => {
                debug_assert!(false, "a reverse frame arrived on a {other:?} route");
                None
            }
        }
    }

    /// Where this shard sends reverse traffic for a track it subscribes to,
    /// and the index it must use to name `rid` — both from the descriptor the
    /// control plane handed it.
    pub fn track_reverse_target(
        &self,
        track_id: &TrackId,
        rid: Option<Rid>,
    ) -> Option<(ReverseRoute, u8)> {
        let entry = self.tracks.get(self.fanout_of(track_id)?)?;
        let handle = entry.reverse?;
        let layer = entry.encodings.iter().position(|r| *r == rid)?;
        Some((handle, u8::try_from(layer).ok()?))
    }

    /// Record the measurements a publisher's shard sent for a track this shard
    /// receives. Wholesale, because a snapshot only means anything intact.
    pub fn apply_stats(
        &mut self,
        fanout: LocalTrackKey,
        stats: crate::track::TrackStates,
        ctx: &mut impl RoutingContext,
    ) {
        let Some(route) = self.tracks.get_mut(fanout) else {
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
        let Some(&key) = self.track_keys.get(&track_id) else {
            return Vec::new();
        };
        let Some(route) = self.tracks.get_mut(key) else {
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
                    RouteEnvelope {
                        route: remote.route,
                        epoch: remote.epoch,
                    },
                )
            })
            .collect()
    }

    /// A local participant published a track: register its measurement handles
    /// on the node so any shard that later subscribes can resolve them.
    pub fn publish_local_track(&mut self, track_id: TrackId, states: crate::track::TrackStates) {
        let key = self.fanout_key(track_id);
        self.tracks[key].layer_states = states;
    }

    pub fn unpublish_local_track(&mut self, track_id: &TrackId) {
        self.release_fanout_if_idle(track_id);
    }

    pub fn remote_shard_for(&self, participant_id: &ParticipantId) -> Option<ShardId> {
        self.participant_shards
            .get(participant_id)
            .map(|m| m.shard_id)
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

        if self.participant_shards.get(&participant_id).copied() == Some(meta) {
            return;
        }

        if let Some(previous) = self.participant_shards.remove(&participant_id) {
            self.release_remote_count(previous);
        }

        self.participant_shards.insert(participant_id, meta);
        self.rooms
            .entry(room_id)
            .or_insert_with(|| ShardRoomContext::new(rng))
            .remote_shards
            .insert(shard_id);
        *self
            .remote_participant_counts
            .entry((room_id, shard_id))
            .or_insert(0) += 1;
    }

    pub fn unregister_remote_participant(
        &mut self,
        participant_id: ParticipantId,
        expected: ParticipantShardMeta,
    ) {
        let Some(current) = self.participant_shards.get(&participant_id).copied() else {
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
        self.participant_shards.remove(&participant_id);
        self.release_remote_count(current);
    }

    fn release_remote_count(&mut self, meta: ParticipantShardMeta) {
        let key = (meta.room_id, meta.shard_id);
        let should_remove_shard = match self.remote_participant_counts.get_mut(&key) {
            Some(count) => {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.remote_participant_counts.remove(&key);
                    true
                } else {
                    false
                }
            }
            None => true,
        };

        if !should_remove_shard {
            return;
        }

        if let Some(room) = self.rooms.get_mut(&meta.room_id) {
            room.remote_shards.swap_remove(&meta.shard_id);
            if room.members.is_empty() && room.remote_shards.is_empty() {
                self.rooms.remove(&meta.room_id);
            }
        }
    }

    // -- track subscription topology (local subscribers) -----------------

    /// Registers a local subscriber for `track`. Returns a `ShardEvent` iff
    /// this is the *first* subscriber, so the caller can notify the
    /// publisher shard to start forwarding.
    pub fn register_subscriber(
        &mut self,
        subscriber: ParticipantId,
        track: TrackMeta,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ShardEvent> {
        let handle = *self.local_participants.get(&subscriber)?;
        debug_assert_eq!(handle.participant_id(), subscriber);
        // Resolve the publisher's handles from the node rather than waiting for
        // them to be sent: they are ready before any subscribe can happen, so
        // the fanout is never briefly live with no measurements behind it.
        // Measurements arrive by message from the publisher's shard, so a fresh
        // fanout simply starts empty and fills on the next snapshot. Nothing is
        // read out of another shard's memory to seed it.
        let key = self.fanout_key(track.id);
        let entry = &mut self.tracks[key];
        let already_subscribed = entry
            .subscribers
            .iter()
            .any(|existing| existing.participant_id() == subscriber);
        entry
            .subscribers
            .retain(|existing| existing.participant_id() != subscriber);
        entry.subscribers.push(handle);
        if already_subscribed {
            return None;
        }

        // The local fanout object (`TrackRoute`) exists before the route is
        // installed, so an installed route always resolves to something.
        if self.imports.subscribe(track.id) != ImportEffect::Install {
            return None;
        }
        let installed = self.routes.install(
            RouteAction::Video { local_track: key },
            RouteNames {
                room_id: None,
                origin: track.origin,
                track_id: Some(track.id),
                topic: None,
            },
            wall.ntp(),
            now,
        );
        let (route, epoch) = match installed {
            Ok(installed) => installed,
            Err(err) => {
                tracing::error!(?err, track_id = %track.id, "video route install failed");
                self.imports.cancel_install(&track.id);
                return None;
            }
        };
        self.imports.on_installed(&track.id, route, epoch);
        Some(ShardEvent::Relay(Topology::TrackSubscribed {
            track,
            route,
            epoch,
        }))
    }

    /// Returns a `ShardEvent` iff this was the *last* local subscriber, so
    /// the caller can tell the publisher shard to stop forwarding.
    pub fn unregister_subscriber(
        &mut self,
        subscriber: ParticipantId,
        track: TrackMeta,
        now: Instant,
    ) -> Option<ShardEvent> {
        let entry = self
            .tracks
            .get_mut(self.track_keys.get(&track.id).copied()?)?;
        let previous_len = entry.subscribers.len();
        entry
            .subscribers
            .retain(|handle| handle.participant_id() != subscriber);
        if entry.subscribers.len() == previous_len {
            return None;
        }

        // Retire the destination-side route only when the last local consumer
        // leaves; everything before that is churn the cluster never sees. The
        // retired incarnation is named in the unsubscribe so the publisher can
        // tell it apart from a resubscription that overtook it.
        let retired = match self.imports.unsubscribe(&track.id) {
            ImportEffect::Retire { route, epoch } => {
                self.routes.retire(route, now);
                self.imports.on_retired(&track.id);
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

    pub fn handle_topology_event(
        &mut self,
        ev: ParticipantTopologyEvent,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ShardEvent> {
        match ev {
            ParticipantTopologyEvent::TrackSubscribed { track, subscriber } => {
                self.register_subscriber(subscriber, track, now, wall)
            }
            ParticipantTopologyEvent::TrackUnsubscribed { track, subscriber } => {
                self.unregister_subscriber(subscriber, track, now)
            }
        }
    }

    pub fn register_data_publisher(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    ) {
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        let all_publisher_subscribers = room
            .all_publisher_subscriptions
            .local_by_topic
            .get(&topic)
            .cloned()
            .unwrap_or_else(fast_set);
        let route = room
            .data_streams
            .entry(DataStreamId::new(publisher, topic))
            .or_insert_with(DataStreamRoute::new);
        debug_assert!(!route.published);
        route.published = true;
        for subscriber in all_publisher_subscribers {
            route.local_subscribers.insert(subscriber);
        }
        // Remote wildcard subscribers are not attached here: a destination must
        // allocate its own route, so it is announced to and hands a handle back.
    }

    pub fn unregister_data_publisher(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
    ) {
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        let key = DataStreamId::new(publisher, topic.clone());
        let Some(route) = room.data_streams.get_mut(&key) else {
            debug_assert!(false, "unregistering an unknown data stream");
            return;
        };
        debug_assert!(route.published);
        route.published = false;
        if let Some(subscribers) = room.all_publisher_subscriptions.local_by_topic.get(topic) {
            for subscriber in subscribers {
                route.local_subscribers.swap_remove(subscriber);
            }
        }
        if route.is_unused() {
            room.data_streams.remove(&key);
        }
    }

    pub fn register_data_subscriber(
        &mut self,
        room_id: RoomId,
        subscriber: ParticipantId,
        topic: Topic,
        publisher: Option<ParticipantId>,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ShardEvent> {
        let handle = self.local_participants.get(&subscriber).copied()?;
        let room = self.rooms.get_mut(&room_id)?;
        match publisher {
            Some(publisher) => {
                let route = room
                    .data_streams
                    .entry(DataStreamId::new(publisher, topic.clone()))
                    .or_insert_with(DataStreamRoute::new);
                let was_empty = route.local_subscribers.is_empty();
                route.local_subscribers.insert(handle);
                if !was_empty {
                    return None;
                }
                // The local fanout entry exists before the route is installed.
                let Some((route, epoch)) =
                    self.install_data_route(room_id, publisher, &topic, now, wall)
                else {
                    // Undo the membership too. `was_empty` is what decides
                    // whether an install is attempted, so leaving this
                    // subscriber behind would make every later one look like
                    // local churn and skip the retry.
                    self.drop_data_subscriber(room_id, publisher, &topic, handle);
                    return None;
                };
                Some(ShardEvent::Relay(Topology::DataTopicSubscribed {
                    room_id,
                    topic,
                    publisher: Some(publisher),
                    route: Some(route),
                    epoch,
                }))
            }
            None => {
                let subscribers = room
                    .all_publisher_subscriptions
                    .local_by_topic
                    .entry(topic.clone())
                    .or_insert_with(fast_set);
                let was_empty = subscribers.is_empty();
                let inserted = subscribers.insert(handle);
                debug_assert!(inserted);
                let mut already_published = Vec::new();
                for (stream_id, route) in &mut room.data_streams {
                    if route.published && stream_id.topic == topic {
                        route.local_subscribers.insert(handle);
                        already_published.push(stream_id.publisher_id);
                    }
                }
                // Locally published streams need no cluster route; remote ones
                // arrive as announcements once the publisher shard learns of
                // this wildcard subscription.
                let _ = already_published;
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
        subscriber: ParticipantId,
        topic: &Topic,
        publisher: Option<ParticipantId>,
        now: Instant,
    ) -> bool {
        let Some(handle) = self.local_participants.get(&subscriber).copied() else {
            return false;
        };
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return false;
        };
        // Publishers whose destination route this shard no longer needs. Held
        // until the room borrow ends, since retiring touches the route table.
        let mut orphaned: Vec<ParticipantId> = Vec::new();
        let was_one = match publisher {
            Some(publisher) => {
                let key = DataStreamId::new(publisher, topic.clone());
                let Some(route) = room.data_streams.get_mut(&key) else {
                    return false;
                };
                let was_one =
                    route.local_subscribers.len() == 1 && route.local_subscribers.contains(&handle);
                route.local_subscribers.swap_remove(&handle);
                if route.is_unused() {
                    room.data_streams.remove(&key);
                    orphaned.push(publisher);
                }
                was_one
            }
            None => {
                let Some(subscribers) = room
                    .all_publisher_subscriptions
                    .local_by_topic
                    .get_mut(topic)
                else {
                    return false;
                };
                let was_one = subscribers.len() == 1 && subscribers.contains(&handle);
                subscribers.swap_remove(&handle);
                if subscribers.is_empty() {
                    room.all_publisher_subscriptions
                        .local_by_topic
                        .remove(topic);
                }
                // A wildcard resolved into one concrete route per publisher, so
                // dropping it can orphan several at once.
                for (stream_id, route) in &mut room.data_streams {
                    if stream_id.topic == *topic {
                        route.local_subscribers.swap_remove(&handle);
                        if route.is_unused() {
                            orphaned.push(stream_id.publisher_id);
                        }
                    }
                }
                room.data_streams
                    .retain(|stream_id, route| stream_id.topic != *topic || !route.is_unused());
                was_one
            }
        };

        // Losing the last local consumer is what retires the cluster route —
        // without this the import stays Active and its slot is never reusable,
        // so a later subscription cannot allocate a fresh route and epoch.
        for publisher in orphaned {
            self.retire_data_route(publisher, topic, now);
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
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return Vec::new();
        };
        match publisher {
            Some(publisher) => {
                let Some(remote) = remote else {
                    debug_assert!(false, "a concrete data subscription needs a route handle");
                    return Vec::new();
                };
                debug_assert_eq!(remote.shard_id, from_shard_id);
                let route = room
                    .data_streams
                    .entry(DataStreamId::new(publisher, topic))
                    .or_insert_with(DataStreamRoute::new);
                route.attach_remote_subscriber_shard(remote);
                Vec::new()
            }
            None => {
                let inserted = room
                    .all_publisher_subscriptions
                    .remote_by_topic
                    .entry(topic.clone())
                    .or_insert_with(fast_set)
                    .insert(from_shard_id);
                if !inserted {
                    return Vec::new();
                }
                room.data_streams
                    .iter()
                    .filter(|(id, route)| route.published && id.topic == topic)
                    .map(|(id, _)| id.publisher_id)
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
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        match publisher {
            Some(publisher) => {
                let key = DataStreamId::new(publisher, topic.clone());
                let Some(route) = room.data_streams.get_mut(&key) else {
                    return;
                };
                route.detach_remote_subscriber_shard(from_shard_id);
                if route.is_unused() {
                    room.data_streams.remove(&key);
                }
            }
            None => {
                let removed = if let Some(shards) = room
                    .all_publisher_subscriptions
                    .remote_by_topic
                    .get_mut(topic)
                {
                    let removed = shards.swap_remove(&from_shard_id);
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
                for (stream_id, route) in &mut room.data_streams {
                    if route.published && stream_id.topic == *topic {
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
        let key = self.fanout_key(track.id);
        let route = &mut self.tracks[key];
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
            .track_keys
            .get(&track.id)
            .copied()
            .and_then(|k| self.tracks.get_mut(k))
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
        let held = entry.remote_routes[idx];
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
        now: Instant,
        wall: &WallAnchor,
        ctx: &mut impl RoutingContext,
    ) -> Option<ShardEvent> {
        let publisher = track.meta.origin;
        let Some(_room) = self.rooms.get(&room_id) else {
            tracing::debug!(%room_id, "publish_track: room missing on this shard");
            return None;
        };
        if let Some(reverse) = track.reverse {
            let key = self.fanout_key(track.meta.id);
            let entry = &mut self.tracks[key];
            entry.reverse = Some(reverse);
            entry.encodings = track.layers.iter().map(|l| l.rid).collect();
        }
        let room = self.rooms.get(&room_id)?;
        let tracks = std::slice::from_ref(&track);
        let has_members = !room.members.is_empty();
        for &participant in &room.members {
            if participant.participant_id() == publisher {
                continue;
            }
            ctx.notify_tracks_published(participant.participant_id(), tracks);
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
        if let Some(room) = self.rooms.get_mut(&room_id)
            && !room.audio_imports.swap_remove(&track_id)
        {
            return;
        }
        if let ImportEffect::Retire { route, .. } = self.imports.unsubscribe(&track_id) {
            self.routes.retire(route, now);
            self.imports.on_retired(&track_id);
        }
    }

    /// Every audio route this shard installed for `room_id`, retired together
    /// when the room no longer has local members.
    fn retire_room_audio_routes(&mut self, room_id: RoomId, now: Instant) {
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        let tracks: Vec<TrackId> = room.audio_imports.iter().copied().collect();
        room.audio_imports.clear();
        for track_id in tracks {
            if let ImportEffect::Retire { route, .. } = self.imports.unsubscribe(&track_id) {
                self.routes.retire(route, now);
                self.imports.on_retired(&track_id);
            }
        }
    }

    /// Install audio routes for tracks already published when this shard's
    /// first member joins the room — publish-then-join is as common as
    /// join-then-publish, and only the latter goes through `publish_track`.
    pub fn install_known_audio_routes(
        &mut self,
        room_id: RoomId,
        tracks: &[Track],
        local: &dyn Fn(&ParticipantId) -> bool,
        now: Instant,
        wall: &WallAnchor,
    ) -> Vec<ShardEvent> {
        tracks
            .iter()
            .filter(|t| t.meta.id.kind() == TrackKind::Audio && !local(&t.meta.origin))
            .filter_map(|t| self.install_audio_route(t.meta.clone(), room_id, now, wall))
            .collect()
    }

    /// Install a destination route for a concrete data stream. Returns the
    /// handle to hand back to the publisher's shard.
    fn install_data_route(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<(RouteId, u16)> {
        let key = DataStreamId::new(publisher, topic.clone());
        if self.data_imports.subscribe(key.clone()) != ImportEffect::Install {
            return None;
        }
        let installed = self.routes.install(
            RouteAction::Data {
                lane: DataLane::Realtime,
                room_id,
                origin: publisher,
                topic: topic.clone(),
            },
            RouteNames {
                room_id: Some(room_id),
                origin: publisher,
                track_id: None,
                topic: Some(topic.clone()),
            },
            wall.ntp(),
            now,
        );
        let installed = match installed {
            Ok(installed) => installed,
            Err(err) => {
                tracing::error!(?err, %topic, "data route install failed");
                self.data_imports.cancel_install(&key);
                return None;
            }
        };
        self.data_imports
            .on_installed(&key, installed.0, installed.1);
        Some(installed)
    }

    /// Remove a local data subscriber without touching the cluster route,
    /// dropping the stream entry if that leaves it referencing nothing.
    fn drop_data_subscriber(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        handle: ParticipantHandle,
    ) {
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        let key = DataStreamId::new(publisher, topic.clone());
        let Some(route) = room.data_streams.get_mut(&key) else {
            return;
        };
        route.local_subscribers.swap_remove(&handle);
        if route.is_unused() {
            room.data_streams.remove(&key);
        }
    }

    fn retire_data_route(&mut self, publisher: ParticipantId, topic: &Topic, now: Instant) {
        let key = DataStreamId::new(publisher, topic.clone());
        if let ImportEffect::Retire { route, .. } = self.data_imports.unsubscribe(&key) {
            self.routes.retire(route, now);
            self.data_imports.on_retired(&key);
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
        let room = self.rooms.get_mut(&room_id)?;
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
        let fanout = room
            .data_streams
            .entry(DataStreamId::new(publisher, topic.clone()))
            .or_insert_with(DataStreamRoute::new);
        for subscriber in wildcard_subscribers {
            fanout.local_subscribers.insert(subscriber);
        }

        let (route, epoch) = self.install_data_route(room_id, publisher, topic, now, wall)?;
        Some(ShardEvent::Relay(Topology::DataTopicSubscribed {
            room_id,
            topic: topic.clone(),
            publisher: Some(publisher),
            route: Some(route),
            epoch,
        }))
    }

    fn install_reliable_route(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<(RouteId, u16)> {
        let key = DataStreamId::new(publisher, topic.clone());
        if self.reliable_imports.subscribe(key.clone()) != ImportEffect::Install {
            return None;
        }
        let installed = self.routes.install(
            RouteAction::Data {
                lane: DataLane::Reliable,
                room_id,
                origin: publisher,
                topic: topic.clone(),
            },
            RouteNames {
                room_id: Some(room_id),
                origin: publisher,
                track_id: None,
                topic: Some(topic.clone()),
            },
            wall.ntp(),
            now,
        );
        let installed = match installed {
            Ok(installed) => installed,
            Err(err) => {
                tracing::error!(?err, %topic, "reliable route install failed");
                self.reliable_imports.cancel_install(&key);
                return None;
            }
        };
        self.reliable_imports
            .on_installed(&key, installed.0, installed.1);
        Some(installed)
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
        let room = self.rooms.get(&room_id)?;
        if !room.reliable.has_local_subscribers(topic) {
            return None;
        }
        let (route, epoch) = self.install_reliable_route(room_id, publisher, topic, now, wall)?;
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.reliable.mark_imported(publisher, topic.clone());
        }
        Some(ShardEvent::Relay(Topology::ReliableTopicSubscribed {
            room_id,
            topic: topic.clone(),
            publisher: Some(publisher),
            route: Some(route),
            epoch,
        }))
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
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return Vec::new();
        };
        match (publisher, remote) {
            (Some(publisher), Some(remote)) => {
                debug_assert_eq!(remote.shard_id, from_shard_id);
                room.reliable.attach_remote(publisher, topic, remote);
                Vec::new()
            }
            (Some(_), None) => {
                debug_assert!(false, "a concrete reliable subscription needs a handle");
                Vec::new()
            }
            (None, _) => room.reliable.published_on(&topic),
        }
    }

    pub fn unregister_remote_reliable_subscriber_shard(
        &mut self,
        room_id: RoomId,
        from_shard_id: ShardId,
        topic: &Topic,
        publisher: Option<ParticipantId>,
    ) {
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        match publisher {
            Some(publisher) => room.reliable.detach_remote(publisher, topic, from_shard_id),
            None => {
                for publisher in room.reliable.published_on(topic) {
                    room.reliable.detach_remote(publisher, topic, from_shard_id);
                }
            }
        }
    }

    fn install_audio_route(
        &mut self,
        meta: TrackMeta,
        room_id: RoomId,
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ShardEvent> {
        if self.imports.subscribe(meta.id) != ImportEffect::Install {
            return None;
        }
        let installed = self.routes.install(
            RouteAction::Audio {
                room_id,
                origin: meta.origin,
                track_id: meta.id,
            },
            RouteNames {
                room_id: Some(room_id),
                origin: meta.origin,
                track_id: Some(meta.id),
                topic: None,
            },
            wall.ntp(),
            now,
        );
        let (route, epoch) = match installed {
            Ok(installed) => installed,
            Err(err) => {
                tracing::error!(?err, track_id = %meta.id, "audio route install failed");
                self.imports.cancel_install(&meta.id);
                return None;
            }
        };
        self.imports.on_installed(&meta.id, route, epoch);
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.audio_imports.insert(meta.id);
        }
        Some(ShardEvent::Relay(Topology::TrackSubscribed {
            track: meta,
            route,
            epoch,
        }))
    }

    pub fn unpublish_tracks(
        &mut self,
        room_id: RoomId,
        track_ids: &[TrackId],
        now: Instant,
        ctx: &mut impl RoutingContext,
    ) {
        if let Some(room) = self.rooms.get_mut(&room_id) {
            for &track_id in track_ids {
                room.audio_selector.remove_track((track_id, None));
            }
        }
        for &track_id in track_ids {
            self.retire_audio_route(room_id, track_id, now);
            self.release_fanout_if_idle(&track_id);
        }
        let Some(room) = self.rooms.get(&room_id) else {
            tracing::debug!(%room_id, "unpublish_tracks: room missing on this shard");
            return;
        };
        for &participant in &room.members {
            ctx.notify_tracks_unpublished(participant.participant_id(), track_ids);
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
        let Some(route) = self.tracks.get_mut(fanout) else {
            return;
        };
        let track_id = route.track_id;

        // Hand the packet to the cache and read it back rather than cloning it
        // in: the cache stores every packet anyway, so a clone here is a second
        // copy of the same bytes — and an `RtpPacket` clone heap-allocates,
        // because str0m's `ExtensionValues` carries a type-keyed map.
        let (rid, seq) = (pkt.ext_vals.rid, pkt.seq_no);
        let too_old = route.cache.push(pkt);
        let Some(pkt) = too_old
            .as_ref()
            .or_else(|| route.cache.encoding(rid).and_then(|c| c.get(seq)))
        else {
            debug_assert!(false, "a stored packet must be readable back");
            return;
        };

        for &subscriber in &route.subscribers {
            ctx.forward_video_rtp(subscriber, track_id, pkt, Some(&route.cache));
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
    pub fn route_audio(&mut self, mut ev: AudioRtpEvent, ctx: &mut impl RoutingContext) {
        tracing::trace!(
            target: crate::log::TARGET_AUDIO,
            room_id = %ev.room_id,
            origin = %ev.origin,
            stream_id = %ev.stream_id.0,
            seq_no = %ev.pkt.seq_no,
            "audio packet entered shard audio fanout"
        );

        // Split the borrow: the room owns the selector and members, while the
        // per-stream sender handles live in `tracks`.
        let Self {
            rooms,
            tracks,
            track_keys,
            ..
        } = self;
        let Some(room) = rooms.get_mut(&ev.room_id) else {
            tracing::warn!(target: crate::log::TARGET_AUDIO, room_id = %ev.room_id, "audio packet dropped: room missing");
            return;
        };

        if ctx.is_local(&ev.origin)
            && let Some(track) = track_keys
                .get(&ev.stream_id.0)
                .and_then(|k| tracks.get_mut(*k))
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

        let Some(slot_idx) = room.audio_selector.filter(ev.stream_id, &mut ev.pkt) else {
            return;
        };
        for &participant in &room.members {
            if participant.participant_id() == ev.origin {
                continue;
            }
            ctx.forward_audio_rtp(participant, slot_idx, &ev.pkt);
        }
    }

    #[inline]
    pub fn route_data(
        &mut self,
        room_id: RoomId,
        origin: ParticipantId,
        topic: &Topic,
        pkt: &[u8],
        ctx: &mut impl RoutingContext,
    ) {
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        let stream_id = DataStreamId::new(origin, topic.clone());
        let Some(route) = room.data_streams.get_mut(&stream_id) else {
            return;
        };
        // `published` marks the shard that hosts the publisher, and only that
        // shard sets it. A destination reaches here too — with a route it
        // installed for the stream and a publisher that is not its own — so the
        // flag tracks locality rather than being universally true.
        debug_assert_eq!(
            route.published,
            ctx.is_local(&origin),
            "the published flag must mean 'this shard hosts the publisher'"
        );
        for &subscriber in &route.local_subscribers {
            ctx.forward_sctp(subscriber, origin, topic, pkt);
        }

        if ctx.is_local(&origin) {
            let playout = ctx.wall().ntp();
            for entry in route.remote_subscriber_shards.values_mut() {
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
        now: Instant,
        wall: &WallAnchor,
    ) -> Option<ReverseRoute> {
        let room = self.rooms.get_mut(&room_id)?;
        room.reliable.publish(publisher, topic.clone());
        self.open_topic_reverse_route(room_id, publisher, topic, now, wall)
    }

    pub fn unregister_reliable_data_publisher(
        &mut self,
        room_id: RoomId,
        publisher: ParticipantId,
        topic: &Topic,
        now: Instant,
    ) {
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.reliable.unpublish(publisher, topic);
        }
        self.close_topic_reverse_route(publisher, topic, now);
    }

    pub fn register_reliable_data_subscriber(
        &mut self,
        room_id: RoomId,
        subscriber: ParticipantId,
        topic: Topic,
    ) -> Option<ShardEvent> {
        let handle = self.local_participants.get(&subscriber).copied()?;
        let room = self.rooms.get_mut(&room_id)?;
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
        subscriber: ParticipantId,
        topic: &Topic,
        now: Instant,
    ) -> bool {
        let Some(handle) = self.local_participants.get(&subscriber).copied() else {
            return false;
        };
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return false;
        };
        let was_last = room.reliable.unsubscribe_local(handle, topic);
        if !was_last {
            return false;
        }
        // Nothing left to deliver to on this topic, so every destination route
        // the shard installed for it retires.
        let imported = room.reliable.imported_on(topic);
        for publisher in imported {
            if let Some(room) = self.rooms.get_mut(&room_id) {
                room.reliable.clear_imported(publisher, topic);
            }
            let key = DataStreamId::new(publisher, topic.clone());
            if let ImportEffect::Retire { route, .. } = self.reliable_imports.unsubscribe(&key) {
                self.routes.retire(route, now);
                self.reliable_imports.on_retired(&key);
            }
        }
        true
    }

    pub fn route_reliable_data(
        &mut self,
        room_id: RoomId,
        origin: ParticipantId,
        topic: &Topic,
        frame: &[u8],
        ctx: &mut impl RoutingContext,
    ) {
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        let local_origin = ctx.is_local(&origin);
        if local_origin {
            let playout = ctx.wall().ntp();
            if let Some(remotes) = room.reliable.remote_routes_mut(origin, topic) {
                let frames: Vec<(ShardId, MediaEnvelope)> = remotes
                    .map(|remote| (remote.shard_id, remote.next_envelope(playout)))
                    .collect();
                for (shard_id, env) in frames {
                    ctx.send_media(shard_id, env, MediaPayload::Data(frame.to_vec()));
                }
            }
        }
        room.reliable.route(origin, topic, frame, local_origin, ctx);
    }

    pub fn route_reliable_control(
        &self,
        publisher: ParticipantId,
        topic: &Topic,
        bytes: &[u8],
        ctx: &mut impl RoutingContext,
    ) {
        if ctx.is_local(&publisher) {
            ctx.deliver_reliable_control(publisher, topic, bytes);
        } else if let Some(shard_id) = self.remote_shard_for(&publisher) {
            let key = DataStreamId::new(publisher, topic.clone());
            let Some(target) = self.topic_reverse_targets.get(&key) else {
                // The handle arrives with the publisher announcement, so a
                // subscription cannot predate it.
                debug_assert!(false, "no reverse route for a remote reliable publisher");
                return;
            };
            ctx.send_frame(
                shard_id,
                ShardFrame::Reverse {
                    env: RouteEnvelope::new(*target),
                    body: Reverse::DataAck(bytes.to_vec()),
                },
            );
        }
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
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    #![allow(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::float_cmp
    )]
    use super::*;
    use slotmap::SlotMap;
    use std::cell::RefCell;
    use std::collections::HashSet as StdHashSet;

    use crate::entity::ExternalRoomId;
    use crate::shard::participants::LocalParticipantKey;

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
        forwarded_video: RefCell<Vec<ParticipantId>>,
        forwarded_audio: RefCell<Vec<(ParticipantId, AudioSelectorSlotId)>>,
        forwarded_sctp: RefCell<Vec<ParticipantId>>,
        published: RefCell<Vec<ParticipantId>>,
        unpublished: RefCell<Vec<ParticipantId>>,
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
            _subscriber: ParticipantHandle,
            _track_id: TrackId,
            _states: &crate::track::TrackStates,
        ) {
        }

        fn forward_video_rtp(
            &mut self,
            subscriber: ParticipantHandle,
            _track_id: TrackId,
            _pkt: &RtpPacket,
            _cache: Option<&TrackStreamCache>,
        ) {
            self.forwarded_video
                .borrow_mut()
                .push(subscriber.participant_id());
        }
        fn forward_audio_rtp(
            &mut self,
            subscriber: ParticipantHandle,
            slot_idx: AudioSelectorSlotId,
            _pkt: &RtpPacket,
        ) {
            self.forwarded_audio
                .borrow_mut()
                .push((subscriber.participant_id(), slot_idx));
        }
        fn forward_sctp(
            &mut self,
            subscriber: ParticipantHandle,
            _origin: ParticipantId,
            _topic: &Topic,
            _pkt: &[u8],
        ) {
            self.forwarded_sctp
                .borrow_mut()
                .push(subscriber.participant_id());
        }
        fn notify_tracks_published(&mut self, participant_id: ParticipantId, _tracks: &[Track]) {
            self.published.borrow_mut().push(participant_id);
        }
        fn notify_tracks_unpublished(
            &mut self,
            participant_id: ParticipantId,
            _track_ids: &[TrackId],
        ) {
            self.unpublished.borrow_mut().push(participant_id);
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
            subscriber: ParticipantHandle,
            _origin: ParticipantId,
            _topic: &Topic,
            _frame: &[u8],
        ) {
            self.forwarded_sctp
                .borrow_mut()
                .push(subscriber.participant_id());
        }

        fn deliver_reliable_control(
            &mut self,
            _publisher: ParticipantId,
            _topic: &Topic,
            _bytes: &[u8],
        ) {
        }
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
        &table.tracks[table
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

    fn add_local_subscriber(table: &mut ShardRoutingTable, participant_id: ParticipantId) {
        let mut slots = SlotMap::<LocalParticipantKey, ()>::with_key();
        let key = slots.insert(());
        let handle = ParticipantHandle::new(key, participant_id, 1);
        table.local_participants.insert(participant_id, handle);
    }

    fn replace_local_subscriber(
        table: &mut ShardRoutingTable,
        participant_id: ParticipantId,
    ) -> ParticipantHandle {
        let mut slots = SlotMap::<LocalParticipantKey, ()>::with_key();
        let key = slots.insert(());
        let handle = ParticipantHandle::new(key, participant_id, 2);
        table.local_participants.insert(participant_id, handle);
        handle
    }

    // -- the bug this refactor exists to prevent recurring ------------------

    #[test]
    fn duplicate_register_remote_participant_does_not_leak_refcount() {
        let mut table = ShardRoutingTable::new();
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
            !table.rooms.contains_key(&room),
            "room must be fully cleaned up after one register + one unregister"
        );
    }

    #[test]
    fn moving_remote_participant_releases_the_old_shard() {
        let mut table = ShardRoutingTable::new();
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let participant = pid();
        let room = room_id("r2");
        let old_shard = ShardId::new(1);
        let new_shard = ShardId::new(2);

        table.register_remote_participant(participant, room, old_shard, &mut rng);
        table.register_remote_participant(participant, room, new_shard, &mut rng);

        assert!(!table.rooms[&room].remote_shards.contains(&old_shard));
        assert!(table.rooms[&room].remote_shards.contains(&new_shard));
    }

    // -- topology ------------------------------------------------------------

    #[test]
    fn first_subscriber_notifies_publisher_shard() {
        let mut table = ShardRoutingTable::new();
        let track = TrackMeta {
            shard_id: ShardId::new(1),
            id: pid().derive_track_id(TrackKind::Video, "v"),
            origin: pid(),
        };

        let first = pid();
        let second = pid();
        add_local_subscriber(&mut table, first);
        add_local_subscriber(&mut table, second);

        let ev = table.register_subscriber(first, track.clone(), now(), &wall());
        assert!(
            matches!(ev, Some(ShardEvent::Relay(Topology::TrackSubscribed { track: t, .. })) if t == track),
            "the first subscriber installs a route and hands over the handle"
        );

        let ev2 = table.register_subscriber(second, track, now(), &wall());
        assert!(ev2.is_none(), "second subscriber must not re-notify");
    }

    #[test]
    fn replacement_subscriber_evicts_stale_route_without_duplicate_notification() {
        let mut table = ShardRoutingTable::new();
        let subscriber = pid();
        let track = TrackMeta {
            shard_id: ShardId::new(1),
            id: pid().derive_track_id(TrackKind::Video, "v"),
            origin: pid(),
        };
        add_local_subscriber(&mut table, subscriber);
        assert!(
            table
                .register_subscriber(subscriber, track.clone(), now(), &wall())
                .is_some()
        );

        let replacement = replace_local_subscriber(&mut table, subscriber);
        assert!(
            table
                .register_subscriber(subscriber, track.clone(), now(), &wall())
                .is_none()
        );

        assert_eq!(fanout(&table, &track.id).subscribers, vec![replacement]);
        assert!(
            table
                .unregister_subscriber(subscriber, track, now())
                .is_some()
        );
    }

    // -- fanout ---------------------------------------------------------------

    /// A reliable subscription names only a topic, so it installs nothing until
    /// a publisher on that topic is announced, then retires with the last
    /// local subscriber.
    #[test]
    fn a_reliable_subscription_resolves_on_publisher_announcement() {
        let mut table = ShardRoutingTable::new();
        let mut rng = rand::seeded_rng(13);
        let room = room_id("reliable-room");
        let subscriber = pid();
        let handle = ParticipantHandle::new(
            SlotMap::<LocalParticipantKey, ()>::with_key().insert(()),
            subscriber,
            1,
        );
        table.add_local_member(subscriber, handle, room, &mut rng);

        let topic = Topic::for_test("chat");
        let ev = table.register_reliable_data_subscriber(room, subscriber, topic.clone());
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
        assert_eq!(table.routes.len(), 0);

        let publisher = pid();
        let resolved = table.on_remote_reliable_publisher(room, publisher, &topic, now(), &wall());
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
        assert_eq!(table.routes.len(), 1);

        assert!(table.unregister_reliable_data_subscriber(room, subscriber, &topic, now()));
        assert_eq!(
            table.routes.len(),
            0,
            "the last subscriber leaving retires the imported route"
        );
    }

    /// An explicit `publisher: Some(..)` data subscription knows its stream, so
    /// the destination installs a route immediately and hands back the handle.
    #[test]
    fn an_explicit_data_subscription_installs_a_route() {
        let mut table = ShardRoutingTable::new();
        let mut rng = rand::seeded_rng(11);
        let room = room_id("data-room");
        let subscriber = pid();
        let handle = ParticipantHandle::new(
            SlotMap::<LocalParticipantKey, ()>::with_key().insert(()),
            subscriber,
            1,
        );
        table.add_local_member(subscriber, handle, room, &mut rng);

        let publisher = pid();
        let topic = Topic::for_test("chat");
        let ev = table.register_data_subscriber(
            room,
            subscriber,
            topic.clone(),
            Some(publisher),
            now(),
            &wall(),
        );
        assert!(
            matches!(
                ev,
                Some(ShardEvent::Relay(Topology::DataTopicSubscribed {
                    route: Some(_),
                    ..
                }))
            ),
            "the first subscriber installs a route and hands over the handle"
        );
        assert_eq!(table.routes.len(), 1);

        let second = pid();
        let h2 = ParticipantHandle::new(
            SlotMap::<LocalParticipantKey, ()>::with_key().insert(()),
            second,
            1,
        );
        table.add_local_member(second, h2, room, &mut rng);
        assert!(
            table
                .register_data_subscriber(room, second, topic, Some(publisher), now(), &wall())
                .is_none(),
            "local churn must not touch the cluster route"
        );
        assert_eq!(table.routes.len(), 1);
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
        let mut table = ShardRoutingTable::new();
        table.routes = crate::route::RouteTable::with_max_slots(0);
        let mut rng = rand::seeded_rng(23);
        let room = room_id("data-room");
        let subscriber = pid();
        let handle = ParticipantHandle::new(
            SlotMap::<LocalParticipantKey, ()>::with_key().insert(()),
            subscriber,
            1,
        );
        table.add_local_member(subscriber, handle, room, &mut rng);

        let publisher = pid();
        let topic = Topic::for_test("chat");
        assert!(
            table
                .register_data_subscriber(
                    room,
                    subscriber,
                    topic.clone(),
                    Some(publisher),
                    now(),
                    &wall()
                )
                .is_none(),
            "no route, so nothing to announce"
        );
        assert_eq!(table.routes.len(), 0);

        table.routes = crate::route::RouteTable::new();
        let second = pid();
        let h2 = ParticipantHandle::new(
            SlotMap::<LocalParticipantKey, ()>::with_key().insert(()),
            second,
            1,
        );
        table.add_local_member(second, h2, room, &mut rng);
        assert!(
            matches!(
                table.register_data_subscriber(
                    room,
                    second,
                    topic,
                    Some(publisher),
                    now(),
                    &wall()
                ),
                Some(ShardEvent::Relay(Topology::DataTopicSubscribed {
                    route: Some(_),
                    ..
                }))
            ),
            "the next subscriber retries the install rather than joining a pending one"
        );
        assert_eq!(table.routes.len(), 1);
    }

    /// A destination's measurements arrive whole, by message, and replace what
    /// was there. There is no handle to go stale: a republished track under the
    /// same `TrackId` simply gets the next snapshot, and until it does the
    /// fanout has none rather than the previous incarnation's.
    #[test]
    fn a_stats_snapshot_replaces_what_the_fanout_held() {
        use crate::rtp::monitor::StreamStats;

        let mut table = ShardRoutingTable::new();
        let mut rng = rand::seeded_rng(77);
        let room = room_id("republish");
        let publisher = pid();
        let track = TrackMeta {
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(TrackKind::Video, "v"),
            origin: publisher,
        };
        let subscriber = pid();
        let handle = ParticipantHandle::new(
            SlotMap::<LocalParticipantKey, ()>::with_key().insert(()),
            subscriber,
            1,
        );
        table.add_local_member(subscriber, handle, room, &mut rng);
        table.register_subscriber(subscriber, track.clone(), now(), &wall());

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
        let mut table = ShardRoutingTable::new();
        let mut rng = rand::seeded_rng(53);
        let room = room_id("fanout-release");
        let publisher = pid();
        let track = TrackMeta {
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(TrackKind::Video, "v"),
            origin: publisher,
        };
        let subscriber = pid();
        let handle = ParticipantHandle::new(
            SlotMap::<LocalParticipantKey, ()>::with_key().insert(()),
            subscriber,
            1,
        );
        table.add_local_member(subscriber, handle, room, &mut rng);

        table.register_subscriber(subscriber, track.clone(), now(), &wall());
        assert_eq!(table.tracks.len(), 1, "subscribing creates the fanout");

        table.unregister_subscriber(subscriber, track.clone(), now());
        assert_eq!(
            table.tracks.len(),
            0,
            "losing the last consumer must release the fanout and its packet rings"
        );
        assert!(
            table.fanout_of(&track.id).is_none(),
            "the name index must be released with it"
        );
    }

    /// A reliable topic gets a reverse route on the same terms as a track: one
    /// per published stream, resolving to the publisher and topic so an ack
    /// names neither.
    #[test]
    fn a_reliable_topic_gets_one_reverse_route() {
        let mut table = ShardRoutingTable::new();
        let mut rng = rand::seeded_rng(91);
        let room = room_id("reliable-reverse");
        let publisher = pid();
        let topic = Topic::for_test("chat");
        let handle = ParticipantHandle::new(
            SlotMap::<LocalParticipantKey, ()>::with_key().insert(()),
            publisher,
            1,
        );
        table.add_local_member(publisher, handle, room, &mut rng);

        let target = table
            .register_reliable_data_publisher(room, publisher, topic.clone(), now(), &wall())
            .expect("publishing a reliable topic opens its reverse route");
        assert_eq!(table.routes.len(), 1);
        assert!(
            matches!(
                table.resolve_reverse(target.route, target.epoch),
                Some((origin, ReverseTarget::Topic { topic: t, .. }))
                    if origin == publisher && *t == topic
            ),
            "an ack resolves to its publisher and topic through the route alone"
        );

        table.unregister_reliable_data_publisher(room, publisher, &topic, now());
        assert_eq!(
            table.routes.len(),
            0,
            "unpublishing must free the reverse route"
        );
    }

    /// The reverse direction must cost one route per track, not one per
    /// (track x subscribing shard). Route ids are 32 bits and the forward
    /// direction already pays per destination; letting feedback do the same
    /// would make it the largest consumer in the table for no benefit, since
    /// it is latest-wins and keeps no per-link state.
    #[test]
    fn feedback_costs_one_route_per_track_regardless_of_subscribers() {
        let mut table = ShardRoutingTable::new();
        let publisher = pid();
        let track = TrackMeta {
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(TrackKind::Video, "v"),
            origin: publisher,
        };

        let target = table
            .open_track_reverse_route(&video_track_with(&track), now(), &wall())
            .expect("publishing opens the reverse route");
        assert_eq!(table.routes.len(), 1);

        // Every subscribing shard addresses the same id.
        for shard in 1..8u8 {
            let _ = shard;
            assert!(
                matches!(
                    table.resolve_reverse(target.route, target.epoch),
                    Some((origin, ReverseTarget::Track { track_id, .. }))
                        if origin == publisher && *track_id == track.id
                ),
                "every subscriber resolves through the one route"
            );
        }
        assert_eq!(
            table.routes.len(),
            1,
            "subscriber count must not grow the reverse table"
        );

        table.close_track_reverse_route(&track.id, now());
        assert_eq!(
            table.routes.len(),
            0,
            "unpublishing must free the reverse route"
        );
    }

    /// A request already in flight when the track was unpublished must not land
    /// on whatever later takes that slot.
    #[test]
    fn feedback_on_a_retired_route_is_dropped() {
        let mut table = ShardRoutingTable::new();
        let publisher = pid();
        let track = TrackMeta {
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(TrackKind::Video, "v"),
            origin: publisher,
        };

        let stale = table
            .open_track_reverse_route(&video_track_with(&track), now(), &wall())
            .unwrap();
        table.close_track_reverse_route(&track.id, now());

        assert!(
            table.resolve_reverse(stale.route, stale.epoch).is_none(),
            "a reverse frame for an unpublished track must not resolve"
        );
    }

    /// Teardown must be idempotent under reordering. An unsubscribe names the
    /// route incarnation it is retiring, so one overtaken by a resubscription
    /// from the same shard is ignored rather than silently stopping the media
    /// the new subscription just asked for.
    #[test]
    fn a_stale_unsubscribe_does_not_tear_down_a_newer_route() {
        let mut table = ShardRoutingTable::new();
        let publisher = pid();
        let track = TrackMeta {
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(TrackKind::Video, "v"),
            origin: publisher,
        };
        let subscriber_shard = ShardId::new(1);

        let stale = RouteId::new(7);
        let fresh = RouteId::new(9);

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
        let mut table = ShardRoutingTable::new();
        let mut rng = rand::seeded_rng(41);
        let room = room_id("data-retire");
        let publisher = pid();
        let topic = Topic::for_test("chat");

        let subscribe = |table: &mut ShardRoutingTable, who: ParticipantId, rng: &mut _| {
            let handle = ParticipantHandle::new(
                SlotMap::<LocalParticipantKey, ()>::with_key().insert(()),
                who,
                1,
            );
            table.add_local_member(who, handle, room, rng);
            table.register_data_subscriber(
                room,
                who,
                topic.clone(),
                Some(publisher),
                now(),
                &wall(),
            )
        };

        let a = pid();
        let b = pid();
        subscribe(&mut table, a, &mut rng);
        subscribe(&mut table, b, &mut rng);
        assert_eq!(table.routes.len(), 1, "one route serves both subscribers");

        table.unregister_data_subscriber(room, a, &topic, Some(publisher), now());
        assert_eq!(
            table.routes.len(),
            1,
            "churn with a subscriber remaining must not touch the cluster route"
        );

        table.unregister_data_subscriber(room, b, &topic, Some(publisher), now());
        assert_eq!(
            table.routes.len(),
            0,
            "the last unsubscribe must retire the route"
        );

        // And the slot must be usable again: resubscribing installs a new one.
        let c = pid();
        subscribe(&mut table, c, &mut rng);
        assert_eq!(
            table.routes.len(),
            1,
            "a later subscription must be able to install a fresh route"
        );
    }

    /// A wildcard subscription cannot name a stream, so it installs nothing
    /// until a publisher is announced — then it resolves to a concrete route.
    #[test]
    fn a_wildcard_data_subscription_resolves_on_publisher_announcement() {
        let mut table = ShardRoutingTable::new();
        let mut rng = rand::seeded_rng(12);
        let room = room_id("data-wildcard");
        let subscriber = pid();
        let handle = ParticipantHandle::new(
            SlotMap::<LocalParticipantKey, ()>::with_key().insert(()),
            subscriber,
            1,
        );
        table.add_local_member(subscriber, handle, room, &mut rng);

        let topic = Topic::for_test("chat");
        let ev =
            table.register_data_subscriber(room, subscriber, topic.clone(), None, now(), &wall());
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
        assert_eq!(table.routes.len(), 0);

        let publisher = pid();
        let resolved = table.on_remote_data_publisher(room, publisher, &topic, now(), &wall());
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
        assert_eq!(table.routes.len(), 1);
    }

    /// Audio gets one route per (stream, destination). Membership in the room
    /// is the subscription, so the destination installs on learning the track
    /// exists and retires when it has nobody left to deliver to.
    #[test]
    fn an_audio_route_is_installed_per_stream_and_retired_with_the_room() {
        let mut table = ShardRoutingTable::new();
        let mut rng = rand::seeded_rng(7);
        let room = room_id("audio-room");
        let local = pid();
        let handle = ParticipantHandle::new(
            SlotMap::<LocalParticipantKey, ()>::with_key().insert(()),
            local,
            1,
        );
        table.add_local_member(local, handle, room, &mut rng);

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
        let ev = table.publish_track(track, room, now(), &wall(), &mut ctx);
        assert!(
            matches!(ev, Some(ShardEvent::Relay(Topology::TrackSubscribed { track: t, .. })) if t == audio),
            "a remote audio publish installs a destination route"
        );
        assert_eq!(table.routes.len(), 1);

        table.remove_local_member(&local, room, std::iter::empty(), now());
        assert_eq!(
            table.routes.len(),
            0,
            "no members left means nothing to deliver to"
        );
    }

    #[test]
    fn a_locally_published_audio_track_installs_no_route() {
        let mut table = ShardRoutingTable::new();
        let mut rng = rand::seeded_rng(7);
        let room = room_id("audio-room-local");
        let origin = pid();
        let handle = ParticipantHandle::new(
            SlotMap::<LocalParticipantKey, ()>::with_key().insert(()),
            origin,
            1,
        );
        table.add_local_member(origin, handle, room, &mut rng);

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
            now(),
            &wall(),
            &mut ctx,
        );
        assert!(ev.is_none(), "a local publisher needs no cluster route");
        assert_eq!(table.routes.len(), 0);
    }

    /// The destination allocates a route, the publisher receives the handle,
    /// and only then does media flow — addressed by route, not by track id.
    #[test]
    fn a_route_is_installed_once_and_retired_with_the_last_subscriber() {
        let mut table = ShardRoutingTable::new();
        let track = TrackMeta {
            shard_id: ShardId::new(1),
            id: pid().derive_track_id(TrackKind::Video, "v"),
            origin: pid(),
        };
        let (first, second) = (pid(), pid());
        add_local_subscriber(&mut table, first);
        add_local_subscriber(&mut table, second);

        let Some(ShardEvent::Relay(Topology::TrackSubscribed { route, epoch, .. })) =
            table.register_subscriber(first, track.clone(), now(), &wall())
        else {
            panic!("the first subscriber must install a route");
        };
        assert_eq!(table.routes.len(), 1);

        assert!(
            table
                .register_subscriber(second, track.clone(), now(), &wall())
                .is_none(),
            "local churn must not touch the cluster route"
        );
        assert_eq!(table.routes.len(), 1, "exactly one route installation");

        assert!(
            table
                .unregister_subscriber(first, track.clone(), now())
                .is_none()
        );
        assert_eq!(table.routes.len(), 1, "still one consumer left");

        assert!(
            table.unregister_subscriber(second, track, now()).is_some(),
            "the last subscriber leaving tells the publisher to stop"
        );
        assert_eq!(table.routes.len(), 0, "the route is retired");

        // A frame still in flight for the retired incarnation must not land.
        let env = MediaEnvelope {
            epoch,
            route,
            link_seq: 0,
            playout_ntp32: 0,
        };
        assert!(table.routes.resolve(&env).is_err());
    }

    #[test]
    fn route_video_forwards_to_subscribers_and_remote_shards() {
        let mut table = ShardRoutingTable::new();
        let track_id = pid().derive_track_id(TrackKind::Video, "v");
        let subscriber = pid();
        add_local_subscriber(&mut table, subscriber);

        table.register_subscriber(
            subscriber,
            TrackMeta {
                shard_id: ShardId::new(0),
                id: track_id,
                origin: pid(),
            },
            now(),
            &wall(),
        );
        // Stand in for a destination shard that installed a route and had its
        // handle acknowledged back to this publisher.
        table.register_remote_subscriber_shard(
            RemoteRoute::new(ShardId::new(3), RouteId::new(0), 0),
            TrackMeta {
                shard_id: ShardId::new(0),
                id: track_id,
                origin: pid(),
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

        assert_eq!(ctx.forwarded_video.borrow().as_slice(), &[subscriber]);
        assert_eq!(ctx.sent.borrow().len(), 1);
    }
}
