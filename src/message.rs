use bytes::Bytes;
use std::fmt::Debug;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::Arc;
use str0m::media::{MediaKind, Mid, Simulcast};

pub use str0m::change::{SdpAnswer, SdpOffer};
pub use str0m::error::SdpError;
pub use str0m::{Rtc, RtcError};

use crate::entity::ParticipantId;

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
    pub mid: Mid,
    pub kind: MediaKind,
    pub simulcast: Option<Simulcast>,
}

#[derive(Hash, PartialEq, Eq, Debug, Clone)]
pub struct TrackKey {
    pub origin: Arc<ParticipantId>,
    pub mid: Mid,
}
impl ActorId for TrackKey {}

#[derive(thiserror::Error, Debug)]
pub enum ActorError {
    #[error("unknown error: {0}")]
    Unknown(String),
}

pub type ActorResult = Result<(), ActorError>;

pub trait ActorId: Hash + Eq + PartialEq + Debug {}
