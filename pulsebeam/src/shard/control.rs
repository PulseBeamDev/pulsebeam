//! Names to keys. Publish, subscribe, teardown. Read on the control path —
//! when a track, room or stream is created, subscribed or torn down — never
//! per frame.

use ahash::{HashMap, HashMapExt};

use crate::entity::{ParticipantId, RoomId, TrackId};
use crate::track::Topic;

use super::router::{DataStreamKey, LocalTrackKey, ReliableStreamKey, RoomKey};

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
}

impl ControlPlane {
    pub fn new() -> Self {
        Self {
            room_keys: HashMap::new(),
            track_keys: HashMap::new(),
            data_stream_keys: HashMap::new(),
            reliable_stream_keys: HashMap::new(),
        }
    }
}
