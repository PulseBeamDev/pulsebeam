//! Local-offerer SDP reconciliation. The host applies candidates and reports the
//! actual result; this module never performs I/O or negotiates with a server.

mod capabilities;
mod context;
mod executor;
mod identity_history;
mod sdp;

use std::collections::{BTreeMap, BTreeSet};

pub use context::{ApplyOutcome, Candidate, CandidateId, Context};
pub use executor::Executor;

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
    pub primary: Option<u32>,
    pub repair: Option<u32>,
    pub paused: bool,
    pub runtime: RuntimeEncoding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum RtcpMode {
    Compound,
    ReducedSize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RtcpParameters {
    pub cname: Option<String>,
    pub reduced_size: bool,
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
    pub rtcp: RtcpParameters,
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
    pub rtcp: RtcpParameters,
}

impl Binding {
    pub(crate) fn identities_pending(&self) -> bool {
        !self.encodings.is_empty()
            && self.rtcp.cname.as_deref().is_none_or(sdp::cname)
            && (self.rtcp.cname.is_none()
                || self.encodings.iter().any(|encoding| {
                    encoding.primary.is_none()
                        || self.codec.repair.is_some() && encoding.repair.is_none()
                }))
    }

    /// Missing sender identities or RTCP CNAME cannot grant media admission.
    pub fn complete(&self) -> bool {
        self.rtcp.cname.as_deref().is_some_and(sdp::cname)
            && !self.encodings.is_empty()
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaOwner {
    pub key: MediaKey,
    pub group: Option<MediaKey>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AllocationMode {
    #[default]
    Create,
    /// Retain a live receive-only transceiver and prepare fresh RTP identities.
    RetargetReceiver,
}

/// Logical allocation intent. MID and payload type come from the actual local
/// description of the created or retained physical transceiver.
#[derive(Clone, Debug, PartialEq)]
pub struct Allocation {
    pub slot: crate::MediaSlot,
    pub format: Option<pulsebeam_proto::state::CodecFormat>,
}

// CodecFormat contains only scalar-valued variants.
impl Eq for Allocation {}

impl Allocation {
    fn owner(&self) -> MediaOwner {
        MediaOwner {
            key: crate::rtc_preparation::bootstrap_media_key(&self.slot),
            group: None,
        }
    }

    fn validate(&self) -> Result<(), Error> {
        use crate::{MediaDirection, MediaKind, MediaSlot};
        if let MediaSlot::LocalAudio(name) | MediaSlot::LocalVideo(name) = &self.slot
            && (name.is_empty() || self.owner().key.0.len() > 256)
        {
            return Err(Error::Identifier);
        }
        if let Some(format) = &self.format {
            let audio = pulsebeam_proto::state::validation::validate_format(format)
                .map_err(|_| Error::Identifier)?;
            if audio != (self.slot.kind() == MediaKind::Audio) {
                return Err(Error::Identifier);
            }
        } else if self.slot.direction() == MediaDirection::ReceiveOnly {
            return Err(Error::Identifier);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preparation {
    pub mid: String,
    pub payload_type: u8,
    pub publication: Option<MediaOwner>,
    pub reception: Option<MediaOwner>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodecCapability {
    pub parameters: CodecParameters,
    pub repair: bool,
}

/// Explicit server support, including when bootstrap contains only SCTP.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub codecs: Vec<CodecCapability>,
    pub extensions: BTreeSet<String>,
    pub rtcp_modes: BTreeSet<RtcpMode>,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Issued/retired guard and sender MID/RID/role payloads, including owned
    /// string capacity. Excludes allocator/tree-node overhead and staging;
    /// not a total-memory or process-RSS bound.
    pub max_identity_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_identity_bytes: 1024 * 1024,
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
    #[error("actual sender RTP identities are not yet available")]
    IdentityPending,
}

/// Resolve only newly allocated slots from actual peer MID getters. Older active
/// and stopped sections are not new allocations.
pub fn assigned_slot_bindings(
    offer: &str,
    slots: impl IntoIterator<Item = (crate::MediaSlot, String)>,
) -> Result<Vec<crate::SlotBinding>, Error> {
    let sections = inspect(offer)?;
    let mut seen_slots = BTreeSet::new();
    let mut seen_mids = BTreeSet::new();
    let mut bindings = Vec::new();
    for (slot, mid) in slots {
        if bindings.len() >= sections.len() {
            return Err(Error::Capacity);
        }
        if !seen_slots.insert(slot.clone()) || !seen_mids.insert(mid.clone()) {
            return Err(Error::Identifier);
        }
        let (index, section) = sections
            .iter()
            .enumerate()
            .find(|(_, section)| section.mid == mid)
            .ok_or_else(|| Error::Envelope(mid.clone()))?;
        let kind = match slot.kind() {
            crate::MediaKind::Audio => Kind::Audio,
            crate::MediaKind::Video => Kind::Video,
        };
        let sending = slot.direction() == crate::MediaDirection::SendOnly;
        if section.stopped
            || section.kind != kind
            || section.direction.sends() != sending
            || section.direction.receives() == sending
        {
            return Err(Error::Envelope(mid));
        }
        bindings.push(crate::SlotBinding {
            kind: slot.kind(),
            direction: slot.direction(),
            slot,
            mid,
            media_index: u32::try_from(index).map_err(|_| Error::Capacity)?,
        });
    }
    Ok(bindings)
}

/// Inspect facts from an actual local offer without retaining parallel MID maps.
pub fn inspect(offer: &str) -> Result<Vec<Section>, Error> {
    Ok(sdp::Document::parse(offer)?
        .media
        .into_iter()
        .map(|m| m.section)
        .collect())
}

#[cfg(test)]
mod tests;
