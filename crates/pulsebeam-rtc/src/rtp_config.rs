//! Typed post-bootstrap media admission on an existing transport.

mod storage;
#[cfg(test)]
mod tests;
mod usage;
use std::collections::{BTreeMap, BTreeSet};
use usage::{Usage, UsageChanges};

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
    pub primary: u32,
    pub repair: Option<u32>,
    pub rid: Option<String>,
    /// False pauses admission without retiring this physical lifetime.
    pub active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcpParameters {
    pub cname: String,
    pub reduced_size: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpBinding {
    pub mid: String,
    pub kind: MediaKind,
    pub codec: RtpCodec,
    pub rtcp: RtcpParameters,
    pub extensions: Vec<RtpExtensionMapping>,
    pub encodings: Vec<RtpEncoding>,
}

/// Complete desired set, from the RTC connection's perspective. Omission
/// permanently retires physical encodings; use `active = false` to pause them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RtpConfiguration {
    pub inbound: Vec<RtpBinding>,
    pub outbound: Vec<RtpBinding>,
}

/// Addressed changes, including initial typed admission. Each supplied binding
/// replaces its direction/MID group, including all of that group's encodings.
/// The first patch pauses unmatched bootstrap candidates for later adoption;
/// subsequent unlisted groups are unchanged.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RtpConfigurationPatch {
    pub inbound: Vec<RtpBinding>,
    pub outbound: Vec<RtpBinding>,
    /// Permanently retire these primary SSRCs in either direction. Unknown or
    /// already removed primaries are no-ops; repair SSRCs are not selectors.
    /// A primary cannot also appear in a supplied binding.
    pub remove_encodings: BTreeSet<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    clippy::disallowed_types,
    reason = "immutable capability backing is shared only at configuration rate"
)]
pub(crate) struct CapabilityContext(std::sync::Arc<RtpCapabilities>);

#[allow(
    clippy::disallowed_types,
    reason = "immutable capability backing is shared only at configuration rate"
)]
impl CapabilityContext {
    fn new(capabilities: RtpCapabilities) -> Result<(Self, usize), RtpConfigurationError> {
        let bytes = capabilities.validate()?;
        Ok((Self(std::sync::Arc::new(capabilities)), bytes))
    }

    fn matches(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0) || self == other
    }
}

impl std::ops::Deref for CapabilityContext {
    type Target = RtpCapabilities;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Validated immutable capability context, reusable outside the runtime owner.
#[derive(Clone, Debug)]
pub struct RtpBindingCompiler {
    capabilities: CapabilityContext,
}

/// Static validation only. Admission, physical identity and capacity checks run
/// against the installed connection when this description is applied.
#[derive(Clone, Debug)]
pub struct CompiledRtpBinding {
    capabilities: CapabilityContext,
    binding: RtpBinding,
    extension_order: Box<[usize]>,
}

impl CompiledRtpBinding {
    pub fn description(&self) -> &RtpBinding {
        &self.binding
    }

    /// Set initial activity for every encoding without repeating static validation.
    /// This only changes the description; applying it still requires live admission.
    pub fn set_active(&mut self, active: bool) {
        for encoding in &mut self.binding.encodings {
            encoding.active = active;
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CompiledRtpConfiguration {
    pub inbound: Vec<CompiledRtpBinding>,
    pub outbound: Vec<CompiledRtpBinding>,
}

#[derive(Clone, Debug, Default)]
pub struct CompiledRtpConfigurationPatch {
    pub inbound: Vec<CompiledRtpBinding>,
    pub outbound: Vec<CompiledRtpBinding>,
    pub remove_encodings: BTreeSet<u32>,
}

impl RtpBindingCompiler {
    pub fn new(capabilities: RtpCapabilities) -> Result<Self, RtpConfigurationError> {
        let (capabilities, _) = CapabilityContext::new(capabilities)?;
        Ok(Self { capabilities })
    }

    pub fn compile(
        &self,
        binding: RtpBinding,
    ) -> Result<CompiledRtpBinding, RtpConfigurationError> {
        validate_binding(&self.capabilities, &binding)?;
        Ok(CompiledRtpBinding {
            capabilities: self.capabilities.clone(),
            extension_order: extension_order(&binding),
            binding,
        })
    }

    pub fn compile_configuration(
        &self,
        configuration: RtpConfiguration,
    ) -> Result<CompiledRtpConfiguration, RtpConfigurationError> {
        Ok(CompiledRtpConfiguration {
            inbound: self.compile_bindings(configuration.inbound)?,
            outbound: self.compile_bindings(configuration.outbound)?,
        })
    }

    pub fn compile_patch(
        &self,
        patch: RtpConfigurationPatch,
    ) -> Result<CompiledRtpConfigurationPatch, RtpConfigurationError> {
        Ok(CompiledRtpConfigurationPatch {
            inbound: self.compile_bindings(patch.inbound)?,
            outbound: self.compile_bindings(patch.outbound)?,
            remove_encodings: patch.remove_encodings,
        })
    }

    fn compile_bindings(
        &self,
        bindings: Vec<RtpBinding>,
    ) -> Result<Vec<CompiledRtpBinding>, RtpConfigurationError> {
        bindings
            .into_iter()
            .map(|binding| self.compile(binding))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpConfigurationLimits {
    /// Retained capability backing/payloads, live typed declaration backing
    /// storage, and MID/encoding index payloads.
    /// Excludes allocator/tree-node overhead, shared format indexes, staging
    /// copies, and media runtime storage; not a process-RSS bound.
    pub max_live_metadata_bytes: usize,
    /// Issued typed SSRC/direction tuple payloads. Retirement never frees these
    /// guards under the same connection/key lifetime.
    pub max_identity_guard_bytes: usize,
}

impl Default for RtpConfigurationLimits {
    fn default() -> Self {
        Self {
            max_live_metadata_bytes: 8 * 1024 * 1024,
            max_identity_guard_bytes: 1024 * 1024,
        }
    }
}

/// One exact format envelope, including format-scoped repair support.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpCodecCapability {
    pub parameters: RtpCodecParameters,
    pub repair: bool,
}

/// Set once, independent of the bootstrap SDP's media sections. Values are
/// supported exact format envelopes, not an instruction to run a negotiator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpCapabilities {
    pub codecs: Vec<RtpCodecCapability>,
    pub extensions: BTreeSet<String>,
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
    #[error("RTP physical identity collides with an issued identity")]
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
    pub(crate) fn ingress_facts(&self) -> crate::negotiation::IngressMediaFacts {
        let binding = &self.binding;
        let codec = &binding.codec;
        let encoding = self.encoding();
        crate::negotiation::IngressMediaFacts {
            kind: binding.kind,
            rtcp: Some(binding.rtcp.clone()),
            mid: binding.mid.clone().into_boxed_str(),
            payloads: vec![crate::RtpPayloadInfo {
                payload_type: codec.payload_type,
                name: codec.parameters.name.to_ascii_lowercase().into_boxed_str(),
                clock_rate: codec.parameters.clock_rate,
            }]
            .into_boxed_slice(),
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
                .map(|repair| (repair, encoding.primary))
                .into_iter()
                .collect(),
            ssrcs: vec![encoding.primary].into_boxed_slice(),
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
            rtcp: Some(self.binding.rtcp.clone()),
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
                .filter(|e| crate::negotiation::forward_extension(&e.uri))
                .map(|e| (e.id, e.uri.clone().into_boxed_str()))
                .collect(),
        }
    }
}

#[derive(Default)]
pub(crate) struct RtpConfigurationState {
    pub(crate) capabilities: Option<CapabilityContext>,
    initialized: bool,
    records: BTreeMap<u64, CurrentBinding>,
    by_mid: BTreeMap<(bool, String), u64>,
    live: BTreeMap<u32, CurrentEncodingLocation>,
    issued: BTreeMap<u32, bool>,
    live_metadata_bytes: usize,
    inbound_reserved: usize,
    next_record: Option<u64>,
    payloads: BTreeMap<u8, Usage<PayloadFormat>>,
    active_payloads: BTreeMap<(bool, u8), Usage<()>>,
    extension_ids: BTreeMap<u8, Usage<String>>,
    extension_uris: BTreeMap<String, Usage<u8>>,
}

#[derive(Clone, Eq, PartialEq)]
struct PayloadFormat {
    codec: RtpCodecParameters,
    repair_of: Option<u8>,
}

#[derive(Clone)]
struct CurrentBinding {
    outbound: bool,
    order: u64,
    binding: RtpBinding,
    extension_order: Box<[usize]>,
}

// Record identities are monotonic; encoding locations never identify media runtime.
struct CurrentEncodingLocation {
    record: u64,
    encoding_index: usize,
}

pub(crate) struct RtpConfigurationUpdate {
    pub(crate) bindings: Vec<(PhysicalBinding, bool)>,
    pub(crate) scope: ConfigurationScope,
    pub(crate) removed_inbound: BTreeSet<u32>,
    pub(crate) removed_outbound: BTreeSet<u32>,
    pub(crate) inbound_membership: BTreeMap<u32, bool>,
    inbound_reserved: usize,
    changes: BTreeMap<u64, Option<CurrentBinding>>,
    next_record: Option<u64>,
    new_ssrcs: BTreeMap<u32, bool>,
    live_metadata_bytes: usize,
    payloads: UsageChanges<u8, PayloadFormat>,
    active_payloads: UsageChanges<(bool, u8), ()>,
    extension_ids: UsageChanges<u8, String>,
    extension_uris: UsageChanges<String, u8>,
}

pub(crate) struct RtpActivityUpdate {
    pub(crate) primary: u32,
    pub(crate) repair: Option<u32>,
    pub(crate) outbound: bool,
    pub(crate) active: bool,
    record: u64,
    encoding_index: usize,
    payloads: UsageChanges<(bool, u8), ()>,
}

impl RtpCapabilities {
    pub(crate) fn validate(&self) -> Result<usize, RtpConfigurationError> {
        let bytes = storage::capability_bytes(self)
            .filter(|bytes| *bytes <= self.limits.max_live_metadata_bytes)
            .ok_or(RtpConfigurationError::Capacity)?;
        if self.feedback != PacketFeedbackKind::TransportWide {
            return Err(RtpConfigurationError::Unsupported);
        }
        for codec in &self.codecs {
            validate_codec(&codec.parameters)?;
        }
        if self.extensions.iter().any(|s| !attribute_value(s)) {
            return Err(RtpConfigurationError::Invalid);
        }
        Ok(bytes)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigurationScope {
    Complete,
    Bootstrap,
    Scoped,
}

#[cfg(test)]
impl From<bool> for ConfigurationScope {
    fn from(complete: bool) -> Self {
        if complete {
            Self::Complete
        } else {
            Self::Scoped
        }
    }
}

impl RtpConfigurationState {
    pub(crate) fn with_capabilities(
        capabilities: RtpCapabilities,
    ) -> Result<Self, RtpConfigurationError> {
        let (capabilities, live_metadata_bytes) = CapabilityContext::new(capabilities)?;
        Ok(Self {
            capabilities: Some(capabilities),
            live_metadata_bytes,
            ..Self::default()
        })
    }

    pub(crate) fn stage(
        &self,
        configuration: RtpConfiguration,
    ) -> Result<RtpConfigurationUpdate, RtpConfigurationError> {
        self.stage_compiled(self.compiler()?.compile_configuration(configuration)?)
    }

    pub(crate) fn compiler(&self) -> Result<RtpBindingCompiler, RtpConfigurationError> {
        Ok(RtpBindingCompiler {
            capabilities: self
                .capabilities
                .as_ref()
                .ok_or(RtpConfigurationError::CapabilityState)?
                .clone(),
        })
    }

    fn unpack(
        &self,
        bindings: Vec<CompiledRtpBinding>,
    ) -> Result<Vec<CompiledRtpBinding>, RtpConfigurationError> {
        let capabilities = self
            .capabilities
            .as_ref()
            .ok_or(RtpConfigurationError::CapabilityState)?;
        bindings
            .into_iter()
            .map(|compiled| {
                if capabilities.matches(&compiled.capabilities) {
                    Ok(compiled)
                } else {
                    Err(RtpConfigurationError::CapabilityState)
                }
            })
            .collect()
    }

    pub(crate) fn stage_compiled(
        &self,
        configuration: CompiledRtpConfiguration,
    ) -> Result<RtpConfigurationUpdate, RtpConfigurationError> {
        let configuration = CompiledRtpConfiguration {
            inbound: self.unpack(configuration.inbound)?,
            outbound: self.unpack(configuration.outbound)?,
        };
        let changes = self.records.keys().map(|&key| (key, None)).collect();
        self.stage_bindings(configuration, changes, true)
    }

    pub(crate) fn stage_patch(
        &self,
        patch: RtpConfigurationPatch,
    ) -> Result<RtpConfigurationUpdate, RtpConfigurationError> {
        self.stage_compiled_patch(self.compiler()?.compile_patch(patch)?)
    }

    pub(crate) fn stage_compiled_patch(
        &self,
        patch: CompiledRtpConfigurationPatch,
    ) -> Result<RtpConfigurationUpdate, RtpConfigurationError> {
        let inbound = self.unpack(patch.inbound)?;
        let outbound = self.unpack(patch.outbound)?;
        let remove_encodings = patch.remove_encodings;
        if inbound
            .iter()
            .chain(&outbound)
            .flat_map(|compiled| &compiled.binding.encodings)
            .any(|encoding| remove_encodings.contains(&encoding.primary))
        {
            return Err(RtpConfigurationError::Invalid);
        }
        let mut changes: BTreeMap<_, _> = remove_encodings
            .iter()
            .filter_map(|ssrc| self.live.get(ssrc))
            .map(|location| (location.record, None))
            .collect();
        for (&key, value) in &mut changes {
            let mut record = self
                .records
                .get(&key)
                .ok_or(RtpConfigurationError::Invalid)?
                .clone();
            record
                .binding
                .encodings
                .retain(|encoding| !remove_encodings.contains(&encoding.primary));
            *value = (!record.binding.encodings.is_empty()).then_some(record);
        }
        self.stage_bindings(
            CompiledRtpConfiguration { inbound, outbound },
            changes,
            false,
        )
    }

    fn stage_bindings(
        &self,
        configuration: CompiledRtpConfiguration,
        mut changes: BTreeMap<u64, Option<CurrentBinding>>,
        reorder: bool,
    ) -> Result<RtpConfigurationUpdate, RtpConfigurationError> {
        let mut next = if self.initialized {
            self.next_record
        } else {
            Some(0)
        };
        let mut mids = BTreeSet::new();
        for (outbound, bindings) in [
            (false, configuration.inbound),
            (true, configuration.outbound),
        ] {
            for (order, compiled) in bindings.into_iter().enumerate() {
                let binding = compiled.binding;
                let mid = (outbound, binding.mid.clone());
                if !mids.insert(mid.clone()) {
                    return Err(RtpConfigurationError::Invalid);
                }
                let key = if let Some(&key) = self.by_mid.get(&mid) {
                    key
                } else {
                    let key = next.ok_or(RtpConfigurationError::Capacity)?;
                    next = key.checked_add(1);
                    key
                };
                changes.insert(
                    key,
                    Some(CurrentBinding {
                        outbound,
                        order: if reorder {
                            order as u64
                        } else {
                            self.records.get(&key).map_or(key, |record| record.order)
                        },
                        binding,
                        extension_order: compiled.extension_order,
                    }),
                );
            }
        }
        let mut update = self.stage_changes(changes, next)?;
        update.scope = if reorder {
            ConfigurationScope::Complete
        } else if self.initialized {
            ConfigurationScope::Scoped
        } else {
            ConfigurationScope::Bootstrap
        };
        Ok(update)
    }

    fn stage_changes(
        &self,
        changes: BTreeMap<u64, Option<CurrentBinding>>,
        next_record: Option<u64>,
    ) -> Result<RtpConfigurationUpdate, RtpConfigurationError> {
        use RtpConfigurationError as Error;
        let caps = self.capabilities.as_ref().ok_or(Error::CapabilityState)?;
        let removed = changes.keys().filter_map(|key| self.records.get(key));
        let removed_bytes = removed
            .clone()
            .try_fold(0usize, |bytes, record| {
                bytes.checked_add(storage::live_bytes(record)?)
            })
            .ok_or(Error::Capacity)?;
        let added_bytes = changes
            .values()
            .flatten()
            .try_fold(0usize, |bytes, record| {
                bytes.checked_add(storage::live_bytes(record)?)
            })
            .ok_or(Error::Capacity)?;
        let live_metadata_bytes = self
            .live_metadata_bytes
            .checked_sub(removed_bytes)
            .ok_or(Error::ConnectionState)?
            .checked_add(added_bytes)
            .filter(|bytes| *bytes <= caps.limits.max_live_metadata_bytes)
            .ok_or(Error::Capacity)?;
        let mut update = RtpConfigurationUpdate {
            bindings: Vec::new(),
            scope: ConfigurationScope::Scoped,
            removed_inbound: BTreeSet::new(),
            removed_outbound: BTreeSet::new(),
            inbound_membership: BTreeMap::new(),
            inbound_reserved: self.inbound_reserved,
            changes,
            next_record,
            new_ssrcs: BTreeMap::new(),
            live_metadata_bytes,
            payloads: UsageChanges::default(),
            active_payloads: UsageChanges::default(),
            extension_ids: UsageChanges::default(),
            extension_uris: UsageChanges::default(),
        };
        for old in update
            .changes
            .keys()
            .filter_map(|key| self.records.get(key))
        {
            for pt in std::iter::once(old.binding.codec.payload_type)
                .chain(old.binding.codec.repair_payload_type)
            {
                update.payloads.release(&self.payloads, &pt)?;
                if old.binding.encodings.iter().any(|encoding| encoding.active) {
                    update
                        .active_payloads
                        .release(&self.active_payloads, &(old.outbound, pt))?;
                }
            }
            for encoding in &old.binding.encodings {
                if old.outbound {
                    update.removed_outbound.insert(encoding.primary);
                } else {
                    update.removed_inbound.insert(encoding.primary);
                    update.inbound_reserved = update.inbound_reserved.saturating_sub(
                        1usize.saturating_add(usize::from(encoding.repair.is_some())),
                    );
                    update.inbound_membership.extend(
                        std::iter::once(encoding.primary)
                            .chain(encoding.repair)
                            .map(|ssrc| (ssrc, false)),
                    );
                }
            }
            for extension in &old.binding.extensions {
                update
                    .extension_ids
                    .release(&self.extension_ids, &extension.id)?;
                update
                    .extension_uris
                    .release(&self.extension_uris, &extension.uri)?;
            }
        }
        let mut mids = BTreeMap::new();
        for (&key, record) in update
            .changes
            .iter()
            .filter_map(|(key, record)| record.as_ref().map(|record| (key, record)))
        {
            let mid = (record.outbound, record.binding.mid.clone());
            if mids.insert(mid.clone(), key).is_some()
                || self
                    .by_mid
                    .get(&mid)
                    .is_some_and(|old| !update.changes.contains_key(old))
            {
                return Err(Error::Invalid);
            }
            for (pt, repair) in [
                (Some(record.binding.codec.payload_type), false),
                (record.binding.codec.repair_payload_type, true),
            ] {
                if let Some(pt) = pt {
                    update.payloads.claim(
                        &self.payloads,
                        pt,
                        PayloadFormat {
                            codec: record.binding.codec.parameters.clone(),
                            repair_of: repair.then_some(record.binding.codec.payload_type),
                        },
                    )?;
                    if record
                        .binding
                        .encodings
                        .iter()
                        .any(|encoding| encoding.active)
                    {
                        update.active_payloads.claim(
                            &self.active_payloads,
                            (record.outbound, pt),
                            (),
                        )?;
                    }
                }
            }
            for extension in &record.binding.extensions {
                update.extension_ids.claim(
                    &self.extension_ids,
                    extension.id,
                    extension.uri.clone(),
                )?;
                update.extension_uris.claim(
                    &self.extension_uris,
                    extension.uri.clone(),
                    extension.id,
                )?;
            }
        }
        let mut identities = BTreeSet::new();
        for record in update.changes.values().flatten() {
            let opposite = (!record.outbound, record.binding.mid.clone());
            let other = mids.get(&opposite).or_else(|| {
                self.by_mid
                    .get(&opposite)
                    .filter(|key| !update.changes.contains_key(key))
            });
            let other = other.and_then(|key| {
                update
                    .changes
                    .get(key)
                    .and_then(Option::as_ref)
                    .or_else(|| self.records.get(key))
            });
            if other.is_some_and(|other| {
                other.binding.kind != record.binding.kind
                    || other.binding.codec != record.binding.codec
            }) {
                return Err(Error::Invalid);
            }
            for encoding in &record.binding.encodings {
                for ssrc in std::iter::once(encoding.primary).chain(encoding.repair) {
                    if !identities.insert(ssrc) {
                        return Err(Error::Collision);
                    }
                }
                let mut identity = encoding.clone();
                identity.active = true;
                let physical = RtpBinding {
                    mid: record.binding.mid.clone(),
                    kind: record.binding.kind,
                    codec: record.binding.codec.clone(),
                    rtcp: record.binding.rtcp.clone(),
                    extensions: record
                        .extension_order
                        .iter()
                        .filter_map(|&index| record.binding.extensions.get(index).cloned())
                        .collect(),
                    encodings: vec![identity],
                };
                let physical = PhysicalBinding {
                    outbound: record.outbound,
                    binding: physical,
                };
                if !self.matches_current(&physical) {
                    if std::iter::once(encoding.primary)
                        .chain(encoding.repair)
                        .any(|ssrc| self.issued.contains_key(&ssrc))
                    {
                        return Err(Error::Collision);
                    }
                    update.new_ssrcs.extend(
                        std::iter::once(encoding.primary)
                            .chain(encoding.repair)
                            .map(|ssrc| (ssrc, record.outbound)),
                    );
                }
                if record.outbound {
                    update.removed_outbound.remove(&encoding.primary);
                } else {
                    update.removed_inbound.remove(&encoding.primary);
                    update.inbound_reserved = update.inbound_reserved.saturating_add(
                        1usize.saturating_add(usize::from(encoding.repair.is_some())),
                    );
                    update.inbound_membership.extend(
                        std::iter::once(encoding.primary)
                            .chain(encoding.repair)
                            .map(|ssrc| (ssrc, encoding.active)),
                    );
                }
                update.bindings.push((physical, encoding.active));
            }
        }
        if self
            .issued
            .len()
            .checked_add(update.new_ssrcs.len())
            .and_then(storage::identity_bytes)
            .is_none_or(|bytes| bytes > caps.limits.max_identity_guard_bytes)
        {
            return Err(Error::Capacity);
        }
        Ok(update)
    }

    fn matches_current(&self, physical: &PhysicalBinding) -> bool {
        let Some(location) = self.live.get(&physical.encoding().primary) else {
            return false;
        };
        let Some(record) = self.records.get(&location.record) else {
            return false;
        };
        let binding = &record.binding;
        let Some(encoding) = binding.encodings.get(location.encoding_index) else {
            return false;
        };
        let incoming = physical.encoding();
        record.outbound == physical.outbound
            && binding.mid == physical.binding.mid
            && binding.kind == physical.binding.kind
            && binding.codec == physical.binding.codec
            && binding.rtcp == physical.binding.rtcp
            && encoding.primary == incoming.primary
            && encoding.repair == incoming.repair
            && encoding.rid == incoming.rid
            && binding.extensions.len() == physical.binding.extensions.len()
            && binding.extensions.iter().all(|old| {
                physical
                    .binding
                    .extensions
                    .binary_search_by_key(&old.id, |extension| extension.id)
                    .ok()
                    .and_then(|index| physical.binding.extensions.get(index))
                    .is_some_and(|new| old.uri == new.uri)
            })
    }

    pub(crate) fn stage_activity(
        &self,
        primary: u32,
        active: bool,
    ) -> Result<Option<RtpActivityUpdate>, RtpConfigurationError> {
        use RtpConfigurationError as Error;
        let location = self.live.get(&primary).ok_or(Error::Invalid)?;
        let record = self
            .records
            .get(&location.record)
            .ok_or(Error::ConnectionState)?;
        let encoding = record
            .binding
            .encodings
            .get(location.encoding_index)
            .ok_or(Error::ConnectionState)?;
        if encoding.active == active {
            return Ok(None);
        }
        let was_active = record
            .binding
            .encodings
            .iter()
            .any(|encoding| encoding.active);
        let will_be_active = active
            || record
                .binding
                .encodings
                .iter()
                .enumerate()
                .any(|(index, encoding)| index != location.encoding_index && encoding.active);
        let mut payloads = UsageChanges::default();
        if was_active != will_be_active {
            for pt in std::iter::once(record.binding.codec.payload_type)
                .chain(record.binding.codec.repair_payload_type)
            {
                if will_be_active {
                    payloads.claim(&self.active_payloads, (record.outbound, pt), ())?;
                } else {
                    payloads.release(&self.active_payloads, &(record.outbound, pt))?;
                }
            }
        }
        Ok(Some(RtpActivityUpdate {
            primary,
            repair: encoding.repair,
            outbound: record.outbound,
            active,
            record: location.record,
            encoding_index: location.encoding_index,
            payloads,
        }))
    }

    pub(crate) fn activity_payload_types(
        &self,
        update: &RtpActivityUpdate,
        outbound: bool,
    ) -> Box<[u8]> {
        update
            .payloads
            .keys(&self.active_payloads)
            .filter_map(|&(direction, pt)| (direction == outbound).then_some(pt))
            .collect()
    }

    pub(crate) fn commit_activity(&mut self, update: RtpActivityUpdate) {
        if let Some(encoding) = self
            .records
            .get_mut(&update.record)
            .and_then(|record| record.binding.encodings.get_mut(update.encoding_index))
        {
            encoding.active = update.active;
        }
        update.payloads.commit(&mut self.active_payloads);
    }

    pub(crate) fn payload_types(
        &self,
        update: &RtpConfigurationUpdate,
        outbound: bool,
    ) -> Box<[u8]> {
        update
            .active_payloads
            .keys(&self.active_payloads)
            .filter_map(|&(direction, pt)| (direction == outbound).then_some(pt))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) fn inbound_reservation(
        &self,
        update: &RtpConfigurationUpdate,
    ) -> (usize, BTreeSet<u32>) {
        let new = update
            .new_ssrcs
            .iter()
            .filter_map(|(&ssrc, &outbound)| (!outbound).then_some(ssrc))
            .collect::<BTreeSet<_>>();
        (update.inbound_reserved, new)
    }

    pub(crate) fn commit(&mut self, update: RtpConfigurationUpdate) {
        for key in update.changes.keys() {
            if let Some(old) = self.records.remove(key) {
                self.by_mid.remove(&(old.outbound, old.binding.mid));
                for encoding in old.binding.encodings {
                    self.live.remove(&encoding.primary);
                }
            }
        }
        for (key, record) in update
            .changes
            .into_iter()
            .filter_map(|(key, record)| record.map(|record| (key, record)))
        {
            self.by_mid
                .insert((record.outbound, record.binding.mid.clone()), key);
            for (encoding_index, encoding) in record.binding.encodings.iter().enumerate() {
                self.live.insert(
                    encoding.primary,
                    CurrentEncodingLocation {
                        record: key,
                        encoding_index,
                    },
                );
            }
            self.records.insert(key, record);
        }
        self.inbound_reserved = update.inbound_reserved;
        self.live_metadata_bytes = update.live_metadata_bytes;
        self.issued.extend(update.new_ssrcs);
        self.next_record = update.next_record;
        update.payloads.commit(&mut self.payloads);
        update.active_payloads.commit(&mut self.active_payloads);
        update.extension_ids.commit(&mut self.extension_ids);
        update.extension_uris.commit(&mut self.extension_uris);
        self.initialized = true;
    }

    pub(crate) fn snapshot(&self) -> Option<RtpConfiguration> {
        if !self.initialized {
            return None;
        }
        let bindings = |outbound| {
            let mut records: Vec<_> = self
                .records
                .values()
                .filter(|record| record.outbound == outbound)
                .collect();
            records.sort_unstable_by_key(|record| record.order);
            records
                .into_iter()
                .map(|record| record.binding.clone())
                .collect()
        };
        Some(RtpConfiguration {
            inbound: bindings(false),
            outbound: bindings(true),
        })
    }
}

fn extension_order(binding: &RtpBinding) -> Box<[usize]> {
    let mut order: Vec<_> = (0..binding.extensions.len()).collect();
    order
        .sort_unstable_by_key(|&index| binding.extensions.get(index).map(|extension| extension.id));
    order.into_boxed_slice()
}

fn validate_binding(
    caps: &RtpCapabilities,
    binding: &RtpBinding,
) -> Result<(), RtpConfigurationError> {
    use RtpConfigurationError as Error;
    if !identifier(&binding.mid)
        || binding.encodings.is_empty()
        || binding.extensions.len() > 64
        || binding.rtcp.cname.is_empty()
        || binding.rtcp.cname.len() > usize::from(u8::MAX)
        || binding
            .rtcp
            .cname
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(Error::Invalid);
    }
    validate_codec(&binding.codec.parameters)?;
    if !caps.codecs.iter().any(|supported| {
        let parameters = &supported.parameters;
        parameters
            .name
            .eq_ignore_ascii_case(&binding.codec.parameters.name)
            && parameters.clock_rate == binding.codec.parameters.clock_rate
            && parameters.channels == binding.codec.parameters.channels
            && parameters.fmtp == binding.codec.parameters.fmtp
            && binding
                .codec
                .parameters
                .feedback
                .is_subset(&parameters.feedback)
            && (binding.codec.repair_payload_type.is_none() || supported.repair)
    }) {
        return Err(Error::Unsupported);
    }
    if binding.codec.parameters.name.eq_ignore_ascii_case("opus")
        != (binding.kind == MediaKind::Audio)
    {
        return Err(Error::Invalid);
    }
    for (payload, repair) in [
        (Some(binding.codec.payload_type), false),
        (binding.codec.repair_payload_type, true),
    ] {
        if let Some(payload) = payload
            && (payload > 127
                || (64..=95).contains(&payload)
                || repair && payload == binding.codec.payload_type)
        {
            return Err(Error::Invalid);
        }
    }
    let mut uris = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for extension in &binding.extensions {
        if extension.id == 0
            || !ids.insert(extension.id)
            || !uris.insert(extension.uri.as_str())
            || !caps.extensions.contains(&extension.uri)
        {
            return Err(Error::Unsupported);
        }
    }
    if !uris.contains(MID_EXTENSION)
        || !crate::negotiation::TWCC_URIS
            .iter()
            .any(|uri| uris.contains(uri))
        || !binding.codec.parameters.feedback.contains("transport-cc")
    {
        return Err(Error::Unsupported);
    }
    let mut rids = BTreeSet::new();
    for encoding in &binding.encodings {
        if encoding.repair.is_some() != binding.codec.repair_payload_type.is_some()
            || encoding
                .rid
                .as_ref()
                .is_some_and(|rid| !identifier(rid) || rid.len() > 32 || !rids.insert(rid))
        {
            return Err(Error::Invalid);
        }
        if encoding.rid.is_some()
            && (!uris.contains(RID_EXTENSION)
                || encoding.repair.is_some() && !uris.contains(REPAIRED_RID_EXTENSION))
        {
            return Err(Error::Unsupported);
        }
    }
    Ok(())
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
