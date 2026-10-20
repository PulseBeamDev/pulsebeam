use std::fmt::Debug;
use str0m::media::{KeyframeRequestKind, Rid};

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
