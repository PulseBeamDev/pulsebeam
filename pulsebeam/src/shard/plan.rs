//! Everything a frame touches on the forwarding path, and nothing else.
//!
//! [`DataPlane`] is read and mutated by the five dispatch functions in
//! `router.rs`. Names never appear here except as values already resolved by
//! the control plane — see `shard/control.rs`.

use ahash::{HashMap, HashMapExt};

use slotmap::SlotMap;

use crate::audio_selector::TopNAudioSelector;
use crate::entity::{RoomId, TrackId};
use crate::id::ShardId;
use crate::route::{RemoteRoute, RouteTable};
use crate::rtp::cache::TrackStreamCache;
use crate::track::Topic;

use super::control::DataStreamId;
use super::participants::ParticipantHandle;
use super::reliable::ReliableRoutes;
use super::router::{FastIndexSet, LocalTrackKey, fast_set, fast_set_with_capacity};

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
    pub published: bool,
    pub local_subscribers: FastIndexSet<ParticipantHandle>,
    pub remote_subscriber_shards: HashMap<ShardId, RemoteDataSubscriber>,
}

impl DataStreamRoute {
    pub fn new() -> Self {
        Self {
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
    pub data_streams: HashMap<DataStreamId, DataStreamRoute>,
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
            data_streams: HashMap::default(),
            all_publisher_subscriptions: AllPublisherSubscriptions::new(),
            reliable: ReliableRoutes::new(),
        }
    }
}

pub(crate) struct DataPlane {
    pub rooms: HashMap<RoomId, RoomFanout>,
    /// Fanout objects, addressed densely. Arrivals resolve to a key, never a
    /// name: a `TrackId` is a 17-byte value to hash, a key is an index.
    pub tracks: SlotMap<LocalTrackKey, TrackRoute>,
    /// Routes this shard has installed as a *destination*, indexed by the id it
    /// handed out. Frames arriving from other shards resolve here.
    pub routes: RouteTable,
}

impl DataPlane {
    pub fn new() -> Self {
        Self {
            rooms: HashMap::new(),
            tracks: SlotMap::with_key(),
            routes: RouteTable::new(),
        }
    }
}
