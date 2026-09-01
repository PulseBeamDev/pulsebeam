#![cfg_attr(not(test), forbid(unsafe_code))]
//! Shared-state exception, crate-wide: The test/reference client. Not a shard — it is an ordinary async program.
//! The thread-per-core restriction in `crates/pulsebeam/docs/thread-per-core.md` applies to the
//! `pulsebeam` SFU crate.
#![allow(clippy::disallowed_types)]
#![cfg_attr(
    test,
    allow(
        clippy::unreachable,
        clippy::string_slice,
        clippy::disallowed_methods,
        clippy::float_cmp,
        clippy::arithmetic_side_effects,
    )
)]

pub use agent_core;
use std::sync::Arc;
use std::time::SystemTime;
pub use str0m::media::{Frequency, MediaTime, Mid, Rid, SimulcastLayer};
pub use str0m::rtp::{ExtensionValues, SeqNo, Ssrc};
use tokio::time::Instant;

pub mod clock;
pub use clock::wallclock_at;
pub use pipeline::{FrameReceiver, FrameSender, JitterBuffer};

pub mod media;
pub mod pipeline;
mod runtime;
pub(crate) mod tcp;

pub use runtime::*;

/// One RTP packet — the currency of the agent's media API.
///
/// The agent is a pure RTP transport: it forwards these in and out and never
/// reassembles frames or reads the payload. Frame reassembly, jitter buffering,
/// and end-to-end encryption are higher-level concerns handled above the agent
/// (see [`pipeline`]). `ext_vals` carries the negotiated header extensions —
/// notably the Dependency Descriptor and Video Layers Allocation.
#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub seq: SeqNo,
    pub ts: MediaTime,
    pub marker: bool,
    /// The RTP stream this packet arrived on. `None` before it has been on the wire.
    ///
    /// A slot carries one stream for the whole session and whoever the SFU puts in it, so this
    /// does not say who is speaking - the assignment does. It says how many streams the SFU is
    /// asking a receiver to hold open, which is the thing a browser is unforgiving about.
    pub ssrc: Option<Ssrc>,
    pub payload: Arc<[u8]>,
    pub ext_vals: ExtensionValues,
    /// When the packet was handed over (arrival on ingress, send time on egress).
    pub arrival: Instant,
}

#[derive(Debug, Clone)]
pub struct MediaFrame {
    pub ts: MediaTime,
    pub data: Arc<[u8]>,
    pub capture_time: Instant,
    pub abs_capture_time: Option<SystemTime>,
    /// Whether this frame follows the previous one with no missing packets.
    pub contiguous: bool,
    pub is_keyframe: bool,
    /// Loudness of this audio frame, in negative dBov: 0 is full scale, -30 is ordinary speech,
    /// and quieter is more negative. RFC 6464.
    ///
    /// Required for audio to be forwarded at all. The SFU selects which speakers to send using
    /// this, and drops any audio packet that arrives without it - so an audio frame published
    /// without a level reaches the selector and goes no further. `None` for video.
    pub audio_level: Option<i8>,
    /// Whether this frame contains speech rather than background. The RFC 6464 voice-activity bit.
    pub voice_activity: Option<bool>,
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
    /// How many temporal layers this encoding carries, when scalable. Lets the
    /// sender declare a per-temporal Video Layers Allocation so the SFU can cost
    /// each decode target instead of estimating. `None` for a non-scalable source.
    pub temporal_layers: Option<u8>,
}
