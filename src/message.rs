use bytes::Bytes;
use std::fmt::Debug;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::Arc;
use str0m::media::{self, KeyframeRequestKind, MediaKind, Rid, Simulcast};

pub use str0m::change::{SdpAnswer, SdpOffer};
pub use str0m::error::SdpError;
pub use str0m::{Rtc, RtcError};

use crate::entity::TrackId;

#[derive(Debug)]
pub struct UDPPacket {
    pub raw: Bytes,
    pub src: SocketAddr,
    pub dst: SocketAddr,
}

#[derive(Debug)]
pub struct EgressUDPPacket {
    pub raw: Bytes,
    pub dst: SocketAddr,
}

#[derive(Debug)]
pub struct TrackIn {
    pub id: Arc<TrackId>,
    pub kind: MediaKind,
    pub simulcast: Option<Simulcast>,
}

#[derive(Debug)]
pub struct KeyframeRequest {
    pub rid: Option<Rid>,
    pub kind: KeyframeRequestKind,
}

impl Into<KeyframeRequest> for str0m::media::KeyframeRequest {
    fn into(self) -> KeyframeRequest {
        KeyframeRequest {
            rid: self.rid,
            kind: self.kind,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ActorError {
    #[error("unknown error: {0}")]
    Unknown(String),
}

pub type ActorResult = Result<(), ActorError>;

pub trait ActorId: Hash + Eq + PartialEq + Debug {}
