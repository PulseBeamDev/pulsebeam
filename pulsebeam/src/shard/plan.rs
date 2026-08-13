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
use crate::route::{RemoteRoute, RouteTable};
use crate::rtp::cache::TrackStreamCache;
use crate::track::Topic;

use super::control::DataStreamId;
use super::participants::ParticipantHandle;
use super::reliable::ReliableRoutes;
use super::router::{
    DataStreamKey, FastIndexSet, LocalTrackKey, ReliableStreamKey, RoomKey, fast_set,
    fast_set_with_capacity,
};

pub(crate) struct AllPublisherSubscriptions {
    pub local_by_topic: HashMap<Topic, FastIndexSet<ParticipantHandle>>,
    pub remote_by_topic: HashMap<Topic, FastIndexSet<ShardId>>,
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
    pub local_subscribers: FastIndexSet<ParticipantHandle>,
    pub remote_subscriber_shards: HashMap<ShardId, RemoteDataSubscriber>,
}

impl DataStreamRoute {
    pub fn new(id: DataStreamId) -> Self {
        Self {
            id,
            published: false,
            local_subscribers: fast_set_with_capacity(256),
            remote_subscriber_shards: HashMap::default(),
        }
    }

    pub fn is_unused(&self) -> bool {
        !self.published
            && self.local_subscribers.is_empty()
            && self.remote_subscriber_shards.is_empty()
    }

    pub fn attach_remote_subscriber_shard(&mut self, remote: RemoteRoute) {
        match self.remote_subscriber_shards.get_mut(&remote.shard_id) {
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
                    .insert(remote.shard_id, RemoteDataSubscriber { remote, refs: 1 });
            }
        }
    }

    pub fn detach_remote_subscriber_shard(&mut self, shard_id: ShardId) {
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
    pub layer_states: crate::track::TrackStates,
    /// Acknowledged sender handles, one per destination shard. A destination
    /// only appears here once it has installed its route, so the presence of a
    /// handle is what permits media to flow.
    pub remote_routes: Vec<RemoteRoute>,
    pub cache: TrackStreamCache,
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

    pub fn new(track_id: TrackId) -> Self {
        Self {
            track_id,
            subscribers: Vec::with_capacity(256),
            layer_states: Vec::new(),
            remote_routes: Vec::new(),
            cache: TrackStreamCache::new(),
        }
    }
}

pub(crate) struct RoomFanout {
    pub members: FastIndexSet<ParticipantHandle>,
    pub remote_shards: FastIndexSet<ShardId>,
    /// Audio tracks this shard has installed a destination route for, so they
    /// can be retired when the room goes away.
    pub audio_imports: FastIndexSet<TrackId>,
    pub audio_selector: TopNAudioSelector,
    /// Realtime data streams that belong to this room, so a room-wide
    /// operation (release on empty, filter by topic) does not have to scan
    /// every stream on the shard. The arena entries themselves live in
    /// `DataPlane::data_streams`; this is bookkeeping, not the fanout.
    pub data_stream_keys: FastIndexSet<DataStreamKey>,
    pub all_publisher_subscriptions: AllPublisherSubscriptions,
    pub(super) reliable: ReliableRoutes,
}

impl RoomFanout {
    pub fn new(rng: &mut impl pulsebeam_runtime::rand::RngCore) -> Self {
        Self {
            members: fast_set(),
            remote_shards: fast_set(),
            audio_imports: fast_set(),
            audio_selector: TopNAudioSelector::new(rng),
            data_stream_keys: fast_set(),
            all_publisher_subscriptions: AllPublisherSubscriptions::new(),
            reliable: ReliableRoutes::new(),
        }
    }
}

/// The reverse-route descriptor for one published reliable stream: enough to
/// resolve an arriving reverse frame's `topic` by key instead of carrying it
/// inline in [`crate::route::ReverseTarget`]. The forward-direction state
/// (subscribers, publish/import flags, remote handles) still lives in
/// `ReliableRoutes`, inside `RoomFanout` — this arena exists only to give the
/// reverse path a `Copy` key, not yet the full per-stream collapse the
/// hash-container audit describes.
pub(crate) struct ReliableStream {
    pub id: DataStreamId,
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
    /// Routes this shard has installed as a *destination*, indexed by the id it
    /// handed out. Frames arriving from other shards resolve here.
    pub routes: RouteTable,
}

impl DataPlane {
    pub fn new() -> Self {
        Self {
            rooms: SlotMap::with_key(),
            tracks: SlotMap::with_key(),
            data_streams: SlotMap::with_key(),
            reliable_streams: SlotMap::with_key(),
            routes: RouteTable::new(),
        }
    }
}
