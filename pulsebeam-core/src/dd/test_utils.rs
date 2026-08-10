//! Fixtures for the Dependency Descriptor tests.
//!
//! Overflow is allowed here, unlike the code under test: a fixture that
//! overflows should fail the test loudly rather than clamp into a value that
//! makes it pass.
#![allow(clippy::arithmetic_side_effects)]

use arrayvec::ArrayVec;

use super::model::*;

pub fn dtis(spec: &str) -> ArrayVec<DecodeTargetIndication, MAX_DECODE_TARGETS> {
    spec.chars()
        .map(|c| match c {
            '-' => DecodeTargetIndication::NotPresent,
            'D' => DecodeTargetIndication::Discardable,
            'S' => DecodeTargetIndication::Switch,
            'R' => DecodeTargetIndication::Required,
            other => panic!("unknown decode target indication {other:?}"),
        })
        .collect()
}

pub fn template(
    spatial_id: u8,
    temporal_id: u8,
    dti_spec: &str,
    frame_diffs: &[u16],
    chain_diffs: &[u8],
) -> FrameDependencyTemplate {
    FrameDependencyTemplate {
        spatial_id,
        temporal_id,
        dtis: dtis(dti_spec),
        frame_diffs: frame_diffs.iter().copied().collect(),
        chain_diffs: chain_diffs.iter().copied().collect(),
    }
}

/// Smallest structure the format can express: one template, one decode target,
/// no chains, no resolutions. Anchors the hand-derived byte vector.
pub fn structure_minimal() -> TemplateDependencyStructure {
    TemplateDependencyStructure {
        template_id_offset: 0,
        decode_target_count: 1,
        chain_count: 0,
        decode_target_protected_by: ArrayVec::new(),
        templates: [template(0, 0, "S", &[], &[])].into_iter().collect(),
        resolutions: ArrayVec::new(),
    }
}

pub fn structure_l1t3() -> TemplateDependencyStructure {
    TemplateDependencyStructure {
        template_id_offset: 0,
        decode_target_count: 3,
        chain_count: 1,
        decode_target_protected_by: [0, 0, 0].into_iter().collect(),
        templates: [
            template(0, 0, "SSS", &[4], &[4]),
            template(0, 1, "-SS", &[2], &[2]),
            template(0, 2, "--S", &[1], &[1]),
        ]
        .into_iter()
        .collect(),
        resolutions: ArrayVec::new(),
    }
}

pub fn structure_l2t1_key() -> TemplateDependencyStructure {
    TemplateDependencyStructure {
        template_id_offset: 0,
        decode_target_count: 2,
        chain_count: 2,
        decode_target_protected_by: [0, 1].into_iter().collect(),
        templates: [
            template(0, 0, "S-", &[2], &[2, 1]),
            template(1, 0, "-S", &[1], &[1, 1]),
        ]
        .into_iter()
        .collect(),
        resolutions: ArrayVec::new(),
    }
}

pub fn structure_l3t1_with_resolutions() -> TemplateDependencyStructure {
    TemplateDependencyStructure {
        template_id_offset: 7,
        decode_target_count: 3,
        chain_count: 3,
        decode_target_protected_by: [0, 1, 2].into_iter().collect(),
        templates: [
            template(0, 0, "S--", &[3], &[3, 2, 1]),
            template(1, 0, "-S-", &[2], &[1, 3, 2]),
            template(2, 0, "--S", &[1], &[2, 1, 3]),
        ]
        .into_iter()
        .collect(),
        resolutions: [
            RenderResolution {
                width: 320,
                height: 180,
            },
            RenderResolution {
                width: 640,
                height: 360,
            },
            RenderResolution {
                width: 1280,
                height: 720,
            },
        ]
        .into_iter()
        .collect(),
    }
}

pub fn all_structures() -> Vec<TemplateDependencyStructure> {
    vec![
        structure_minimal(),
        structure_l1t3(),
        structure_l2t1_key(),
        structure_l3t1_with_resolutions(),
    ]
}

/// A keyframe descriptor: carries the structure and selects template 0.
pub fn keyframe(structure: &TemplateDependencyStructure) -> DependencyDescriptor {
    let mut dd = delta(structure, 0, 0);
    dd.attached_structure = Some(Box::new(structure.clone()));
    dd
}

/// A descriptor referencing `template_index` with no extended fields.
pub fn delta(
    structure: &TemplateDependencyStructure,
    template_index: usize,
    frame_number: u16,
) -> DependencyDescriptor {
    let deps = structure.templates[template_index].clone();
    let template_id =
        ((template_index + usize::from(structure.template_id_offset)) % MAX_TEMPLATES) as u8;
    DependencyDescriptor {
        start_of_frame: true,
        end_of_frame: true,
        template_id,
        frame_number,
        attached_structure: None,
        active_decode_targets: None,
        fields: DdFields::empty(),
        resolution: structure
            .resolutions
            .get(usize::from(deps.spatial_id))
            .copied(),
        frame_dependencies: deps,
    }
}
