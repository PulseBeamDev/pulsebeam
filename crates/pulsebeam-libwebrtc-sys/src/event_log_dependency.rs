use crate::{DecodeTargetIndication, EventLogError, RtcEvent, ffi};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventDependencyTemplate {
    pub spatial_id: i32,
    pub temporal_id: i32,
    pub decode_target_indications: Vec<DecodeTargetIndication>,
    pub frame_diffs: Vec<i32>,
    pub chain_diffs: Vec<i32>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtcEventRenderResolution {
    pub width: i32,
    pub height: i32,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventDependencyStructure {
    pub structure_id: i32,
    pub num_decode_targets: i32,
    pub num_chains: i32,
    pub decode_target_protected_by_chain: Vec<i32>,
    pub resolutions: Vec<RtcEventRenderResolution>,
    pub templates: Vec<RtcEventDependencyTemplate>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventDependencyDescriptor {
    pub first_packet_in_frame: bool,
    pub last_packet_in_frame: bool,
    pub frame_number: i32,
    pub frame_dependencies: RtcEventDependencyTemplate,
    pub resolution: Option<RtcEventRenderResolution>,
    pub active_decode_targets_bitmask: Option<u32>,
    pub attached_structure: Option<RtcEventDependencyStructure>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventDependencyDescriptorSnapshot {
    pub raw: Vec<u8>,
    /// Full native parse success, distinct from mandatory-only parse success.
    /// A valid mandatory part may lack the external structure needed here.
    pub decoded: Option<RtcEventDependencyDescriptor>,
}

fn template_native(value: &RtcEventDependencyTemplate) -> ffi::FfiRtcEventDependencyTemplate {
    ffi::FfiRtcEventDependencyTemplate {
        spatial_id: value.spatial_id,
        temporal_id: value.temporal_id,
        decode_target_indications: value
            .decode_target_indications
            .iter()
            .map(|dti| match dti {
                DecodeTargetIndication::NotPresent => 0,
                DecodeTargetIndication::Discardable => 1,
                DecodeTargetIndication::Switch => 2,
                DecodeTargetIndication::Required => 3,
            })
            .collect(),
        frame_diffs: value.frame_diffs.clone(),
        chain_diffs: value.chain_diffs.clone(),
    }
}
fn template_rust(value: ffi::FfiRtcEventDependencyTemplate) -> RtcEventDependencyTemplate {
    RtcEventDependencyTemplate {
        spatial_id: value.spatial_id,
        temporal_id: value.temporal_id,
        decode_target_indications: value
            .decode_target_indications
            .into_iter()
            .map(|dti| match dti {
                0 => DecodeTargetIndication::NotPresent,
                1 => DecodeTargetIndication::Discardable,
                2 => DecodeTargetIndication::Switch,
                // Native parsing reads exactly two bits; no other values exist.
                _ => DecodeTargetIndication::Required,
            })
            .collect(),
        frame_diffs: value.frame_diffs,
        chain_diffs: value.chain_diffs,
    }
}
fn structure_native(
    value: &RtcEventDependencyStructure,
) -> Result<ffi::FfiRtcEventDependencyStructure, EventLogError> {
    // Native reader assumes the saved structure has format-valid cardinalities
    // and matching template arrays. In particular it indexes resolutions by
    // the selected template's spatial ID and reads num_decode_targets bits.
    if !(0..64).contains(&value.structure_id)
        || !(0..=32).contains(&value.num_decode_targets)
        || !(0..=value.num_decode_targets).contains(&value.num_chains)
        || value.templates.len() > 64
        || value.resolutions.len() > 4
        || (value.num_chains > 0
            && (value.decode_target_protected_by_chain.len() != value.num_decode_targets as usize
                || value
                    .decode_target_protected_by_chain
                    .iter()
                    .any(|&chain| !(0..value.num_chains).contains(&chain))))
        || value.templates.iter().any(|template| {
            !(0..4).contains(&template.spatial_id)
                || !(0..8).contains(&template.temporal_id)
                || template.decode_target_indications.len() != value.num_decode_targets as usize
                || template.chain_diffs.len() != value.num_chains as usize
                || (!value.resolutions.is_empty()
                    && template.spatial_id as usize >= value.resolutions.len())
        })
    {
        return Err(EventLogError::InvalidDependencyStructure);
    }
    Ok(ffi::FfiRtcEventDependencyStructure {
        structure_id: value.structure_id,
        num_decode_targets: value.num_decode_targets,
        num_chains: value.num_chains,
        decode_target_protected_by_chain: value.decode_target_protected_by_chain.clone(),
        resolutions: value
            .resolutions
            .iter()
            .map(|resolution| ffi::FfiRtcEventResolution {
                width: resolution.width,
                height: resolution.height,
            })
            .collect(),
        templates: value.templates.iter().map(template_native).collect(),
    })
}
fn structure_rust(value: ffi::FfiRtcEventDependencyStructure) -> RtcEventDependencyStructure {
    RtcEventDependencyStructure {
        structure_id: value.structure_id,
        num_decode_targets: value.num_decode_targets,
        num_chains: value.num_chains,
        decode_target_protected_by_chain: value.decode_target_protected_by_chain,
        resolutions: value
            .resolutions
            .into_iter()
            .map(|resolution| RtcEventRenderResolution {
                width: resolution.width,
                height: resolution.height,
            })
            .collect(),
        templates: value.templates.into_iter().map(template_rust).collect(),
    }
}
impl RtcEvent {
    /// Decode the full dependency descriptor using the native extension trait.
    /// Pass the last successfully received attached structure as context.
    /// An attached structure in this packet overrides that context natively.
    /// A missing structure is not guessed: present-but-unparseable descriptors
    /// retain their raw bytes with `decoded: None`.
    pub fn rtp_dependency_descriptor(
        &self,
        structure: Option<&RtcEventDependencyStructure>,
    ) -> Result<Option<RtcEventDependencyDescriptorSnapshot>, EventLogError> {
        let value = ffi::rtc_event_rtp_dependency_descriptor(
            self.native(),
            structure.is_some(),
            structure
                .map(structure_native)
                .transpose()?
                .unwrap_or_default(),
        );
        if !value.present {
            return Ok(None);
        }
        let decoded = value.parsed.then(|| RtcEventDependencyDescriptor {
            first_packet_in_frame: value.first_packet_in_frame,
            last_packet_in_frame: value.last_packet_in_frame,
            frame_number: value.frame_number,
            frame_dependencies: template_rust(value.frame_dependencies),
            resolution: value.has_resolution.then_some(RtcEventRenderResolution {
                width: value.resolution.width,
                height: value.resolution.height,
            }),
            active_decode_targets_bitmask: value
                .has_active_decode_targets
                .then_some(value.active_decode_targets),
            attached_structure: value
                .has_attached_structure
                .then(|| structure_rust(value.attached_structure)),
        });
        Ok(Some(RtcEventDependencyDescriptorSnapshot {
            raw: value.raw,
            decoded,
        }))
    }
}
