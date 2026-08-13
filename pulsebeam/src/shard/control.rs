//! Names to keys. Publish, subscribe, teardown. Read on the control path —
//! when a track, room or stream is created, subscribed or torn down — never
//! per frame.

use ahash::{HashMap, HashMapExt};

use str0m::media::Rid;

use crate::entity::{ParticipantId, RoomId, TrackId};
use crate::id::ShardId;
use crate::route::{ImportTable, ReverseRoute, RouteId};
use crate::track::Topic;

use super::participants::ParticipantHandle;
use super::router::LocalTrackKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParticipantShardMeta {
    pub shard_id: ShardId,
    pub room_id: RoomId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DataStreamId {
    pub publisher_id: ParticipantId,
    pub topic: Topic,
}

impl DataStreamId {
    pub fn new(publisher_id: ParticipantId, topic: Topic) -> Self {
        Self {
            publisher_id,
            topic,
        }
    }
}

/// Where to send keyframe requests for a track published on another shard, and
/// the encoding order needed to name one of its layers.
pub(crate) struct TrackReverseTarget {
    pub route: ReverseRoute,
    pub encodings: Vec<Option<Rid>>,
}

pub(crate) struct ControlPlane {
    /// Names to keys. Read when a track is published, subscribed or torn
    /// down, never per packet.
    pub track_keys: HashMap<TrackId, LocalTrackKey>,
    // Invariant: `track_keys` and `DataPlane::tracks` are created and removed
    // together, so a key handed to a route always resolves.
    /// Lifecycle of each stream imported from another shard, deciding when a
    /// cluster route is installed and retired.
    pub imports: ImportTable<TrackId>,
    pub data_imports: ImportTable<DataStreamId>,
    /// Separate from `data_imports`: the same (publisher, topic) can exist on
    /// both the realtime and reliable lanes and needs its own route.
    pub reliable_imports: ImportTable<DataStreamId>,
    pub participant_shards: HashMap<ParticipantId, ParticipantShardMeta>,
    pub local_participants: HashMap<ParticipantId, ParticipantHandle>,
    pub remote_participant_counts: HashMap<(RoomId, ShardId), usize>,
    /// Reverse routes this shard opened for the streams it publishes, so they
    /// can be retired when those streams go away.
    pub track_reverse_routes: HashMap<TrackId, RouteId>,
    pub topic_reverse_routes: HashMap<DataStreamId, ReverseRoute>,
    /// Handles for reverse routes *other* shards opened, learned from publisher
    /// announcements — the addresses this shard sends acks to.
    pub topic_reverse_targets: HashMap<DataStreamId, ReverseRoute>,
    /// The same for tracks: where this shard addresses keyframe requests.
    ///
    /// Keyed by track rather than kept in the fanout entry, because it
    /// describes the track and not this shard's subscribers to it. The two have
    /// different lifetimes, and both differences lost the handle: a fanout is
    /// released once its last subscriber leaves, and a shard that gains its
    /// first room member *after* a track was published never runs
    /// `publish_track` for that track at all — so a descriptor kept there was
    /// missing in exactly the case a late subscriber needs it, and its keyframe
    /// requests went nowhere for the life of the track.
    pub track_reverse_targets: HashMap<TrackId, TrackReverseTarget>,
}

impl ControlPlane {
    pub fn new() -> Self {
        Self {
            track_keys: HashMap::new(),
            imports: ImportTable::new(),
            data_imports: ImportTable::new(),
            reliable_imports: ImportTable::new(),
            participant_shards: HashMap::new(),
            local_participants: HashMap::new(),
            remote_participant_counts: HashMap::new(),
            track_reverse_routes: HashMap::new(),
            topic_reverse_routes: HashMap::new(),
            topic_reverse_targets: HashMap::new(),
            track_reverse_targets: HashMap::new(),
        }
    }
}
