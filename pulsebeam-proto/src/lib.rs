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
