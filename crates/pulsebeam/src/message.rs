use crate::media::{KeyframeRequestKind, Rid};
use std::fmt::Debug;

#[derive(Debug)]
pub struct KeyframeRequest {
    pub rid: Option<Rid>,
    pub kind: KeyframeRequestKind,
}

impl From<crate::media::KeyframeRequest> for KeyframeRequest {
    fn from(value: crate::media::KeyframeRequest) -> Self {
        Self {
            rid: value.rid,
            kind: value.kind,
        }
    }
}
