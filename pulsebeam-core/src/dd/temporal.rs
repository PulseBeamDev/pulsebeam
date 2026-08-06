//! Building Dependency Descriptors for a temporally scalable stream.
//!
//! A single spatial layer with `N` temporal layers ("L1T{N}") lets a forwarder
//! shed frames to reach a lower frame rate: dropping the top temporal layer
//! roughly halves the frame rate, and so on. This module builds the per-frame
//! descriptors a sender attaches so the SFU knows which frames each decode target
//! needs. It is codec-agnostic — a caller supplies each frame's temporal id and
//! whether it is a keyframe; the codec bytes are separate.
//!
//! The structures mirror WebRTC's `ScalabilityStructureL1Tx`: decode target `k`
//! contains temporal layers `0..=k`, so target 0 is the base layer alone and the
//! top target is full frame rate.

use crate::dd::model::{
    DdFields, DecodeTargetIndication, DependencyDescriptor, FrameDependencyTemplate,
    TemplateDependencyStructure,
};

/// Maximum temporal layers this generator builds (L1T3).
pub const MAX_TEMPORAL_LAYERS: u8 = 3;

fn dti(spec: &str) -> arrayvec::ArrayVec<DecodeTargetIndication, 32> {
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

fn template(temporal_id: u8, dti_spec: &str, frame_diff: &[u16]) -> FrameDependencyTemplate {
    FrameDependencyTemplate {
        spatial_id: 0,
        temporal_id,
        dtis: dti(dti_spec),
        frame_diffs: frame_diff.iter().copied().collect(),
        // One chain protecting the base layer; enough for the SFU to reason about
        // temporal targets, whose membership is carried by the DTIs.
        chain_diffs: [frame_diff.first().copied().unwrap_or(0) as u8]
            .into_iter()
            .collect(),
    }
}

/// The L1T{layers} template structure and the per-frame temporal-id pattern.
fn structure_for(layers: u8) -> (TemplateDependencyStructure, &'static [u8]) {
    // The temporal-id pattern over one period; the frame at pattern position i
    // uses the template for that temporal id.
    match layers {
        1 => (
            TemplateDependencyStructure {
                template_id_offset: 0,
                decode_target_count: 1,
                chain_count: 1,
                decode_target_protected_by: [0].into_iter().collect(),
                templates: [template(0, "S", &[1])].into_iter().collect(),
                resolutions: Default::default(),
            },
            &[0],
        ),
        2 => (
            TemplateDependencyStructure {
                template_id_offset: 0,
                decode_target_count: 2,
                chain_count: 1,
                decode_target_protected_by: [0, 0].into_iter().collect(),
                templates: [template(0, "SS", &[2]), template(1, "-D", &[1])]
                    .into_iter()
                    .collect(),
                resolutions: Default::default(),
            },
            &[0, 1],
        ),
        _ => (
            TemplateDependencyStructure {
                template_id_offset: 0,
                decode_target_count: 3,
                chain_count: 1,
                decode_target_protected_by: [0, 0, 0].into_iter().collect(),
                templates: [
                    template(0, "SSS", &[4]),
                    template(1, "-SS", &[2]),
                    template(2, "--D", &[1]),
                ]
                .into_iter()
                .collect(),
                resolutions: Default::default(),
            },
            &[0, 2, 1, 2],
        ),
    }
}

/// Produces the sequence of Dependency Descriptors for an L1T{N} stream.
///
/// Feed it one call per encoded frame; it attaches the template structure on
/// keyframes, advances the frame number, and cycles the temporal pattern.
#[derive(Debug, Clone)]
pub struct TemporalDdGenerator {
    layers: u8,
    structure: TemplateDependencyStructure,
    pattern: &'static [u8],
    frame_number: u16,
    position: usize,
}

impl TemporalDdGenerator {
    /// `layers` is clamped to `1..=MAX_TEMPORAL_LAYERS`.
    pub fn new(layers: u8) -> Self {
        let layers = layers.clamp(1, MAX_TEMPORAL_LAYERS);
        let (structure, pattern) = structure_for(layers);
        Self {
            layers,
            structure,
            pattern,
            frame_number: 0,
            position: 0,
        }
    }

    pub fn temporal_layers(&self) -> u8 {
        self.layers
    }

    pub fn decode_target_count(&self) -> u8 {
        self.structure.decode_target_count
    }

    pub fn structure(&self) -> &TemplateDependencyStructure {
        &self.structure
    }

    /// The temporal id the next frame will carry.
    pub fn next_temporal_id(&self) -> u8 {
        self.pattern[self.position]
    }

    /// Build the descriptor for the next frame. A keyframe restarts the pattern
    /// and carries the structure; a delta frame references the pattern template.
    pub fn next(&mut self, is_keyframe: bool) -> DependencyDescriptor {
        if is_keyframe {
            self.position = 0;
        }
        let template_index = usize::from(self.pattern[self.position]);
        let deps = self.structure.templates[template_index].clone();
        let template_id = ((template_index + usize::from(self.structure.template_id_offset))
            % crate::dd::model::MAX_TEMPLATES) as u8;

        let dd = DependencyDescriptor {
            start_of_frame: true,
            end_of_frame: true,
            template_id,
            frame_number: self.frame_number,
            attached_structure: is_keyframe.then(|| Box::new(self.structure.clone())),
            active_decode_targets: None,
            fields: DdFields::empty(),
            resolution: None,
            frame_dependencies: deps,
        };

        self.frame_number = self.frame_number.wrapping_add(1);
        self.position = (self.position + 1) % self.pattern.len();
        dd
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::dd::read::DependencyDescriptorReader;
    use crate::dd::write::DependencyDescriptorWriter;

    /// Encode a descriptor and read it back through a fresh reader, asserting the
    /// decode-target membership survives — the property the SFU forwarder relies on.
    fn round_trip(descriptors: &[DependencyDescriptor]) {
        let mut writer = DependencyDescriptorWriter::new();
        let mut reader = DependencyDescriptorReader::new();
        for want in descriptors {
            let mut buf = [0u8; 256];
            let n = writer.write(want, &mut buf).expect("encode");
            let got = reader.read(&buf[..n]).expect("decode");
            assert_eq!(
                got.frame_dependencies.temporal_id, want.frame_dependencies.temporal_id,
                "temporal id survives the round trip"
            );
            assert_eq!(
                got.frame_dependencies.dtis, want.frame_dependencies.dtis,
                "decode-target indications survive the round trip"
            );
        }
    }

    #[test]
    fn l1t3_frames_round_trip_through_read_write() {
        let mut g = TemporalDdGenerator::new(3);
        let mut frames = vec![g.next(true)];
        for _ in 0..12 {
            frames.push(g.next(false));
        }
        round_trip(&frames);
    }

    #[test]
    fn l1t2_and_l1t1_round_trip() {
        for layers in [1u8, 2] {
            let mut g = TemporalDdGenerator::new(layers);
            let mut frames = vec![g.next(true)];
            for _ in 0..6 {
                frames.push(g.next(false));
            }
            round_trip(&frames);
        }
    }

    #[test]
    fn base_layer_is_in_every_decode_target() {
        // The first frame after a keyframe is temporal id 0; it must belong to all
        // decode targets so the lowest target still receives it.
        let mut g = TemporalDdGenerator::new(3);
        let kf = g.next(true);
        assert_eq!(kf.temporal_id(), 0);
        for dt in 0..usize::from(g.decode_target_count()) {
            assert!(kf.is_in_decode_target(dt), "base frame missing from dt{dt}");
        }
    }

    #[test]
    fn top_temporal_frames_only_belong_to_the_top_target() {
        // In L1T3 the pattern is [T0, T2, T1, T2]; the T2 frames must be absent from
        // dt0 and dt1 so lowering the target sheds exactly them.
        let mut g = TemporalDdGenerator::new(3);
        let _kf = g.next(true); // T0
        let t2_a = g.next(false); // T2
        assert_eq!(t2_a.temporal_id(), 2);
        assert!(!t2_a.is_in_decode_target(0));
        assert!(!t2_a.is_in_decode_target(1));
        assert!(t2_a.is_in_decode_target(2));

        let t1 = g.next(false); // T1
        assert_eq!(t1.temporal_id(), 1);
        assert!(!t1.is_in_decode_target(0));
        assert!(t1.is_in_decode_target(1));
    }

    #[test]
    fn keyframe_carries_the_structure_delta_does_not() {
        let mut g = TemporalDdGenerator::new(3);
        assert!(g.next(true).attached_structure.is_some());
        assert!(g.next(false).attached_structure.is_none());
    }

    #[test]
    fn decode_target_count_tracks_temporal_layers_and_clamps() {
        assert_eq!(TemporalDdGenerator::new(1).decode_target_count(), 1);
        assert_eq!(TemporalDdGenerator::new(2).decode_target_count(), 2);
        assert_eq!(TemporalDdGenerator::new(3).decode_target_count(), 3);
        assert_eq!(
            TemporalDdGenerator::new(9).decode_target_count(),
            3,
            "clamped to L1T3"
        );
    }
}
