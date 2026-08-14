//! Names to keys. Publish, subscribe, teardown. Read on the control path —
//! when a track, room or stream is created, subscribed or torn down — never
//! per frame.

use ahash::{HashMap, HashMapExt};

use crate::entity::{ParticipantId, RoomId, TrackId};
use crate::id::ShardId;
use crate::route::ImportTable;
use crate::track::Topic;

use super::router::{DataStreamKey, LocalTrackKey, ReliableStreamKey, RoomKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParticipantShardMeta {
    pub shard_id: ShardId,
    pub room_id: RoomId,
}

/// A data stream's control-plane identity: `(publisher, topic)` **within a
/// room**.
///
/// The room is part of the key rather than an argument callers may forget,
/// because a route being globally unique on the node does not make a
/// cross-room publish, subscribe or teardown legal. With the room inside the
/// key there is no lookup that can omit it, and an operation aimed at the
/// wrong room misses instead of hitting a stream it has no business reaching.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DataStreamId {
    pub room_id: RoomId,
    pub publisher_id: ParticipantId,
    pub topic: Topic,
}

impl DataStreamId {
    pub fn new(room_id: RoomId, publisher_id: ParticipantId, topic: Topic) -> Self {
        Self {
            room_id,
            publisher_id,
            topic,
        }
    }
}

pub(crate) struct ControlPlane {
    /// Names to keys. Read when a room gets its first member or remote
    /// registration, never per packet.
    pub room_keys: HashMap<RoomId, RoomKey>,
    /// Names to keys. Read when a track is published, subscribed or torn
    /// down, never per packet.
    pub track_keys: HashMap<TrackId, LocalTrackKey>,
    /// Names to keys for realtime and reliable data streams, same lifecycle
    /// rule. Two indexes because the same (publisher, topic) can be a stream
    /// on both lanes at once, each with its own arena entry.
    pub data_stream_keys: HashMap<DataStreamId, DataStreamKey>,
    pub reliable_stream_keys: HashMap<DataStreamId, ReliableStreamKey>,
    // Invariant: `room_keys`/`track_keys`/`data_stream_keys`/
    // `reliable_stream_keys` and `DataPlane::rooms`/`tracks`/`data_streams`/
    // `reliable_streams` are created and removed together, so a key handed to
    // a route always resolves.
    /// Lifecycle of each stream imported from another shard, deciding when a
    /// cluster route is installed and retired.
    pub imports: ImportTable<TrackId>,
    pub data_imports: ImportTable<DataStreamId>,
    /// Separate from `data_imports`: the same (publisher, topic) can exist on
    /// both the realtime and reliable lanes and needs its own route.
    pub reliable_imports: ImportTable<DataStreamId>,
    pub participant_shards: HashMap<ParticipantId, ParticipantShardMeta>,
}

impl ControlPlane {
    pub fn new() -> Self {
        Self {
            room_keys: HashMap::new(),
            track_keys: HashMap::new(),
            data_stream_keys: HashMap::new(),
            reliable_stream_keys: HashMap::new(),
            imports: ImportTable::new(),
            data_imports: ImportTable::new(),
            reliable_imports: ImportTable::new(),
            participant_shards: HashMap::new(),
        }
    }
}
