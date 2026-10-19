use crate::{FrameBoundary, FrameDependencies, FrameId, FrameMetadata};
use pulsebeam_core::dd::{
    DdFields, DependencyDescriptor, DependencyDescriptorWriter, MAX_DD_LEN,
    TemplateDependencyStructure,
};
use std::collections::{BTreeMap, VecDeque};

pub(crate) const URIS: [&str; 2] = [
    pulsebeam_core::dd::URI,
    "http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor",
];
const HISTORY: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DescriptorError {
    Invalid,
    MissingReference,
}

pub(crate) fn validate(
    frame: &FrameMetadata,
    selection: Option<u32>,
) -> Result<(), DescriptorError> {
    let Some(source) = &frame.descriptor else {
        return if selection.is_some() {
            Err(DescriptorError::Invalid)
        } else {
            Ok(())
        };
    };
    let FrameDependencies::Known(references) = &frame.dependencies else {
        return Err(DescriptorError::Invalid);
    };
    let structure = &source.structure;
    if !(1..=32).contains(&structure.decode_target_count)
        || structure.chain_count > structure.decode_target_count
        || (structure.chain_count != 0
            && structure.decode_target_protected_by.len()
                != usize::from(structure.decode_target_count))
        || structure
            .decode_target_protected_by
            .iter()
            .any(|chain| *chain >= structure.chain_count)
        || source.template_id >= 64
        || structure.template_id_offset >= 64
        || structure.template(source.template_id).is_none()
        || source.decode_target_indications.len() != usize::from(structure.decode_target_count)
        || source.chain_dependencies.len() != usize::from(structure.chain_count)
        || references.len() > FrameDependencies::MAX_DIRECT_DEPENDENCIES
        || references.iter().any(|reference| *reference == frame.id)
        || source
            .chain_dependencies
            .iter()
            .any(|reference| *reference == Some(frame.id))
        || source.attached_structure
            && !matches!(
                frame.boundary,
                FrameBoundary::Start | FrameBoundary::Complete
            )
    {
        return Err(DescriptorError::Invalid);
    }
    let all = structure.all_decode_targets_bitmask();
    if source.active_decode_targets & !all != 0
        || selection.is_some_and(|mask| mask == 0 || mask & !all != 0)
    {
        return Err(DescriptorError::Invalid);
    }
    Ok(())
}

#[allow(
    clippy::disallowed_types,
    reason = "receiver-local immutable template snapshots are explicitly copied on ownership transfer"
)]
pub(crate) struct DescriptorContinuity {
    next_frame: u64,
    current: Option<(FrameId, u64)>,
    frames: BTreeMap<FrameId, u64>,
    order: VecDeque<FrameId>,
    structure: Option<std::sync::Arc<TemplateDependencyStructure>>,
    writer: DependencyDescriptorWriter,
    active_targets: Option<u32>,
}

#[allow(
    clippy::disallowed_types,
    reason = "prepared output retains an owner-local immutable source template"
)]
pub(crate) struct PreparedDescriptor {
    pub(crate) bytes: Option<Vec<u8>>,
    frame: FrameId,
    number: u64,
    new_frame: bool,
    structure: Option<std::sync::Arc<TemplateDependencyStructure>>,
    replacement_writer: Option<DependencyDescriptorWriter>,
    active_targets: Option<u32>,
}

impl Default for DescriptorContinuity {
    fn default() -> Self {
        Self {
            next_frame: 0,
            current: None,
            frames: BTreeMap::new(),
            order: VecDeque::new(),
            structure: None,
            writer: DependencyDescriptorWriter::new(),
            active_targets: None,
        }
    }
}

impl DescriptorContinuity {
    pub(crate) fn reset_source(&mut self) {
        self.current = None;
        self.frames.clear();
        self.order.clear();
        self.structure = None;
        self.writer.reset();
        self.active_targets = None;
    }

    pub(crate) fn prepare(
        &mut self,
        frame: &FrameMetadata,
        selection: Option<u32>,
    ) -> Result<PreparedDescriptor, DescriptorError> {
        validate(frame, selection)?;
        let new_frame = matches!(
            frame.boundary,
            FrameBoundary::Start | FrameBoundary::Complete
        );
        let number = if new_frame {
            self.next_frame
        } else {
            self.current
                .filter(|(id, _)| *id == frame.id)
                .map(|(_, number)| number)
                .ok_or(DescriptorError::Invalid)?
        };
        let mut prepared = PreparedDescriptor {
            bytes: None,
            frame: frame.id,
            number,
            new_frame,
            structure: None,
            replacement_writer: None,
            active_targets: None,
        };
        let Some(source) = &frame.descriptor else {
            return Ok(prepared);
        };
        let FrameDependencies::Known(references) = &frame.dependencies else {
            return Err(DescriptorError::Invalid);
        };
        let template = source
            .structure
            .template(source.template_id)
            .ok_or(DescriptorError::Invalid)?;
        let targets = source.active_decode_targets & selection.unwrap_or(u32::MAX);
        let mut dependencies = template.clone();
        dependencies.dtis = source.decode_target_indications.iter().copied().collect();
        dependencies.frame_diffs.clear();
        for reference in references.iter() {
            dependencies
                .frame_diffs
                .push(self.remap(*reference, number)?);
        }
        dependencies.chain_diffs.clear();
        for (chain, reference) in source.chain_dependencies.iter().enumerate() {
            let active = source
                .structure
                .decode_target_protected_by
                .iter()
                .enumerate()
                .any(|(target, protected_by)| {
                    usize::from(*protected_by) == chain
                        && u32::try_from(target)
                            .ok()
                            .and_then(|target| 1_u32.checked_shl(target))
                            .is_some_and(|bit| targets & bit != 0)
                });
            let diff = if active {
                reference
                    .map(|id| {
                        self.remap(id, number).and_then(|diff| {
                            u8::try_from(diff).map_err(|_| DescriptorError::MissingReference)
                        })
                    })
                    .transpose()?
                    .unwrap_or(0)
            } else {
                0
            };
            dependencies.chain_diffs.push(diff);
        }
        let mut fields = DdFields::empty();
        fields.set_custom_dtis(dependencies.dtis != template.dtis);
        fields.set_custom_fdiffs(dependencies.frame_diffs != template.frame_diffs);
        fields.set_custom_chains(dependencies.chain_diffs != template.chain_diffs);
        let attach = source.attached_structure
            || self.structure.as_ref().is_none_or(|current| {
                !std::ptr::eq(current.as_ref(), source.structure.as_ref())
                    && current.as_ref() != source.structure.as_ref()
            });
        let previous_targets = if attach {
            Some(source.structure.all_decode_targets_bitmask())
        } else {
            self.active_targets
        };
        let mut descriptor = DependencyDescriptor {
            start_of_frame: new_frame,
            end_of_frame: matches!(frame.boundary, FrameBoundary::End | FrameBoundary::Complete),
            template_id: source.template_id,
            frame_number: wire_number(number),
            fields,
            frame_dependencies: dependencies,
            active_decode_targets: (previous_targets != Some(targets)).then_some(targets),
            ..DependencyDescriptor::default()
        };
        let mut replacement = DependencyDescriptorWriter::new();
        let writer = if attach {
            descriptor.attached_structure = Some(Box::new(source.structure.as_ref().clone()));
            &mut replacement
        } else {
            &mut self.writer
        };
        let mut bytes = [0; MAX_DD_LEN];
        let len = writer
            .write(&descriptor, &mut bytes)
            .map_err(|_| DescriptorError::Invalid)?;
        prepared.bytes = Some(bytes.get(..len).ok_or(DescriptorError::Invalid)?.to_vec());
        prepared.active_targets = Some(targets);
        if attach {
            prepared.structure = Some(source.structure.clone());
            prepared.replacement_writer = Some(replacement);
        }
        Ok(prepared)
    }

    fn remap(&self, reference: FrameId, number: u64) -> Result<u16, DescriptorError> {
        let output = self
            .frames
            .get(&reference)
            .ok_or(DescriptorError::MissingReference)?;
        let delta = number
            .checked_sub(*output)
            .filter(|delta| (1..=4096).contains(delta))
            .ok_or(DescriptorError::MissingReference)?;
        u16::try_from(delta).map_err(|_| DescriptorError::MissingReference)
    }

    pub(crate) fn commit(&mut self, prepared: PreparedDescriptor) {
        if prepared.new_frame {
            self.next_frame = prepared.number.saturating_add(1);
            self.current = Some((prepared.frame, prepared.number));
            if prepared.bytes.is_some() {
                if self.order.len() == HISTORY
                    && let Some(old) = self.order.pop_front()
                {
                    self.frames.remove(&old);
                }
                self.frames.insert(prepared.frame, prepared.number);
                self.order.push_back(prepared.frame);
            }
        }
        if let Some(targets) = prepared.active_targets {
            self.active_targets = Some(targets);
        }
        if let Some(writer) = prepared.replacement_writer {
            self.writer = writer;
            self.structure = prepared.structure;
        }
    }
}

fn wire_number(number: u64) -> u16 {
    u16::try_from(number % 65_536).unwrap_or_default()
}

#[cfg(test)]
#[allow(
    clippy::disallowed_types,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "bounded receiver-continuity fixtures"
)]
mod tests {
    use super::*;
    use crate::{FrameDependencies, SourceDescriptor};
    use pulsebeam_core::dd::{
        DependencyDescriptorReader, RenderResolution, temporal::TemporalDdGenerator,
    };
    use std::sync::Arc;

    struct Source {
        generator: TemporalDdGenerator,
        structure: Arc<TemplateDependencyStructure>,
        next: u64,
    }
    impl Source {
        fn new() -> Self {
            let generator = TemporalDdGenerator::new(3);
            let mut structure = generator.structure().clone();
            structure.resolutions.push(RenderResolution {
                width: 1280,
                height: 720,
            });
            Self {
                generator,
                structure: Arc::new(structure),
                next: 0,
            }
        }
        fn next(&mut self, keyframe: bool) -> FrameMetadata {
            let dd = self.generator.next(keyframe);
            let id = self.next;
            self.next += 1;
            let reference = |diff| FrameId::from_value(id - u64::from(diff));
            FrameMetadata {
                id: FrameId::from_value(id),
                boundary: FrameBoundary::Complete,
                random_access: keyframe,
                discardable: false,
                dependencies: FrameDependencies::known(
                    dd.frame_dependencies
                        .frame_diffs
                        .iter()
                        .map(|diff| reference(*diff))
                        .collect::<Vec<_>>(),
                )
                .unwrap(),
                descriptor: Some(Arc::new(SourceDescriptor {
                    template_id: dd.template_id,
                    decode_target_indications: dd.frame_dependencies.dtis.into_iter().collect(),
                    chain_dependencies: dd
                        .frame_dependencies
                        .chain_diffs
                        .into_iter()
                        .map(|diff| (diff != 0).then(|| reference(u16::from(diff))))
                        .collect(),
                    structure: self.structure.clone(),
                    active_decode_targets: self.structure.all_decode_targets_bitmask(),
                    attached_structure: dd.attached_structure.is_some(),
                })),
            }
        }
    }
    fn facts(frame: &mut FrameMetadata) -> &mut SourceDescriptor {
        Arc::make_mut(frame.descriptor.as_mut().unwrap())
    }
    fn emit(
        continuity: &mut DescriptorContinuity,
        reader: &mut DependencyDescriptorReader,
        frame: &FrameMetadata,
    ) -> DependencyDescriptor {
        let prepared = continuity.prepare(frame, None).expect("prepare");
        let dd = reader
            .read(prepared.bytes.as_ref().expect("bytes"))
            .expect("wire DD");
        continuity.commit(prepared);
        dd
    }

    #[test]
    fn shared_source_facts_survive_explicit_transit_and_independent_cold_joins() {
        fn assert_send<T: Send>() {}
        assert_send::<crate::Connection>();
        let mut source = Source::new();
        source.next(true);
        source.next(false);
        let mut original = source.next(true);
        facts(&mut original).attached_structure = false;
        facts(&mut original).active_decode_targets = 3;
        let transit = original.to_transit();
        let a = original.descriptor.as_ref().unwrap();
        let b = transit.descriptor.as_ref().unwrap();
        assert_eq!(a, b);
        assert!(!Arc::ptr_eq(a, b));
        assert!(!Arc::ptr_eq(&a.structure, &b.structure));
        assert_ne!(
            a.decode_target_indications.as_ptr(),
            b.decode_target_indications.as_ptr()
        );
        assert_ne!(a.chain_dependencies.as_ptr(), b.chain_dependencies.as_ptr());
        for (metadata, selection, expected) in [(&original, Some(1), 1), (&transit, None, 3)] {
            let mut continuity = DescriptorContinuity::default();
            let prepared = continuity.prepare(metadata, selection).unwrap();
            let mut reader = DependencyDescriptorReader::new();
            let dd = reader.read(prepared.bytes.as_ref().unwrap()).unwrap();
            assert_eq!(dd.frame_number, 0);
            assert!(dd.attached_structure.is_some());
            assert_eq!(reader.active_decode_targets(), expected);
            assert_eq!(
                dd.resolution,
                Some(RenderResolution {
                    width: 1280,
                    height: 720
                })
            );
            assert!(dd.frame_dependencies.frame_diffs.is_empty());
            continuity.commit(prepared);
        }
        assert_eq!(a.active_decode_targets, 3);
    }

    #[test]
    fn selected_mask_intersects_source_activity_and_none_preserves_it() {
        let mut source = Source::new();
        for (selection, source_mask, expected) in [
            (None, 3, 3),
            (Some(1), 7, 1),
            (Some(3), 1, 1),
            (Some(4), 3, 0),
        ] {
            let mut key = source.next(true);
            facts(&mut key).active_decode_targets = source_mask;
            let prepared = DescriptorContinuity::default()
                .prepare(&key, selection)
                .unwrap();
            let mut reader = DependencyDescriptorReader::new();
            reader.read(prepared.bytes.as_ref().unwrap()).unwrap();
            assert_eq!(reader.active_decode_targets(), expected);
        }
        let key = source.next(true);
        assert_eq!(validate(&key, Some(8)), Err(DescriptorError::Invalid));
        assert_eq!(validate(&key, Some(0)), Err(DescriptorError::Invalid));
        let mut invalid = key.clone();
        facts(&mut invalid).decode_target_indications =
            vec![crate::DecodeTargetIndication::Required; 33].into();
        assert_eq!(validate(&invalid, None), Err(DescriptorError::Invalid));
        let mut invalid = key.clone();
        facts(&mut invalid).chain_dependencies = [Some(key.id)].into();
        assert_eq!(validate(&invalid, None), Err(DescriptorError::Invalid));
        let mut invalid = key;
        Arc::make_mut(&mut facts(&mut invalid).structure).decode_target_count = 33;
        assert_eq!(validate(&invalid, None), Err(DescriptorError::Invalid));
    }

    #[test]
    fn unknown_dependencies_and_absent_descriptors_are_not_fabricated() {
        let mut frame = Source::new().next(true);
        frame.dependencies = FrameDependencies::Unknown;
        assert_eq!(validate(&frame, None), Err(DescriptorError::Invalid));
        frame.descriptor = None;
        assert_eq!(validate(&frame, Some(1)), Err(DescriptorError::Invalid));
        let mut continuity = DescriptorContinuity::default();
        let prepared = continuity.prepare(&frame, None).unwrap();
        assert!(prepared.bytes.is_none());
        continuity.commit(prepared);
        assert_eq!(continuity.next_frame, 1);
        assert!(continuity.frames.is_empty());
    }

    #[test]
    fn inactive_chains_are_ignored_but_selected_chain_gaps_still_fail_closed() {
        let mut source = Source::new();
        let structure = Arc::make_mut(&mut source.structure);
        structure.chain_count = 3;
        structure.decode_target_protected_by = [0, 1, 2].into_iter().collect();
        for template in &mut structure.templates {
            template.chain_diffs = [0, 0, 0].into_iter().collect();
        }
        let mut key = source.next(true);
        facts(&mut key).chain_dependencies = vec![None; 3].into_boxed_slice();
        facts(&mut key).active_decode_targets = 1;
        let mut continuity = DescriptorContinuity::default();
        let mut reader = DependencyDescriptorReader::new();
        emit(&mut continuity, &mut reader, &key);
        for _ in 0..3 {
            source.next(false);
        }
        let mut base = source.next(false);
        facts(&mut base).chain_dependencies = [
            Some(FrameId::from_value(0)),
            Some(FrameId::from_value(3)),
            Some(FrameId::from_value(3)),
        ]
        .into();
        facts(&mut base).active_decode_targets = 1;
        let dd = emit(&mut continuity, &mut reader, &base);
        assert_eq!(dd.frame_dependencies.frame_diffs.as_slice(), &[1]);
        assert_eq!(dd.frame_dependencies.chain_diffs.as_slice(), &[1, 0, 0]);
        assert_eq!(reader.active_decode_targets(), 1);
        let mut dependent = source.next(false);
        facts(&mut dependent).chain_dependencies = [
            Some(base.id),
            Some(FrameId::from_value(3)),
            Some(FrameId::from_value(3)),
        ]
        .into();
        facts(&mut dependent).active_decode_targets = 2;
        assert!(matches!(
            continuity.prepare(&dependent, None),
            Err(DescriptorError::MissingReference)
        ));
    }

    #[test]
    fn temporal_shedding_remaps_references_without_flattening_source_semantics() {
        let mut source = Source::new();
        let mut continuity = DescriptorContinuity::default();
        let mut reader = DependencyDescriptorReader::new();
        assert_eq!(
            emit(&mut continuity, &mut reader, &source.next(true)).frame_number,
            0
        );
        source.next(false);
        let middle = source.next(false);
        let dd = emit(&mut continuity, &mut reader, &middle);
        assert_eq!(dd.frame_number, 1);
        assert_eq!(dd.frame_dependencies.frame_diffs.as_slice(), &[1]);
        assert_eq!(dd.frame_dependencies.chain_diffs.as_slice(), &[1]);
        assert_eq!(
            dd.frame_dependencies.dtis.as_slice(),
            middle
                .descriptor
                .as_ref()
                .unwrap()
                .decode_target_indications
                .as_ref()
        );
        assert_eq!(dd.temporal_id(), 1);
        assert_eq!(dd.spatial_id(), 0);
        assert_eq!(
            dd.resolution,
            Some(RenderResolution {
                width: 1280,
                height: 720
            })
        );
        source.next(false);
        let base = source.next(false);
        let dd = emit(&mut continuity, &mut reader, &base);
        assert_eq!(dd.frame_number, 2);
        assert_eq!(dd.frame_dependencies.frame_diffs.as_slice(), &[2]);
        assert_eq!(dd.frame_dependencies.chain_diffs.as_slice(), &[2]);
        assert_eq!(reader.structure(), Some(source.structure.as_ref()));
    }

    #[test]
    fn prepare_is_transactional_and_source_reset_keeps_outbound_numbering() {
        let mut source = Source::new();
        let key = source.next(true);
        let mut continuity = DescriptorContinuity::default();
        let first = continuity.prepare(&key, None).unwrap();
        let retry = continuity.prepare(&key, None).unwrap();
        assert_eq!(first.bytes, retry.bytes);
        assert_eq!(continuity.next_frame, 0);
        assert!(continuity.frames.is_empty());
        continuity.commit(retry);
        continuity.reset_source();
        let mut reader = DependencyDescriptorReader::new();
        assert_eq!(
            emit(&mut continuity, &mut reader, &source.next(true)).frame_number,
            1
        );
        assert_eq!(reader.structure(), Some(source.structure.as_ref()));
    }

    #[test]
    fn missing_required_or_chain_predecessors_never_become_independent() {
        let mut source = Source::new();
        let mut continuity = DescriptorContinuity::default();
        let mut reader = DependencyDescriptorReader::new();
        emit(&mut continuity, &mut reader, &source.next(true));
        source.next(false);
        source.next(false);
        let mut dependent = source.next(false);
        assert!(matches!(
            continuity.prepare(&dependent, None),
            Err(DescriptorError::MissingReference)
        ));
        dependent.dependencies = FrameDependencies::known(Vec::new()).unwrap();
        assert!(matches!(
            continuity.prepare(&dependent, None),
            Err(DescriptorError::MissingReference)
        ));
        assert_eq!(continuity.next_frame, 1);
    }

    #[test]
    fn opaque_frame_ids_fragments_and_output_wrap_use_committed_receiver_identity() {
        let mut source = Source::new();
        let mut key = source.next(true);
        key.id = FrameId::from_value(7);
        key.boundary = FrameBoundary::Start;
        let mut continuity = DescriptorContinuity::default();
        continuity.next_frame = u64::from(u16::MAX);
        let mut reader = DependencyDescriptorReader::new();
        assert_eq!(
            emit(&mut continuity, &mut reader, &key).frame_number,
            u16::MAX
        );
        key.boundary = FrameBoundary::End;
        facts(&mut key).attached_structure = false;
        assert_eq!(
            emit(&mut continuity, &mut reader, &key).frame_number,
            u16::MAX
        );
        let mut other = source.next(true);
        other.id = FrameId::from_value(65_543);
        assert_eq!(emit(&mut continuity, &mut reader, &other).frame_number, 0);
        let mut dependent = source.next(false);
        dependent.id = FrameId::from_value(93);
        dependent.dependencies = FrameDependencies::known(vec![key.id]).unwrap();
        facts(&mut dependent).chain_dependencies = [Some(key.id)].into();
        let dd = emit(&mut continuity, &mut reader, &dependent);
        assert_eq!(dd.frame_number, 1);
        assert_eq!(dd.frame_dependencies.frame_diffs.as_slice(), &[2]);
        assert_eq!(dd.frame_dependencies.chain_diffs.as_slice(), &[2]);
    }
    #[test]
    fn committed_reference_history_evicts_at_its_bound() {
        let mut frame = Source::new().next(true);
        facts(&mut frame).attached_structure = false;
        let mut continuity = DescriptorContinuity::default();
        for id in 0..=HISTORY {
            frame.id = FrameId::from_value(u64::try_from(id).unwrap());
            let prepared = continuity.prepare(&frame, None).unwrap();
            continuity.commit(prepared);
        }
        assert_eq!(continuity.frames.len(), HISTORY);
        assert_eq!(continuity.order.len(), HISTORY);
        assert_eq!(
            continuity.remap(FrameId::from_value(0), continuity.next_frame),
            Err(DescriptorError::MissingReference)
        );
        assert_eq!(
            continuity.remap(FrameId::from_value(1), continuity.next_frame),
            Ok(4096)
        );
    }
}
