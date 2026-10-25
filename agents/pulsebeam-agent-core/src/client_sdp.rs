//! Local-offerer SDP reconciliation. The host applies candidates and reports the
//! actual result; this module never performs I/O or negotiates with a server.

mod context;
mod sdp;

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
};

pub use context::{ApplyOutcome, Candidate, CandidateId, Context};

pub const MAX_SDP_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Audio,
    Video,
    Application,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Direction {
    #[default]
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

impl Direction {
    pub const fn sends(self) -> bool {
        matches!(self, Self::SendRecv | Self::SendOnly)
    }
    pub const fn receives(self) -> bool {
        matches!(self, Self::SendRecv | Self::RecvOnly)
    }
    pub const fn inverse(self) -> Self {
        match self {
            Self::SendOnly => Self::RecvOnly,
            Self::RecvOnly => Self::SendOnly,
            other => other,
        }
    }
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::SendRecv => "sendrecv",
            Self::SendOnly => "sendonly",
            Self::RecvOnly => "recvonly",
            Self::Inactive => "inactive",
        }
    }
}

/// Format parameters are wire values, not an application codec enum.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodecParameters {
    pub name: String,
    pub clock_rate: u32,
    pub channels: Option<u16>,
    pub fmtp: BTreeMap<String, String>,
    pub feedback: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepairCodec {
    pub payload_type: u8,
    pub clock_rate: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Codec {
    pub payload_type: u8,
    pub parameters: CodecParameters,
    pub repair: Option<RepairCodec>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Extension {
    pub id: u8,
    pub uri: String,
    pub direction: Direction,
}

/// Runtime controls do not participate in the physical wire identity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RuntimeEncoding {
    pub active: Option<bool>,
    pub max_bitrate: Option<u64>,
    pub max_framerate: Option<f64>,
    pub scalability_mode: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Encoding {
    pub rid: Option<String>,
    pub primary: Option<NonZeroU32>,
    pub repair: Option<NonZeroU32>,
    pub paused: bool,
    pub runtime: RuntimeEncoding,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub mid: String,
    pub kind: Kind,
    pub direction: Direction,
    pub stopped: bool,
    pub codecs: Vec<Codec>,
    pub extensions: Vec<Extension>,
    pub encodings: Vec<Encoding>,
}

/// Opaque caller correlation. Several physical bindings can share a group key.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct MediaKey(pub String);

#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub key: MediaKey,
    pub group: Option<MediaKey>,
    pub mid: String,
    pub kind: Kind,
    pub codec: Codec,
    pub extensions: Vec<Extension>,
    pub encodings: Vec<Encoding>,
}

impl Binding {
    /// RID-only sender facts cannot be submitted as admitted RTC bindings.
    pub fn complete(&self) -> bool {
        !self.encodings.is_empty()
            && self
                .encodings
                .iter()
                .all(|e| e.primary.is_some() && (self.codec.repair.is_none() || e.repair.is_some()))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bindings {
    pub publications: Vec<Binding>,
    pub receptions: Vec<Binding>,
}

#[derive(Clone, Debug)]
pub struct MediaOwner {
    pub key: MediaKey,
    pub group: Option<MediaKey>,
}

#[derive(Clone, Debug)]
pub struct Preparation {
    pub mid: String,
    pub payload_type: u8,
    pub publication: Option<MediaOwner>,
    pub reception: Option<MediaOwner>,
}

/// Explicit server support, including when bootstrap contains only SCTP.
#[derive(Clone, Debug)]
pub struct Capabilities {
    pub codecs: Vec<CodecParameters>,
    pub repair: bool,
    pub extensions: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_sections: usize,
    pub max_encodings: usize,
    pub max_identity_history: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sections: 256,
            max_encodings: 256,
            max_identity_history: 8192,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Error {
    #[error("SDP or configuration exceeds a connection limit")]
    Capacity,
    #[error("malformed supported SDP attribute: {0}")]
    Malformed(&'static str),
    #[error("unsupported SDP transport change")]
    TransportChange,
    #[error("unsupported RTP envelope at MID {0}")]
    Envelope(String),
    #[error("new active MID requires configuration: {0}")]
    MissingConfiguration(String),
    #[error("duplicate or invalid media identifier")]
    Identifier,
    #[error("RTP identity collision")]
    Collision,
    #[error("a candidate is already pending or its completion is stale")]
    CandidateState,
    #[error("runtime encoding observations do not match the accepted envelope")]
    Observation,
}

/// Inspect facts from an actual local offer without retaining parallel MID maps.
pub fn inspect(offer: &str) -> Result<Vec<Section>, Error> {
    Ok(sdp::Document::parse(offer, Limits::default())?
        .media
        .into_iter()
        .map(|m| m.section)
        .collect())
}

#[cfg(test)]
mod tests;
