use pulsebeam_core::dd::{
    DependencyDescriptor, DependencyDescriptorReader, DependencyDescriptorWriter, MAX_DD_LEN,
    TemplateDependencyStructure, read_mandatory,
};

use crate::{FrameBoundary, FrameId, FrameMetadata, MediaPacket};

pub(crate) const URIS: [&str; 2] = [
    pulsebeam_core::dd::URI,
    "http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor",
];
const HISTORY: usize = 4096;

#[allow(
    clippy::disallowed_types,
    reason = "immutable descriptor snapshots are retained only inside one connection; Arc preserves transfer of sole Connection ownership, not shared mutable or cross-shard DTO state"
)]
#[derive(Clone)]
struct TemplateDefinition(std::sync::Arc<TemplateDependencyStructure>);

impl TemplateDefinition {
    #[allow(
        clippy::disallowed_types,
        reason = "connection-private immutable snapshot allocation"
    )]
    fn new(structure: TemplateDependencyStructure) -> Self {
        Self(std::sync::Arc::new(structure))
    }

    fn get(&self) -> &TemplateDependencyStructure {
        &self.0
    }

    fn same(&self, other: &Self) -> bool {
        std::ptr::eq(self.get(), other.get())
    }
}

#[allow(
    clippy::disallowed_types,
    reason = "immutable source-definition metadata stays owner-local; transit explicitly deep-copies it"
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TemplateSeed(bytes::Bytes);

impl TemplateSeed {
    #[allow(
        clippy::disallowed_types,
        reason = "this constructor owns fresh immutable source-definition bytes"
    )]
    pub(crate) fn copy_from_slice(bytes: &[u8]) -> Self {
        Self(bytes::Bytes::copy_from_slice(bytes))
    }
}

impl std::ops::Deref for TemplateSeed {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DescriptorContext {
    pub(crate) definition: TemplateSeed,
    pub(crate) active_targets: u32,
}

#[derive(Default)]
pub(crate) struct DescriptorSource {
    source: Option<u32>,
    reader: DependencyDescriptorReader,
    definition: Option<TemplateSeed>,
}

impl DescriptorSource {
    pub(crate) fn observe(
        &mut self,
        packet: &MediaPacket,
        source: u32,
    ) -> Option<DescriptorContext> {
        if self.source != Some(source) {
            *self = Self::default();
            self.source = Some(source);
        }
        let bytes = URIS.iter().find_map(|uri| packet.extension(uri))?;
        let descriptor = self.reader.read(bytes).ok()?;
        if descriptor.attached_structure.is_some() {
            if !descriptor.start_of_frame {
                self.definition = None;
                return None;
            }
            self.definition = Some(TemplateSeed::copy_from_slice(bytes));
        }
        Some(DescriptorContext {
            definition: self.definition.clone()?,
            active_targets: self.reader.active_decode_targets(),
        })
    }
}

pub(crate) fn valid_context(definition: &[u8], active_targets: u32) -> bool {
    let mut reader = DependencyDescriptorReader::new();
    reader.read(definition).is_ok_and(|descriptor| {
        descriptor.start_of_frame
            && descriptor
                .attached_structure
                .as_ref()
                .is_some_and(|structure| {
                    active_targets & !structure.all_decode_targets_bitmask() == 0
                })
    })
}

pub(crate) fn valid_packet_context(definition: &[u8], active_targets: u32, bytes: &[u8]) -> bool {
    let mut reader = DependencyDescriptorReader::new();
    if reader.read(definition).is_err() {
        return false;
    }
    let structure = reader.structure().cloned();
    reader.read(bytes).is_ok_and(|descriptor| {
        structure.as_ref() == reader.structure()
            && descriptor
                .active_decode_targets
                .is_none_or(|mask| mask == active_targets)
    })
}

#[derive(Default)]
pub(crate) struct DescriptorIngress {
    source: Option<u32>,
    reader: DependencyDescriptorReader,
    structure: Option<TemplateDefinition>,
    definition: Option<TemplateSeed>,
}

pub(crate) struct ParsedDescriptor {
    source: u32,
    descriptor: DependencyDescriptor,
    structure: TemplateDefinition,
    source_attached_structure: bool,
    active_targets: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DescriptorError {
    Invalid,
    MissingReference,
}

impl DescriptorIngress {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn read(
        &mut self,
        packet: &MediaPacket,
        source: u32,
        frame: &FrameMetadata,
    ) -> Result<Option<ParsedDescriptor>, DescriptorError> {
        let Some(bytes) = URIS.iter().find_map(|uri| packet.extension(uri)) else {
            if frame.decode_target_mask.is_some() {
                return Err(DescriptorError::Invalid);
            }
            if self.source != Some(source) {
                self.reset();
                self.source = Some(source);
            }
            return Ok(None);
        };
        let mandatory = read_mandatory(bytes).map_err(|_| DescriptorError::Invalid)?;
        let start = matches!(
            frame.boundary,
            FrameBoundary::Start | FrameBoundary::Complete
        );
        let end = matches!(frame.boundary, FrameBoundary::End | FrameBoundary::Complete);
        if mandatory.start_of_frame != start || mandatory.end_of_frame != end {
            return Err(DescriptorError::Invalid);
        }
        let mut replacement = DependencyDescriptorReader::new();
        let replace = self.source != Some(source)
            || packet
                .descriptor_context()
                .is_some_and(|context| self.definition.as_ref() != Some(&context.definition));
        let reader = if replace {
            &mut replacement
        } else {
            &mut self.reader
        };
        if let Some(context) = packet.descriptor_context()
            && replace
        {
            reader
                .read(&context.definition)
                .map_err(|_| DescriptorError::Invalid)?;
        }
        let mut descriptor = reader.parse(bytes).map_err(|_| DescriptorError::Invalid)?;
        let structure = descriptor
            .attached_structure
            .as_deref()
            .or_else(|| reader.structure())
            .ok_or(DescriptorError::Invalid)?;
        let active_targets = packet.descriptor_context().map_or_else(
            || {
                descriptor.active_decode_targets.unwrap_or_else(|| {
                    if descriptor.attached_structure.is_some() {
                        structure.all_decode_targets_bitmask()
                    } else {
                        reader.active_decode_targets()
                    }
                })
            },
            |context| context.active_targets,
        );
        let active_targets = match frame.decode_target_mask {
            Some(mask) if mask == 0 || mask & !structure.all_decode_targets_bitmask() != 0 => {
                return Err(DescriptorError::Invalid);
            }
            Some(mask) => active_targets & mask,
            None => active_targets,
        };
        let retained = match &self.structure {
            Some(retained) if retained.get() == structure => retained.clone(),
            _ => TemplateDefinition::new(structure.clone()),
        };
        reader.commit(&descriptor);
        if replace {
            self.reader = replacement;
        }
        self.source = Some(source);
        self.definition = packet
            .descriptor_context()
            .map(|context| context.definition.clone());
        self.structure = Some(retained.clone());
        let source_attached_structure = descriptor.attached_structure.take().is_some();
        Ok(Some(ParsedDescriptor {
            source,
            descriptor,
            structure: retained,
            source_attached_structure,
            active_targets,
        }))
    }
}

impl ParsedDescriptor {
    // Charge a whole retained structure even when locally shared. Admission
    // reserves this fixed charge before advancing the source descriptor context.
    pub(crate) const MEMORY_CHARGE: usize =
        size_of::<Self>().saturating_add(size_of::<TemplateDependencyStructure>());

    pub(crate) fn memory_charge(&self) -> usize {
        Self::MEMORY_CHARGE
    }
}

pub(crate) struct DescriptorContinuity {
    next_frame: u64,
    current: Option<(FrameId, u64)>,
    source: Option<u32>,
    frames: Vec<Option<(u16, u64)>>,
    structure: Option<TemplateDefinition>,
    writer: DependencyDescriptorWriter,
    active_targets: Option<u32>,
}

pub(crate) struct PreparedDescriptor {
    pub(crate) bytes: Option<Vec<u8>>,
    frame: FrameId,
    number: u64,
    new_frame: bool,
    source_frame: Option<(u32, u16)>,
    structure: Option<TemplateDefinition>,
    replacement_writer: Option<DependencyDescriptorWriter>,
    active_targets: Option<u32>,
}

impl Default for DescriptorContinuity {
    fn default() -> Self {
        Self {
            next_frame: 0,
            current: None,
            source: None,
            frames: Vec::new(),
            structure: None,
            writer: DependencyDescriptorWriter::new(),
            active_targets: None,
        }
    }
}

impl DescriptorContinuity {
    pub(crate) fn reset_source(&mut self) {
        self.current = None;
        self.source = None;
        self.frames.fill(None);
        self.structure = None;
        self.writer.reset();
        self.active_targets = None;
    }

    pub(crate) fn prepare(
        &mut self,
        frame: &FrameMetadata,
        parsed: Option<&ParsedDescriptor>,
    ) -> Result<PreparedDescriptor, DescriptorError> {
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
            source_frame: None,
            structure: None,
            replacement_writer: None,
            active_targets: None,
        };
        let Some(parsed) = parsed else {
            return Ok(prepared);
        };
        let mut descriptor = parsed.descriptor.clone();
        let source_number = descriptor.frame_number;
        descriptor.frame_number = wire_number(number);
        let mut changed_diffs = false;
        for diff in &mut descriptor.frame_dependencies.frame_diffs {
            let mapped = self.remap_diff(parsed.source, source_number, *diff, number)?;
            changed_diffs |= mapped != *diff;
            *diff = mapped;
        }
        let mut changed_chains = false;
        for (chain, diff) in descriptor
            .frame_dependencies
            .chain_diffs
            .iter_mut()
            .enumerate()
        {
            let active = parsed
                .structure
                .get()
                .decode_target_protected_by
                .iter()
                .enumerate()
                .any(|(target, protected_by)| {
                    usize::from(*protected_by) == chain
                        && u32::try_from(target)
                            .ok()
                            .and_then(|target| 1_u32.checked_shl(target))
                            .is_some_and(|bit| parsed.active_targets & bit != 0)
                });
            if !active {
                // An inactive chain declares no output dependency. Required
                // predecessors below still fail closed, never become zero.
                changed_chains |= *diff != 0;
                *diff = 0;
            } else if *diff != 0 {
                let mapped = u8::try_from(self.remap_diff(
                    parsed.source,
                    source_number,
                    u16::from(*diff),
                    number,
                )?)
                .map_err(|_| DescriptorError::MissingReference)?;
                changed_chains |= mapped != *diff;
                *diff = mapped;
            }
        }
        if changed_diffs {
            descriptor.fields.set_custom_fdiffs(true);
        }
        if changed_chains {
            descriptor.fields.set_custom_chains(true);
        }
        let changed_structure = self.source != Some(parsed.source)
            || self
                .structure
                .as_ref()
                .is_none_or(|current| !current.same(&parsed.structure));
        let attach = parsed.source_attached_structure || changed_structure;
        let previous_targets = if attach {
            Some(parsed.structure.get().all_decode_targets_bitmask())
        } else {
            self.active_targets
        };
        descriptor.active_decode_targets =
            (previous_targets != Some(parsed.active_targets)).then_some(parsed.active_targets);
        prepared.active_targets = Some(parsed.active_targets);
        let mut replacement = DependencyDescriptorWriter::new();
        let writer = if attach {
            descriptor.attached_structure = Some(Box::new(parsed.structure.get().clone()));
            &mut replacement
        } else {
            &mut self.writer
        };
        let mut bytes = [0; MAX_DD_LEN];
        let len = writer
            .write(&descriptor, &mut bytes)
            .map_err(|_| DescriptorError::Invalid)?;
        prepared.bytes = Some(bytes.get(..len).ok_or(DescriptorError::Invalid)?.to_vec());
        prepared.source_frame = Some((parsed.source, source_number));
        if attach {
            prepared.structure = Some(parsed.structure.clone());
            prepared.replacement_writer = Some(replacement);
        }
        Ok(prepared)
    }

    fn remap_diff(
        &self,
        source: u32,
        source_number: u16,
        diff: u16,
        number: u64,
    ) -> Result<u16, DescriptorError> {
        if self.source != Some(source) {
            return Err(DescriptorError::MissingReference);
        }
        let referenced = source_number.wrapping_sub(diff);
        let (retained, output) = self
            .frames
            .get(usize::from(referenced) % HISTORY)
            .copied()
            .flatten()
            .ok_or(DescriptorError::MissingReference)?;
        if retained != referenced {
            return Err(DescriptorError::MissingReference);
        }
        let delta = number
            .checked_sub(output)
            .filter(|delta| (1..=4096).contains(delta))
            .ok_or(DescriptorError::MissingReference)?;
        u16::try_from(delta).map_err(|_| DescriptorError::MissingReference)
    }

    pub(crate) fn commit(&mut self, prepared: PreparedDescriptor) {
        if prepared.new_frame {
            self.next_frame = prepared.number.saturating_add(1);
            self.current = Some((prepared.frame, prepared.number));
        }
        if let Some((source, source_number)) = prepared.source_frame {
            if self.frames.is_empty() {
                self.frames.resize(HISTORY, None);
            }
            if self.source != Some(source) {
                self.frames.fill(None);
                self.source = Some(source);
            }
            if let Some(slot) = self.frames.get_mut(usize::from(source_number) % HISTORY) {
                *slot = Some((source_number, prepared.number));
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
    reason = "bounded protocol fixtures"
)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use pulsebeam_core::dd::{RenderResolution, temporal::TemporalDdGenerator};

    struct Source {
        generator: TemporalDdGenerator,
        structure: TemplateDefinition,
        writer: DependencyDescriptorWriter,
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
                structure: TemplateDefinition::new(structure),
                writer: DependencyDescriptorWriter::new(),
            }
        }

        fn next(&mut self, keyframe: bool) -> ParsedDescriptor {
            let mut descriptor = self.generator.next(keyframe);
            let source_attached_structure = descriptor.attached_structure.take().is_some();
            ParsedDescriptor {
                source: 42,
                descriptor,
                structure: self.structure.clone(),
                source_attached_structure,
                active_targets: self.structure.get().all_decode_targets_bitmask(),
            }
        }
    }

    impl Source {
        fn packet(&mut self, parsed: &ParsedDescriptor) -> MediaPacket {
            let mut descriptor = parsed.descriptor.clone();
            descriptor.attached_structure = parsed
                .source_attached_structure
                .then(|| Box::new(parsed.structure.get().clone()));
            let mut value = [0; MAX_DD_LEN];
            let len = self.writer.write(&descriptor, &mut value).unwrap();
            let mut extension = vec![5, u8::try_from(len).unwrap()];
            extension.extend_from_slice(&value[..len]);
            while !extension.len().is_multiple_of(4) {
                extension.push(0);
            }
            let mut bytes = vec![0x90, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 42, 0x10, 0];
            bytes.extend_from_slice(&u16::try_from(extension.len() / 4).unwrap().to_be_bytes());
            bytes.extend_from_slice(&extension);
            bytes.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
            MediaPacket::new(
                Bytes::from(bytes),
                crate::GlobalMediaTime::from_micros(42),
                vec![(std::sync::Arc::from(URIS[0]), 18..18 + len)].into(),
            )
        }
    }

    #[test]
    fn cold_join_keeps_source_context_through_owned_transit_without_replaying_its_frame() {
        fn assert_send<T: Send>() {}
        assert_send::<crate::Connection>();
        let mut source = Source::new();
        let key = source.next(true);
        let mut observer = DescriptorSource::default();
        observer.observe(&source.packet(&key), 42).unwrap();
        let mut delta = source.next(false);
        delta.descriptor.active_decode_targets = Some(3);
        observer.observe(&source.packet(&delta), 42).unwrap();
        let mut independent = source.next(true);
        independent.source_attached_structure = false;
        independent.descriptor.active_decode_targets = None;
        let mut packet = source.packet(&independent);
        let original = packet.bytes().clone();
        let context = observer.observe(&packet, 42).unwrap();
        packet.set_descriptor_context(Some(context));
        let transit = packet.to_transit();
        let (a, _) = packet.dependency_descriptor_context().unwrap();
        let (b, _) = transit.dependency_descriptor_context().unwrap();
        assert_ne!(a.as_ptr(), b.as_ptr());
        let materialized = transit.materialize().unwrap();
        assert_eq!(materialized.as_bytes()[4], 2);
        for len in 0..materialized.as_bytes().len() {
            assert!(
                crate::MaterializedMediaPacket::from_bytes(materialized.as_bytes()[..len].into())
                    .is_err()
            );
        }
        let restored = crate::MaterializedMediaPacket::from_bytes(materialized.into_bytes())
            .unwrap()
            .into_packet()
            .unwrap();
        assert_eq!(restored.bytes(), &original);
        assert_eq!(restored.global_media_at(), packet.global_media_at());
        assert_eq!(
            restored.dependency_descriptor_context(),
            packet.dependency_descriptor_context()
        );
        let mut ingress = DescriptorIngress::default();
        let metadata = frame(1);
        let parsed = ingress.read(&restored, 42, &metadata).unwrap().unwrap();
        let mut continuity = DescriptorContinuity::default();
        let mut reader = DependencyDescriptorReader::new();
        let emitted = emit(&mut continuity, &mut reader, &parsed);
        assert_eq!(emitted.frame_number, 0);
        assert!(emitted.attached_structure.is_some());
        assert_eq!(emitted.active_decode_targets, Some(3));
        assert_eq!(
            emitted.resolution,
            Some(RenderResolution {
                width: 1280,
                height: 720
            })
        );
        assert!(emitted.frame_dependencies.frame_diffs.is_empty());
    }

    #[test]
    fn materialized_context_rejects_bad_mask_or_wrong_source_definition() {
        let mut source = Source::new();
        let key = source.next(true);
        let mut observer = DescriptorSource::default();
        let mut packet = source.packet(&key);
        let mut context = observer.observe(&packet, 42).unwrap();
        context.active_targets = 8;
        packet.set_descriptor_context(Some(context.clone()));
        assert_eq!(
            packet.materialize().unwrap_err(),
            crate::MediaTransitError::Invalid
        );
        context.active_targets = 7;
        let mut other = Source::new();
        let mut other_key = other.next(true);
        let mut other_structure = other_key.structure.get().clone();
        other_structure.resolutions[0].width = 640;
        other_key.structure = TemplateDefinition::new(other_structure);
        let other_packet = other.packet(&other_key);
        context.definition =
            TemplateSeed::copy_from_slice(other_packet.extension(URIS[0]).unwrap());
        packet.set_descriptor_context(Some(context));
        assert_eq!(
            packet.materialize().unwrap_err(),
            crate::MediaTransitError::Invalid
        );
    }

    fn frame(id: u64) -> FrameMetadata {
        FrameMetadata {
            decode_target_mask: None,
            id: FrameId::from_value(id),
            boundary: FrameBoundary::Complete,
            random_access: false,
            discardable: false,
            dependencies: crate::FrameDependencies::Unknown,
        }
    }

    fn emit(
        continuity: &mut DescriptorContinuity,
        reader: &mut DependencyDescriptorReader,
        parsed: &ParsedDescriptor,
    ) -> DependencyDescriptor {
        let prepared = continuity
            .prepare(
                &frame(u64::from(parsed.descriptor.frame_number)),
                Some(parsed),
            )
            .expect("prepare");
        let descriptor = reader
            .read(prepared.bytes.as_ref().expect("bytes"))
            .expect("wire DD");
        continuity.commit(prepared);
        descriptor
    }

    #[test]
    fn invalid_selection_cannot_replace_source_definition_or_activity() {
        for with_context in [false, true] {
            let mut source = Source::new();
            let key = source.next(true);
            let mut ingress = DescriptorIngress::default();
            ingress.read(&source.packet(&key), 42, &frame(0)).unwrap();
            let delta = source.next(false);
            let delta_packet = source.packet(&delta);

            let mut rejected = source.next(true);
            let mut structure = rejected.structure.get().clone();
            structure.resolutions[0].width = 640;
            rejected.structure = TemplateDefinition::new(structure);
            rejected.descriptor.active_decode_targets = Some(1);
            let mut packet = source.packet(&rejected);
            if with_context {
                let context = DescriptorSource::default().observe(&packet, 42).unwrap();
                packet.set_descriptor_context(Some(context));
            }
            let mut invalid = frame(2);
            invalid.decode_target_mask = Some(8);
            assert!(matches!(
                ingress.read(&packet, 42, &invalid),
                Err(DescriptorError::Invalid)
            ));

            let accepted = ingress.read(&delta_packet, 42, &frame(1)).unwrap().unwrap();
            assert_eq!(accepted.active_targets, 7);
            assert_eq!(accepted.structure.get(), key.structure.get());
        }
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
            key.descriptor.active_decode_targets = Some(source_mask);
            let packet = source.packet(&key);
            let mut metadata = frame(u64::from(key.descriptor.frame_number));
            metadata.decode_target_mask = selection;
            let parsed = DescriptorIngress::default()
                .read(&packet, 42, &metadata)
                .unwrap()
                .unwrap();
            let mut reader = DependencyDescriptorReader::new();
            emit(&mut DescriptorContinuity::default(), &mut reader, &parsed);
            assert_eq!(reader.active_decode_targets(), expected);
        }
    }

    #[test]
    fn selection_without_source_descriptor_is_rejected_not_fabricated() {
        let mut metadata = frame(0);
        metadata.decode_target_mask = Some(1);
        let packet = MediaPacket::new(
            bytes::Bytes::from_static(&[0x80, 96, 0, 0, 0, 0, 0, 0, 0, 0, 0, 42, 0]),
            crate::GlobalMediaTime::from_micros(42),
            std::sync::Arc::from([]),
        );
        assert!(matches!(
            DescriptorIngress::default().read(&packet, 42, &metadata),
            Err(DescriptorError::Invalid)
        ));
    }

    #[test]
    fn inactive_chains_are_ignored_but_selected_chain_gaps_still_fail_closed() {
        let mut source = Source::new();
        let mut structure = source.structure.get().clone();
        structure.chain_count = 3;
        structure.decode_target_protected_by = [0, 1, 2].into_iter().collect();
        for template in &mut structure.templates {
            template.chain_diffs = [0, 0, 0].into_iter().collect();
        }
        source.structure = TemplateDefinition::new(structure);
        let mut key = source.next(true);
        key.descriptor.frame_dependencies.chain_diffs = [0, 0, 0].into_iter().collect();
        key.active_targets = 1;
        let mut continuity = DescriptorContinuity::default();
        let mut reader = DependencyDescriptorReader::new();
        emit(&mut continuity, &mut reader, &key);
        for _ in 0..3 {
            source.next(false);
        }
        let mut base = source.next(false);
        base.descriptor.frame_dependencies.chain_diffs = [4, 1, 1].into_iter().collect();
        base.active_targets = 1;
        let emitted = emit(&mut continuity, &mut reader, &base);
        assert_eq!(emitted.frame_dependencies.frame_diffs.as_slice(), &[1]);
        assert_eq!(
            emitted.frame_dependencies.chain_diffs.as_slice(),
            &[1, 0, 0]
        );
        assert_eq!(reader.active_decode_targets(), 1);
        assert_eq!(reader.structure(), Some(source.structure.get()));
        base.descriptor.frame_number = 5;
        base.descriptor.frame_dependencies.frame_diffs = [1].into_iter().collect();
        base.descriptor.frame_dependencies.chain_diffs = [1, 2, 2].into_iter().collect();
        base.active_targets = 2;
        assert!(matches!(
            continuity.prepare(&frame(5), Some(&base)),
            Err(DescriptorError::MissingReference)
        ));
    }

    #[test]
    fn temporal_shedding_remaps_references_without_flattening_source_semantics() {
        let mut source = Source::new();
        let mut continuity = DescriptorContinuity::default();
        let mut reader = DependencyDescriptorReader::new();
        let key = source.next(true);
        assert_eq!(emit(&mut continuity, &mut reader, &key).frame_number, 0);
        source.next(false);
        let middle = source.next(false);
        let descriptor = emit(&mut continuity, &mut reader, &middle);
        assert_eq!(descriptor.frame_number, 1);
        assert_eq!(descriptor.frame_dependencies.frame_diffs.as_slice(), &[1]);
        assert_eq!(descriptor.frame_dependencies.chain_diffs.as_slice(), &[1]);
        assert_eq!(
            descriptor.frame_dependencies.dtis,
            middle.descriptor.frame_dependencies.dtis
        );
        assert_eq!(descriptor.temporal_id(), 1);
        assert_eq!(descriptor.spatial_id(), 0);
        assert_eq!(
            descriptor.resolution,
            Some(RenderResolution {
                width: 1280,
                height: 720
            })
        );
        assert_eq!(reader.active_decode_targets(), 7);
        source.next(false);
        let base = source.next(false);
        let descriptor = emit(&mut continuity, &mut reader, &base);
        assert_eq!(descriptor.frame_number, 2);
        assert_eq!(descriptor.frame_dependencies.frame_diffs.as_slice(), &[2]);
        assert_eq!(descriptor.frame_dependencies.chain_diffs.as_slice(), &[2]);
        assert_eq!(
            descriptor.frame_dependencies.dtis,
            base.descriptor.frame_dependencies.dtis
        );
        assert_eq!(reader.structure(), Some(source.structure.get()));
    }

    #[test]
    fn prepare_is_transactional_and_source_reset_keeps_outbound_numbering() {
        let mut source = Source::new();
        let key = source.next(true);
        let mut continuity = DescriptorContinuity::default();
        let first = continuity.prepare(&frame(0), Some(&key)).unwrap();
        let retry = continuity.prepare(&frame(0), Some(&key)).unwrap();
        assert_eq!(first.bytes, retry.bytes);
        assert_eq!(continuity.next_frame, 0);
        assert!(continuity.structure.is_none());
        continuity.commit(retry);
        continuity.reset_source();
        let fresh = source.next(true);
        let mut reader = DependencyDescriptorReader::new();
        assert_eq!(emit(&mut continuity, &mut reader, &fresh).frame_number, 1);
        assert_eq!(reader.structure(), Some(source.structure.get()));
    }

    #[test]
    fn missing_required_or_chain_predecessors_never_become_independent() {
        let mut source = Source::new();
        let mut continuity = DescriptorContinuity::default();
        let mut reader = DependencyDescriptorReader::new();
        emit(&mut continuity, &mut reader, &source.next(true));
        source.next(false);
        source.next(false);
        let dependent = source.next(false);
        assert!(matches!(
            continuity.prepare(&frame(3), Some(&dependent)),
            Err(DescriptorError::MissingReference)
        ));
        assert_eq!(continuity.next_frame, 1);
        let mut chain_only = dependent;
        chain_only.descriptor.frame_dependencies.frame_diffs.clear();
        assert!(matches!(
            continuity.prepare(&frame(3), Some(&chain_only)),
            Err(DescriptorError::MissingReference)
        ));
        assert_eq!(continuity.next_frame, 1);
    }

    #[test]
    fn absent_source_descriptor_stays_absent_even_with_generic_independent_metadata() {
        let mut continuity = DescriptorContinuity::default();
        let mut metadata = frame(0);
        metadata.dependencies = crate::FrameDependencies::known(Vec::new()).unwrap();
        let prepared = continuity.prepare(&metadata, None).unwrap();
        assert!(prepared.bytes.is_none());
        continuity.commit(prepared);
        assert_eq!(continuity.next_frame, 1);
        assert!(continuity.source.is_none());
    }

    #[test]
    fn source_frame_number_wrap_and_fragments_share_one_output_identity() {
        let mut source = Source::new();
        let mut key = source.next(true);
        key.descriptor.frame_number = u16::MAX;
        key.descriptor.start_of_frame = true;
        key.descriptor.end_of_frame = false;
        let mut metadata = frame(7);
        metadata.boundary = FrameBoundary::Start;
        let mut continuity = DescriptorContinuity::default();
        let mut reader = DependencyDescriptorReader::new();
        let first = continuity.prepare(&metadata, Some(&key)).unwrap();
        assert_eq!(
            reader
                .read(first.bytes.as_ref().unwrap())
                .unwrap()
                .frame_number,
            0
        );
        continuity.commit(first);
        key.descriptor.start_of_frame = false;
        key.descriptor.end_of_frame = true;
        key.source_attached_structure = false;
        metadata.boundary = FrameBoundary::End;
        let last = continuity.prepare(&metadata, Some(&key)).unwrap();
        assert_eq!(
            reader
                .read(last.bytes.as_ref().unwrap())
                .unwrap()
                .frame_number,
            0
        );
        continuity.commit(last);
        assert_eq!(continuity.next_frame, 1);
        let mut delta = source.next(false);
        delta.descriptor.frame_number = 0;
        let descriptor = emit(&mut continuity, &mut reader, &delta);
        assert_eq!(descriptor.frame_number, 1);
        assert_eq!(descriptor.frame_dependencies.frame_diffs.as_slice(), &[1]);
    }
}
