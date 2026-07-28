pub use bytes::Bytes;
use std::sync::Arc;
use std::time::SystemTime;
pub use str0m;
pub use str0m::Candidate;
pub use str0m::IceConnectionState;
pub use str0m::media::{MediaData, MediaKind, MediaTime, Mid, Rid, SimulcastLayer};
use tokio::time::Instant;

pub mod clock;

pub use agent::AgentDriver;
pub use clock::wallclock_at;

pub mod actor;
pub mod agent;
pub mod api;
pub mod manager;
pub mod media;
pub(crate) mod tcp;

#[derive(Debug, Clone)]
pub struct MediaFrame {
    pub ts: MediaTime,
    pub data: Arc<[u8]>,
    pub capture_time: Instant,
    pub abs_capture_time: Option<SystemTime>,
    /// Whether this frame follows the previous one with no missing packets.
    pub contiguous: bool,
    pub is_keyframe: bool,
}

impl From<MediaData> for MediaFrame {
    fn from(value: MediaData) -> Self {
        let is_keyframe = match value.codec_extra {
            str0m::format::CodecExtra::H264(e) => e.is_keyframe,
            str0m::format::CodecExtra::Vp9(e) => e.is_keyframe,
            _ => false,
        };
        Self {
            ts: value.time,
            data: value.data,
            capture_time: value.network_time.into(),
            abs_capture_time: value.ext_vals.abs_capture_time.map(|act| act.capture_time),
            contiguous: value.contiguous,
            is_keyframe,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransceiverDirection {
    SendOnly,
    RecvOnly,
}
