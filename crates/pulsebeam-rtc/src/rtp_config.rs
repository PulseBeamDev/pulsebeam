//! Typed post-bootstrap media admission on an existing transport.

#[cfg(test)]
mod tests;
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
};

use crate::{MediaKind, PacketFeedbackKind};

pub const MID_EXTENSION: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";
pub const RID_EXTENSION: &str = "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id";
pub const REPAIRED_RID_EXTENSION: &str = "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id";
pub const TWCC_EXTENSION: &str =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpCodecParameters {
    pub name: String,
    pub clock_rate: u32,
    pub channels: Option<u16>,
    pub fmtp: BTreeMap<String, String>,
    pub feedback: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpCodec {
    pub payload_type: u8,
    pub parameters: RtpCodecParameters,
    pub repair_payload_type: Option<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpExtensionMapping {
    pub id: u8,
    pub uri: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpEncoding {
    pub primary: NonZeroU32,
    pub repair: Option<NonZeroU32>,
    pub rid: Option<String>,
    pub active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpBinding {
    pub mid: String,
    pub kind: MediaKind,
    pub codec: RtpCodec,
    pub extensions: Vec<RtpExtensionMapping>,
    pub encodings: Vec<RtpEncoding>,
}

/// Complete desired set, from the RTC connection's perspective. Omitting a
/// binding withdraws it. Omitting an encoding withdraws that physical encoding.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RtpConfiguration {
    pub inbound: Vec<RtpBinding>,
    pub outbound: Vec<RtpBinding>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpConfigurationLimits {
    pub max_bindings: usize,
    pub max_active_encodings: usize,
    /// Physical encodings are retained for the entire connection lifetime.
    pub max_retained_encodings: usize,
}

impl Default for RtpConfigurationLimits {
    fn default() -> Self {
        Self {
            max_bindings: 128,
            max_active_encodings: 256,
            max_retained_encodings: 256,
        }
    }
}

/// Set once, independent of the bootstrap SDP's media sections. Values are
/// supported exact format envelopes, not an instruction to run a negotiator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpCapabilities {
    pub codecs: Vec<RtpCodecParameters>,
    pub extensions: BTreeSet<String>,
    pub repair: bool,
    pub feedback: PacketFeedbackKind,
    pub limits: RtpConfigurationLimits,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RtpConfigurationError {
    #[error("typed RTP capability envelope is not initialized or cannot change")]
    CapabilityState,
    #[error("unsupported RTP codec, feedback or extension envelope")]
    Unsupported,
    #[error("invalid or ambiguous RTP binding")]
    Invalid,
    #[error("RTP physical identity collides with retained history")]
    Collision,
    #[error("RTP connection capacity exhausted")]
    Capacity,
    #[error("connection cannot accept media updates in its current state")]
    ConnectionState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PhysicalBinding {
    pub(crate) outbound: bool,
    pub(crate) binding: RtpBinding,
}

impl PhysicalBinding {
    #[allow(
        clippy::indexing_slicing,
        reason = "physical bindings are constructed with exactly one encoding by stage"
    )]
    pub(crate) fn encoding(&self) -> &RtpEncoding {
        &self.binding.encodings[0]
    }
    pub(crate) fn extension_id(&self, uri: &str) -> Option<u8> {
        self.binding
            .extensions
            .iter()
            .find(|e| e.uri == uri)
            .map(|e| e.id)
    }
    pub(crate) fn ingress_facts(&self, index: u32) -> crate::negotiation::IngressMediaFacts {
        let binding = &self.binding;
        let codec = &binding.codec;
        let encoding = self.encoding();
        crate::negotiation::IngressMediaFacts {
            sender_index: index,
            kind: binding.kind,
            mid: binding.mid.clone().into_boxed_str(),
            payloads: vec![(codec.payload_type, codec.parameters.clock_rate)].into_boxed_slice(),
            feedback_payloads: vec![codec.payload_type].into_boxed_slice(),
            repairs: codec
                .repair_payload_type
                .map(|pt| (pt, codec.payload_type))
                .into_iter()
                .collect(),
            nack_payloads: if codec.parameters.feedback.contains("nack") {
                vec![codec.payload_type].into_boxed_slice()
            } else {
                Box::new([])
            },
            ssrc_repairs: encoding
                .repair
                .map(|repair| (repair.get(), encoding.primary.get()))
                .into_iter()
                .collect(),
            ssrcs: vec![encoding.primary.get()].into_boxed_slice(),
            rids: encoding
                .rid
                .iter()
                .map(|r| r.clone().into_boxed_str())
                .collect(),
            extensions: binding
                .extensions
                .iter()
                .map(|e| (e.id, e.uri.clone().into_boxed_str()))
                .collect(),
        }
    }
    pub(crate) fn egress_facts(
        &self,
        id: crate::id::SenderId,
    ) -> crate::negotiation::EgressSenderFacts {
        let codec = &self.binding.codec;
        crate::negotiation::EgressSenderFacts {
            id,
            kind: self.binding.kind,
            mid: self.binding.mid.clone().into_boxed_str(),
            payload_type: codec.payload_type,
            retransmission_payload_type: codec.repair_payload_type,
            clock_rate: codec.parameters.clock_rate,
            mid_extension_id: self.extension_id(MID_EXTENSION),
            twcc_extension_id: crate::negotiation::TWCC_URIS
                .iter()
                .find_map(|uri| self.extension_id(uri)),
            playout_extension_id: self
                .extension_id("http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"),
            dependency_extension_id: crate::negotiation::DEPENDENCY_URIS
                .iter()
                .find_map(|uri| self.extension_id(uri)),
            forwarded_extensions: self
                .binding
                .extensions
                .iter()
                .filter(|e| {
                    ![MID_EXTENSION, RID_EXTENSION, REPAIRED_RID_EXTENSION]
                        .contains(&e.uri.as_str())
                        && !crate::negotiation::TWCC_URIS.contains(&e.uri.as_str())
                        && !crate::negotiation::DEPENDENCY_URIS.contains(&e.uri.as_str())
                        && e.uri != "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"
                })
                .map(|e| (e.id, e.uri.clone().into_boxed_str()))
                .collect(),
        }
    }
    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        self.outbound == other.outbound && self.binding == other.binding
    }
}

#[derive(Default)]
pub(crate) struct RtpConfigurationState {
    pub(crate) capabilities: Option<RtpCapabilities>,
    pub(crate) current: Option<RtpConfiguration>,
    pub(crate) history: Vec<PhysicalBinding>,
}

impl RtpCapabilities {
    pub(crate) fn validate(&self) -> Result<(), RtpConfigurationError> {
        if self.limits.max_bindings == 0
            || self.limits.max_bindings > 256
            || self.limits.max_active_encodings == 0
            || self.limits.max_active_encodings > 256
            || self.limits.max_retained_encodings < self.limits.max_active_encodings
            || self.limits.max_retained_encodings > 256
            || self.codecs.is_empty()
            || self.codecs.len() > 128
            || self.extensions.len() > 256
        {
            return Err(RtpConfigurationError::Capacity);
        }
        if self.feedback != PacketFeedbackKind::TransportWide {
            return Err(RtpConfigurationError::Unsupported);
        }
        for codec in &self.codecs {
            validate_codec(codec)?;
        }
        if self.extensions.iter().any(|s| !attribute_value(s)) {
            return Err(RtpConfigurationError::Invalid);
        }
        Ok(())
    }
}

impl RtpConfigurationState {
    /// Validate the entire update before any media engine is changed.
    pub(crate) fn stage(
        &self,
        configuration: &RtpConfiguration,
    ) -> Result<Vec<(PhysicalBinding, bool)>, RtpConfigurationError> {
        let caps = self
            .capabilities
            .as_ref()
            .ok_or(RtpConfigurationError::CapabilityState)?;
        if configuration
            .inbound
            .len()
            .saturating_add(configuration.outbound.len())
            > caps.limits.max_bindings
        {
            return Err(RtpConfigurationError::Capacity);
        }
        let mut staged = Vec::new();
        let mut identities = BTreeSet::new();
        let mut payloads: BTreeMap<u8, (&RtpCodecParameters, bool)> = BTreeMap::new();
        let mut extension_ids = BTreeMap::new();
        let mut extension_uris = BTreeMap::new();
        let mut duplex: BTreeMap<&str, &RtpBinding> = BTreeMap::new();
        for (outbound, bindings) in [
            (false, &configuration.inbound),
            (true, &configuration.outbound),
        ] {
            let mut mids = BTreeSet::new();
            for binding in bindings {
                if !identifier(&binding.mid)
                    || !mids.insert(&binding.mid)
                    || binding.encodings.is_empty()
                    || binding.extensions.len() > 64
                {
                    return Err(RtpConfigurationError::Invalid);
                }
                if let Some(other) = duplex.insert(&binding.mid, binding) {
                    if other.kind != binding.kind || other.codec != binding.codec {
                        return Err(RtpConfigurationError::Invalid);
                    }
                }
                validate_codec(&binding.codec.parameters)?;
                if !caps.codecs.contains(&binding.codec.parameters)
                    || binding.codec.repair_payload_type.is_some() && !caps.repair
                {
                    return Err(RtpConfigurationError::Unsupported);
                }
                let audio = binding.codec.parameters.name.eq_ignore_ascii_case("opus");
                if audio != (binding.kind == MediaKind::Audio) {
                    return Err(RtpConfigurationError::Invalid);
                }
                for (payload, repair) in [
                    (Some(binding.codec.payload_type), false),
                    (binding.codec.repair_payload_type, true),
                ] {
                    if let Some(payload) = payload {
                        // RFC 5761 reserves 64..95 for RTCP demultiplexing.
                        if payload > 127
                            || (64..=95).contains(&payload)
                            || repair && payload == binding.codec.payload_type
                        {
                            return Err(RtpConfigurationError::Invalid);
                        }
                        let value = (&binding.codec.parameters, repair);
                        if payloads
                            .insert(payload, value)
                            .is_some_and(|old| old != value)
                        {
                            return Err(RtpConfigurationError::Invalid);
                        }
                    }
                }
                let mut uris = BTreeSet::new();
                let mut ids = BTreeSet::new();
                for e in &binding.extensions {
                    if e.id == 0
                        || !ids.insert(e.id)
                        || !uris.insert(e.uri.as_str())
                        || !caps.extensions.contains(&e.uri)
                    {
                        return Err(RtpConfigurationError::Unsupported);
                    }
                    if extension_ids
                        .insert(e.id, &e.uri)
                        .is_some_and(|old| old != &e.uri)
                        || extension_uris
                            .insert(&e.uri, e.id)
                            .is_some_and(|old| old != e.id)
                    {
                        return Err(RtpConfigurationError::Invalid);
                    }
                }
                if !uris.contains(MID_EXTENSION)
                    || !crate::negotiation::TWCC_URIS
                        .iter()
                        .any(|uri| uris.contains(uri))
                    || !binding.codec.parameters.feedback.contains("transport-cc")
                {
                    return Err(RtpConfigurationError::Unsupported);
                }
                let mut rids = BTreeSet::new();
                for encoding in &binding.encodings {
                    if encoding.repair.is_some() != binding.codec.repair_payload_type.is_some()
                        || encoding.rid.as_ref().is_some_and(|rid| {
                            !identifier(rid) || rid.len() > 32 || !rids.insert(rid)
                        })
                    {
                        return Err(RtpConfigurationError::Invalid);
                    }
                    if encoding.rid.is_some()
                        && (!uris.contains(RID_EXTENSION)
                            || encoding.repair.is_some() && !uris.contains(REPAIRED_RID_EXTENSION))
                    {
                        return Err(RtpConfigurationError::Unsupported);
                    }
                    for id in [Some(encoding.primary), encoding.repair]
                        .into_iter()
                        .flatten()
                    {
                        if !identities.insert(id) {
                            return Err(RtpConfigurationError::Collision);
                        }
                    }
                    let mut physical = binding.clone();
                    let mut identity = encoding.clone();
                    identity.active = true;
                    physical.encodings = vec![identity];
                    physical.extensions.sort_by_key(|e| e.id);
                    staged.push((
                        PhysicalBinding {
                            outbound,
                            binding: physical,
                        },
                        encoding.active,
                    ));
                }
            }
        }
        if staged.len() > caps.limits.max_active_encodings {
            return Err(RtpConfigurationError::Capacity);
        }
        let mut new = 0usize;
        for (physical, _) in &staged {
            if self.history.iter().any(|old| old.same_identity(physical)) {
                continue;
            }
            let encoding = physical.encoding();
            if self.history.iter().any(|old| {
                [Some(old.encoding().primary), old.encoding().repair]
                    .into_iter()
                    .flatten()
                    .any(|id| id == encoding.primary || Some(id) == encoding.repair)
            }) {
                return Err(RtpConfigurationError::Collision);
            }
            new = new.saturating_add(1);
        }
        if self.history.len().saturating_add(new) > caps.limits.max_retained_encodings {
            return Err(RtpConfigurationError::Capacity);
        }
        Ok(staged)
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
}
fn attribute_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_whitespace)
}
fn validate_codec(codec: &RtpCodecParameters) -> Result<(), RtpConfigurationError> {
    let name = codec.name.to_ascii_lowercase();
    if codec.fmtp.len() > 64
        || codec.feedback.len() > 16
        || codec
            .fmtp
            .iter()
            .any(|(k, v)| !identifier(k) || v.len() > 1024 || v.contains(['\r', '\n', ';', '\0']))
        || codec
            .feedback
            .iter()
            .any(|f| !matches!(f.as_str(), "transport-cc" | "nack" | "nack pli" | "ccm fir"))
    {
        return Err(RtpConfigurationError::Invalid);
    }
    let supported = match name.as_str() {
        "opus" => codec.clock_rate == 48000 && codec.channels == Some(2),
        "vp8" => codec.clock_rate == 90000 && codec.channels.is_none() && codec.fmtp.is_empty(),
        "vp9" => {
            codec.clock_rate == 90000
                && codec.channels.is_none()
                && codec.fmtp.keys().all(|k| k == "profile-id")
                && codec
                    .fmtp
                    .get("profile-id")
                    .is_none_or(|v| matches!(v.as_str(), "0" | "2"))
        }
        "h264" => {
            codec.clock_rate == 90000
                && codec.channels.is_none()
                && codec
                    .fmtp
                    .get("packetization-mode")
                    .is_some_and(|v| v == "1")
                && codec.fmtp.keys().all(|k| {
                    matches!(
                        k.as_str(),
                        "packetization-mode" | "profile-level-id" | "level-asymmetry-allowed"
                    )
                })
                && codec
                    .fmtp
                    .get("profile-level-id")
                    .is_none_or(|v| v.len() == 6 && v.bytes().all(|b| b.is_ascii_hexdigit()))
                && codec
                    .fmtp
                    .get("level-asymmetry-allowed")
                    .is_none_or(|v| matches!(v.as_str(), "0" | "1"))
        }
        _ => false,
    };
    if !supported {
        return Err(RtpConfigurationError::Unsupported);
    }
    Ok(())
}
