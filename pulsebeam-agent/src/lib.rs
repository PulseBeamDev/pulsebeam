pub use bytes::Bytes;
pub use str0m::Candidate;
pub use str0m::IceConnectionState;
pub use str0m::media::{MediaData, MediaKind, MediaTime};
use tokio::time::Instant;

pub mod actor;
pub mod media;
pub mod signaling;

pub struct MediaFrame {
    pub ts: MediaTime,
    pub data: Bytes,
    pub capture_time: Instant,
}

impl From<MediaData> for MediaFrame {
    fn from(value: MediaData) -> Self {
        Self {
            ts: value.time,
            data: value.data.into(),
            capture_time: value.network_time.into(),
        }
    }
}

pub enum TransceiverDirection {
    SendOnly,
    RecvOnly,
}
