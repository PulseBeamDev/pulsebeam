pub use bytes::Bytes;
pub use str0m::Candidate;
pub use str0m::media::{MediaData, MediaTime};

pub mod actor;
pub mod media;
pub mod signaling;
// pub mod rt;

pub struct MediaFrame {
    pub ts: MediaTime,
    pub data: Bytes,
}

impl From<MediaData> for MediaFrame {
    fn from(value: MediaData) -> Self {
        Self {
            ts: value.time,
            data: value.data.into(),
        }
    }
}
