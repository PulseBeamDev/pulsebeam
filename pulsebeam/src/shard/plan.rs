//! Everything a frame touches on the forwarding path, and nothing else.
//!
//! [`DataPlane`] is read and mutated by the five dispatch functions in
//! `router.rs`. Names never appear here except as values already resolved by
//! the control plane — see `shard/control.rs`.

use ahash::HashMap;

use slotmap::SlotMap;

use crate::audio_selector::TopNAudioSelector;
use crate::entity::TrackId;
use crate::id::ShardId;
use super::keyset::KeySet;
use crate::route::{RemoteRoute, RouteHandle, RouteRuntime};
use crate::rtp::cache::TrackStreamCache;
use crate::track::Topic;

use super::control::DataStreamId;
use super::participants::ParticipantKey;
use super::reliable::ReliableRoutes;
use super::router::{
    DataStreamKey, FastIndexSet, LocalTrackKey, ReliableStreamKey, RoomKey, fast_set,
};

pub(crate) struct AllPublisherSubscriptions {
    /// Per topic, wildcard local subscribers. `ParticipantKey` is already a
    /// dense slotmap key — same trade `RoomFanout::members` makes.
    pub local_by_topic: HashMap<Topic, KeySet<ParticipantKey>>,
    /// Wildcard remote subscribers per topic. A node's shard count is small
    /// and bounded, so linear-scanning it beats hashing `ShardId`, the same
    /// trade-off `RoomFanout::remote_shards` makes.
    pub remote_by_topic: HashMap<Topic, Vec<ShardId>>,
}

impl AllPublisherSubscriptions {
    pub fn new() -> Self {
        Self {
            local_by_topic: HashMap::default(),
            remote_by_topic: HashMap::default(),
        }
    }
}

/// A destination's acknowledged handle plus how many local subscriptions
/// (explicit and wildcard) reference it.
pub(crate) struct RemoteDataSubscriber {
    pub remote: RemoteRoute,
    pub refs: usize,
}

pub(crate) struct DataStreamRoute {
    /// The stream this fanout serves. Carried for filtered iteration (by
    /// topic, by publisher) and for logs — never hashed to find this object,
    /// the same rule `TrackRoute::track_id` follows.
    pub id: DataStreamId,
    pub published: bool,
    /// `ParticipantKey` is already a dense slotmap key, so dedup is a linear
    /// scan (`VecSet`) rather than a hash index — the same trade `RoomFanout::members` makes.
    pub local_subscribers: KeySet<ParticipantKey>,
    /// This shard's route lifecycle for the stream — see
    /// [`TrackRoute::import`].
    pub import: crate::route::ImportSlot,
    /// Remote destination shards for this stream. A shard's fan-out is
    /// bounded by node size, not room size, so a linear scan beats a hash
    /// lookup here — the same trade-off `RoomFanout::remote_shards` makes.
    pub remote_subscriber_shards: Vec<RemoteDataSubscriber>,
}

impl DataStreamRoute {
    pub fn new(id: DataStreamId) -> Self {
        Self {
            id,
            published: false,
            local_subscribers: KeySet::with_capacity(256),
            import: crate::route::ImportSlot::default(),
            remote_subscriber_shards: Vec::new(),
        }
    }

    /// Nothing left to deliver to. Decides whether the *route* should be
    /// retired — separate from [`Self::is_unused`], which decides whether the
    /// entry itself can go, because the route has to be released before the
    /// entry that records it.
    pub fn has_no_consumers(&self) -> bool {
        !self.published
            && self.local_subscribers.is_empty()
            && self.remote_subscriber_shards.is_empty()
    }

    pub fn is_unused(&self) -> bool {
        // An entry still carrying a route lifecycle is not unused: the import
        // state lives here now, and dropping the entry would strand a route
        // that still has to be released.
        self.has_no_consumers() && self.import.state().is_none()
    }

    pub fn attach_remote_subscriber_shard(&mut self, remote: RemoteRoute) {
        match self
            .remote_subscriber_shards
            .iter_mut()
            .find(|entry| entry.remote.shard_id == remote.shard_id)
        {
            Some(existing) => {
                existing.refs = existing.refs.saturating_add(1);
                debug_assert!(existing.refs <= 2);
                // A reinstall at the destination supersedes the old incarnation.
                if existing.remote.route != remote.route || existing.remote.epoch != remote.epoch {
                    existing.remote = remote;
                }
            }
            None => {
                self.remote_subscriber_shards
                    .push(RemoteDataSubscriber { remote, refs: 1 });
            }
        }
    }

    pub fn detach_remote_subscriber_shard(&mut self, shard_id: ShardId) {
        let Some(pos) = self
            .remote_subscriber_shards
            .iter()
            .position(|entry| entry.remote.shard_id == shard_id)
        else {
            debug_assert!(false, "detaching an unknown remote subscriber shard");
            return;
        };
        let Some(entry) = self.remote_subscriber_shards.get_mut(pos) else {
            debug_assert!(false, "position() returned an index outside the vec");
            return;
        };
        debug_assert!(entry.refs > 0, "refcount underflow would leak this route");
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs == 0 {
            self.remote_subscriber_shards.swap_remove(pos);
        }
    }
}

/// Where to send keyframe requests for a track published on another shard, and
/// the encoding order needed to name one of its layers.
pub(crate) struct TrackReverseTarget {
    pub route: RouteHandle,
    pub encodings: Vec<Option<str0m::media::Rid>>,
}

pub(crate) struct TrackRoute {
    /// The track this fanout serves. Carried for the downstream slot match and
    /// for logs — never hashed to find this object, which is the whole point of
    /// addressing it by key.
    pub track_id: TrackId,
    /// The publisher, carried for the same reason `track_id` is: a resolved
    /// `RouteAction::Audio` or `RouteAction::Reverse` reads it off here
    /// instead of carrying it inline.
    pub origin: crate::entity::ParticipantId,
    pub subscribers: KeySet<ParticipantKey>,
    /// This shard's route lifecycle for the track, when it imports it from
    /// another shard. Lives here rather than in a table keyed by `TrackId`,
    /// because this is the object the state is about.
    pub import: crate::route::ImportSlot,
    /// Measurement handles for the publisher's encodings. Reaches this shard
    /// along the media path — from the local publisher, or from the publisher's
    /// shard on subscribe — never through the controller.
    pub layer_states: crate::track::TrackStates,
    /// Acknowledged sender handles, one per destination shard. A destination
    /// only appears here once it has installed its route, so the presence of a
    /// handle is what permits media to flow.
    pub remote_routes: Vec<RemoteRoute>,
    /// Encodings in declared order, set when this shard opens the track's
    /// reverse path. A reverse frame names one by index instead of carrying a
    /// rid, so resolving it needs the same order both ends used.
    pub encodings: Vec<Option<str0m::media::Rid>>,
    /// Whole packets for replay to a new subscriber — a 512-slot ring per
    /// encoding, hundreds of kilobytes once populated. Dropped as soon as
    /// nothing consumes it, independent of whether the track stays published,
    /// so a departed audience does not keep paying for it.
    pub cache: Option<TrackStreamCache>,
    /// Whether this shard currently hosts the track's publisher. Distinct
    /// from having subscribers: a published track with none right now must
    /// still not be collected, or a late subscriber finds nothing here.
    pub published: bool,
    /// This shard's own reverse route for the track, opened alongside
    /// `published` and retired with it.
    pub reverse_route: Option<RouteHandle>,
    /// Where *this* shard sends its own reverse traffic when it is a
    /// destination for a track published elsewhere — learned from the
    /// publish announcement or the late-join replay, whichever comes first.
    pub reverse_target: Option<TrackReverseTarget>,
}

impl TrackRoute {
    #[cfg(test)]
    pub fn state_for(
        &self,
        rid: Option<str0m::media::Rid>,
    ) -> Option<&crate::rtp::monitor::StreamStats> {
        self.layer_states
            .iter()
            .find(|(r, _)| *r == rid)
            .map(|(_, s)| s)
    }

    pub fn new(track_id: TrackId, origin: crate::entity::ParticipantId) -> Self {
        Self {
            track_id,
            origin,
            subscribers: KeySet::with_capacity(256),
            import: crate::route::ImportSlot::default(),
            layer_states: Vec::new(),
            remote_routes: Vec::new(),
            encodings: Vec::new(),
            cache: None,
            published: false,
            reverse_route: None,
            reverse_target: None,
        }
    }

    /// Nothing left that needs this entry: not published here, nobody local
    /// subscribes, no destination shard has a route, and no other shard is
    /// known to be waiting on a reverse target from it.
    pub fn is_unused(&self) -> bool {
        !self.published
            && self.subscribers.is_empty()
            && self.remote_routes.is_empty()
            && self.reverse_target.is_none()
            && self.import.state().is_none()
    }
}

pub(crate) struct RoomFanout {
    /// `ParticipantKey` is already a dense slotmap key, so membership indexes
    /// `VecSet`-deduped `Vec` rather than a hash index.
    pub members: KeySet<ParticipantKey>,
    /// Audio tracks this shard has installed a destination route for, so they
    /// can be retired when the room goes away.
    pub audio_imports: FastIndexSet<TrackId>,
    pub audio_selector: TopNAudioSelector,
    /// Realtime data streams that belong to this room, so a room-wide
    /// operation (release on empty, filter by topic) does not have to scan
    /// every stream on the shard. The arena entries themselves live in
    /// `DataPlane::data_streams`; this is bookkeeping, not the fanout.
    pub data_stream_keys: FastIndexSet<DataStreamKey>,
    /// Same bookkeeping, for the reliable lane's arena entries.
    pub reliable_stream_keys: FastIndexSet<ReliableStreamKey>,
    pub all_publisher_subscriptions: AllPublisherSubscriptions,
    pub(super) reliable: ReliableRoutes,
}

impl RoomFanout {
    pub fn new(rng: &mut impl pulsebeam_runtime::rand::RngCore) -> Self {
        Self {
            members: KeySet::new(),
            audio_imports: fast_set(),
            audio_selector: TopNAudioSelector::new(rng),
            data_stream_keys: fast_set(),
            reliable_stream_keys: fast_set(),
            all_publisher_subscriptions: AllPublisherSubscriptions::new(),
            reliable: ReliableRoutes::new(),
        }
    }




}

/// The descriptor for one reliable stream on this shard, in either role —
/// publisher (reverse route target, `published`) or destination (forward
/// route target, `imported`). Resolves an arriving frame or ack by key
/// instead of carrying `room_id`/`origin`/`topic` inline in
/// [`crate::route::RouteAction`] and [`crate::route::ReverseTarget`].
///
/// Local topic subscriptions stay in `ReliableRoutes` (inside `RoomFanout`):
/// a subscription names a topic, not a stream, so it has no natural
/// `ReliableStreamKey` to live under here.
pub(crate) struct ReliableStream {
    pub id: DataStreamId,
    /// The room's own dense key, resolved once at creation so the hot
    /// dispatch path (`route_reliable_data`) reaches `DataPlane::rooms` by
    /// index instead of hashing a `RoomId` back through the control plane on
    /// every frame. The room's name travels separately, in `RouteNames`, for
    /// whatever needs it for logs.
    pub room: RoomKey,
    pub published: bool,
    pub imported: bool,
    /// This shard's route lifecycle for the topic — see
    /// [`TrackRoute::import`].
    pub import: crate::route::ImportSlot,
    /// Acknowledged destination handles, one per subscribing shard. Small and
    /// scanned linearly rather than indexed by `ShardId` — same trade `TrackRoute::remote_routes` already makes.
    pub remote_routes: Vec<RemoteRoute>,
    /// This shard's own reverse route for the topic, opened alongside
    /// `published` and retired with it.
    pub reverse_route: Option<RouteHandle>,
    /// Where this shard sends acks when it is a destination for a topic
    /// published elsewhere — learned from the publish announcement.
    pub reverse_target: Option<RouteHandle>,
}

impl ReliableStream {
    pub fn new(id: DataStreamId, room: RoomKey) -> Self {
        Self {
            id,
            room,
            published: false,
            imported: false,
            import: crate::route::ImportSlot::default(),
            remote_routes: Vec::new(),
            reverse_route: None,
            reverse_target: None,
        }
    }

    pub fn is_unused(&self) -> bool {
        !self.published
            && !self.imported
            && self.remote_routes.is_empty()
            && self.reverse_target.is_none()
            && self.import.state().is_none()
    }
}

pub(crate) struct DataPlane {
    /// Fanout objects, addressed densely. Arrivals resolve to a key, never a
    /// name: a `RoomId` is a 16-byte value to hash, a key is an index.
    pub rooms: SlotMap<RoomKey, RoomFanout>,
    /// Fanout objects, addressed densely. Arrivals resolve to a key, never a
    /// name: a `TrackId` is a 17-byte value to hash, a key is an index.
    pub tracks: SlotMap<LocalTrackKey, TrackRoute>,
    /// Realtime data stream fanout objects, addressed densely for the same
    /// reason. Shard-global rather than nested per room — `DataStreamId`
    /// (publisher, topic) is already unique across rooms, since a publisher
    /// belongs to exactly one room.
    pub data_streams: SlotMap<DataStreamKey, DataStreamRoute>,
    pub reliable_streams: SlotMap<ReliableStreamKey, ReliableStream>,
    /// Per-route accounting for the routes this shard is a destination for.
    /// Resolution itself happens in the published view; this is only the
    /// mutable processing state behind it.
    pub routes: RouteRuntime,
}

impl DataPlane {
    pub fn new(shard_id: ShardId) -> Self {
        Self {
            rooms: SlotMap::with_key(),
            tracks: SlotMap::with_key(),
            data_streams: SlotMap::with_key(),
            reliable_streams: SlotMap::with_key(),
            routes: RouteRuntime::new(shard_id),
        }
    }
}
