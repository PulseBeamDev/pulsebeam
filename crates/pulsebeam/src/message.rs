use std::fmt::Debug;
use str0m::media::{KeyframeRequestKind, Rid};

pub use str0m::change::{SdpAnswer, SdpOffer};
pub use str0m::error::SdpError;
pub use str0m::{Rtc, RtcError};

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
