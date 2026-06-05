pub use bytes::Bytes;
use std::sync::Arc;
use std::time::SystemTime;
pub use str0m;
pub use str0m::Candidate;
pub use str0m::IceConnectionState;
pub use str0m::media::{MediaData, MediaKind, MediaTime, Mid, Rid, SimulcastLayer};
use tokio::time::Instant;

pub mod clock;

pub use actor::AgentDriver;
pub use clock::wallclock_at;

pub mod actor;
pub mod api;
pub mod manager;
pub mod media;
pub(crate) mod tcp;

#[derive(Debug)]
pub struct MediaFrame {
    pub ts: MediaTime,
    pub data: Arc<[u8]>,
    pub capture_time: Instant,
    pub abs_capture_time: Option<SystemTime>,
}

impl From<MediaData> for MediaFrame {
    fn from(value: MediaData) -> Self {
        Self {
            ts: value.time,
            data: value.data,
            capture_time: value.network_time.into(),
            abs_capture_time: value.ext_vals.abs_capture_time.map(|act| act.capture_time),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransceiverDirection {
    SendOnly,
    RecvOnly,
}
