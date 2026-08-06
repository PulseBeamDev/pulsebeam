pub use bytes::Bytes;
use std::sync::Arc;
use std::time::SystemTime;
pub use str0m;
pub use str0m::Candidate;
pub use str0m::IceConnectionState;
pub use str0m::media::{MediaData, MediaKind, MediaTime, Mid, Rid, SimulcastLayer};
use tokio::time::Instant;

pub mod clock;

pub use agent::{
    Agent, AgentBuilder, AgentError, AgentRunner, Connection, ConnectionState, LocalEncoding,
    LocalTrack, Media, Participant, ParticipantChange, Participants, RemoteTrack, RemoteVideo,
    Statistics, StatisticsSnapshot, VideoSubscriber,
};
pub use clock::wallclock_at;

pub mod actor;
pub mod agent;
pub mod api;
pub(crate) mod manager;
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
    /// What the encoder says this layer will cost, if it declares one.
    ///
    /// Sent on to the SFU as a Video Layers Allocation. The distinction from the measured rate is
    /// the point: screen content is genuinely variable - a still desktop encodes almost nothing -
    /// so what a layer *costs* cannot be read from what it happens to be sending. The sender
    /// knows its own target and says so, and the SFU allocates against that rather than against
    /// an instantaneous byte count.
    ///
    /// A real encoder retargets as conditions change, so this is expected to step, not to hold
    /// still: the production log shows the same layer declared at 1250 kbps and later at 729.
    pub target_bitrate_bps: Option<u64>,
    /// Resolution and current frame rate, when known.
    ///
    /// Framerate belongs here because a screen share configured `maintain-resolution` sheds
    /// frames rather than pixels under pressure, so its fps moves continuously while its
    /// resolution does not.
    pub resolution: Option<(u16, u16, u8)>,
    /// The frame's Dependency Descriptor, when the source is scalable and declares
    /// one. Attached verbatim to egress RTP so the SFU can shed temporal/spatial
    /// layers by decode target. `None` for a non-scalable source (the SFU then
    /// falls back to whole-encoding selection).
    pub dependency_descriptor: Option<pulsebeam_core::dd::RawDependencyDescriptor>,
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
            target_bitrate_bps: None,
            resolution: None,
            dependency_descriptor: None,
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
