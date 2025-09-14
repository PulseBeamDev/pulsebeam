use std::fmt::Debug;
use std::sync::Arc;
use str0m::media::{KeyframeRequestKind, MediaKind, Rid};

pub use str0m::change::{SdpAnswer, SdpOffer};
pub use str0m::error::SdpError;
pub use str0m::{Rtc, RtcError};

use crate::entity::TrackId;

#[derive(Debug, Eq, PartialEq, Hash)]
pub struct TrackMeta {
    pub id: Arc<TrackId>,
    pub kind: MediaKind,
    pub simulcast_rids: Option<Vec<Rid>>,
}

impl std::fmt::Display for TrackMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.id, f)
    }
}

#[derive(Debug)]
pub struct KeyframeRequest {
    pub rid: Option<Rid>,
    pub kind: KeyframeRequestKind,
}

impl From<str0m::media::KeyframeRequest> for KeyframeRequest {
    fn from(value: str0m::media::KeyframeRequest) -> Self {
        Self {
            rid: value.rid,
            kind: value.kind,
        }
    }
}
