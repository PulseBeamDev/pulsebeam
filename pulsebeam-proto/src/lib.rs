pub mod reliable;
pub mod signaling;
pub mod rtp_extensions {
    /// RTP header extension IDs that are reserved by PulseBeam.
    ///
    /// The default str0m standard extensions are:
    /// 1=audio-level, 2=abs-send-time, 3=transport-cc, 4=mid,
    /// 10=rid, 11=repaired-rid, 13=video-orientation.
    ///
    /// We use 9 for abs-capture-time so it does not collide with these defaults.
    pub const ABS_CAPTURE_TIME: u8 = 9;

    /// Video Layers Allocation (`video-layers-allocation00`). 12 is free of the
    /// str0m defaults above; as the SDP answerer str0m adopts the offerer's id,
    /// so this is only our local preference.
    pub const VIDEO_LAYERS_ALLOCATION: u8 = 12;

    /// Playout delay (`playout-delay`). Sent on egress RTP to bound the
    /// receiver's jitter buffer (see `ClientIntent.max_playout_delay_ms`). 6 is
    /// free of the str0m defaults above.
    pub const PLAYOUT_DELAY: u8 = 6;

    /// AV1 Dependency Descriptor. Not 13 (Chrome's usual id) because that is
    /// str0m's default video-orientation, which registering over would evict;
    /// str0m swaps ids to match the offerer, so both survive Chrome's 13. Must
    /// stay <= 14, above which str0m forces the two-byte form on every packet.
    pub const DEPENDENCY_DESCRIPTOR: u8 = 14;
}

pub mod namespace {
    pub enum Signaling {
        Reliable,
    }

    impl Signaling {
        pub fn as_str(&self) -> &str {
            match self {
                Self::Reliable => "v1/sys/signaling",
            }
        }
    }
}

pub mod prelude {
    pub use prost::Message;
}
