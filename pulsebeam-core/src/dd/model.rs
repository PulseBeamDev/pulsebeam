use arrayvec::ArrayVec;

pub const MAX_TEMPLATES: usize = 64;
pub const MAX_DECODE_TARGETS: usize = 32;
pub const MAX_CHAINS: usize = 32;
pub const MAX_SPATIAL_IDS: usize = 4;
pub const MAX_TEMPORAL_IDS: usize = 8;
pub const MAX_FRAME_DIFFS: usize = 8;

/// Descriptors always negotiate the two-byte extension form, whose length field
/// is a single byte.
pub const MAX_DD_LEN: usize = 255;

pub const MANDATORY_LEN: usize = 3;

const TEMPLATE_ID_MODULUS: u16 = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
#[repr(u8)]
pub enum DecodeTargetIndication {
    #[default]
    NotPresent = 0,
    Discardable = 1,
    Switch = 2,
    Required = 3,
}

impl DecodeTargetIndication {
    pub const fn from_bits(v: u32) -> Self {
        match v & 0b11 {
            0 => Self::NotPresent,
            1 => Self::Discardable,
            2 => Self::Switch,
            _ => Self::Required,
        }
    }

    pub const fn bits(self) -> u32 {
        self as u32
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct FrameDependencyTemplate {
    pub spatial_id: u8,
    pub temporal_id: u8,
    pub dtis: ArrayVec<DecodeTargetIndication, MAX_DECODE_TARGETS>,
    pub frame_diffs: ArrayVec<u16, MAX_FRAME_DIFFS>,
    pub chain_diffs: ArrayVec<u8, MAX_CHAINS>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct RenderResolution {
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TemplateDependencyStructure {
    pub template_id_offset: u8,
    pub decode_target_count: u8,
    pub chain_count: u8,
    pub decode_target_protected_by: ArrayVec<u8, MAX_DECODE_TARGETS>,
    pub templates: ArrayVec<FrameDependencyTemplate, MAX_TEMPLATES>,
    pub resolutions: ArrayVec<RenderResolution, MAX_SPATIAL_IDS>,
}

impl TemplateDependencyStructure {
    pub fn template_index(&self, wire_id: u8) -> usize {
        let offset = u16::from(self.template_id_offset);
        // Modular by intent: template ids wrap within the modulus, so the
        // addition before the subtraction is what keeps it non-negative.
        usize::from(
            u16::from(wire_id)
                .wrapping_add(TEMPLATE_ID_MODULUS)
                .wrapping_sub(offset)
                % TEMPLATE_ID_MODULUS,
        )
    }

    pub fn template(&self, wire_id: u8) -> Option<&FrameDependencyTemplate> {
        self.templates.get(self.template_index(wire_id))
    }

    pub fn all_decode_targets_bitmask(&self) -> u32 {
        debug_assert!(self.decode_target_count as usize <= MAX_DECODE_TARGETS);
        match self.decode_target_count {
            0 => 0,
            n if n as usize >= MAX_DECODE_TARGETS => u32::MAX,
            n => (1u32 << n).saturating_sub(1),
        }
    }

    pub fn spatial_layer_count(&self) -> usize {
        self.templates
            .iter()
            .map(|t| usize::from(t.spatial_id).saturating_add(1))
            .max()
            .unwrap_or(0)
    }
}

const FIELD_CUSTOM_DTIS: u8 = 1 << 0;
const FIELD_CUSTOM_FDIFFS: u8 = 1 << 1;
const FIELD_CUSTOM_CHAINS: u8 = 1 << 2;

/// Which `frame_dependency_definition()` fields the sender coded explicitly
/// rather than inheriting from the template. Retained so a descriptor can be
/// re-encoded with the sender's original byte layout.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Hash)]
pub struct DdFields(u8);

impl DdFields {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn custom_dtis(self) -> bool {
        self.0 & FIELD_CUSTOM_DTIS != 0
    }

    pub const fn custom_fdiffs(self) -> bool {
        self.0 & FIELD_CUSTOM_FDIFFS != 0
    }

    pub const fn custom_chains(self) -> bool {
        self.0 & FIELD_CUSTOM_CHAINS != 0
    }

    pub fn set_custom_dtis(&mut self, v: bool) {
        self.set(FIELD_CUSTOM_DTIS, v);
    }

    pub fn set_custom_fdiffs(&mut self, v: bool) {
        self.set(FIELD_CUSTOM_FDIFFS, v);
    }

    pub fn set_custom_chains(&mut self, v: bool) {
        self.set(FIELD_CUSTOM_CHAINS, v);
    }

    fn set(&mut self, flag: u8, v: bool) {
        if v {
            self.0 |= flag;
        } else {
            self.0 &= !flag;
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct MandatoryFields {
    pub start_of_frame: bool,
    pub end_of_frame: bool,
    pub template_id: u8,
    pub frame_number: u16,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DependencyDescriptor {
    pub start_of_frame: bool,
    pub end_of_frame: bool,
    /// Raw 6-bit wire value, before `template_id_offset` is applied.
    pub template_id: u8,
    pub frame_number: u16,
    pub attached_structure: Option<Box<TemplateDependencyStructure>>,
    /// `Some` only when the sender put a bitmask on the wire; an SFU that drops
    /// layers must be able to tell that apart from "unchanged".
    pub active_decode_targets: Option<u32>,
    pub fields: DdFields,
    /// The referenced template with any custom overrides already applied.
    pub frame_dependencies: FrameDependencyTemplate,
    /// This frame's spatial layer resolution, when the structure declared any.
    pub resolution: Option<RenderResolution>,
}

impl DependencyDescriptor {
    pub fn mandatory(&self) -> MandatoryFields {
        MandatoryFields {
            start_of_frame: self.start_of_frame,
            end_of_frame: self.end_of_frame,
            template_id: self.template_id,
            frame_number: self.frame_number,
        }
    }

    pub fn spatial_id(&self) -> u8 {
        self.frame_dependencies.spatial_id
    }

    pub fn temporal_id(&self) -> u8 {
        self.frame_dependencies.temporal_id
    }

    /// Whether this frame contributes to `decode_target`.
    pub fn is_in_decode_target(&self, decode_target: usize) -> bool {
        self.frame_dependencies
            .dtis
            .get(decode_target)
            .is_some_and(|d| *d != DecodeTargetIndication::NotPresent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_index_wraps_around_the_id_modulus() {
        let mut s = TemplateDependencyStructure {
            template_id_offset: 60,
            ..Default::default()
        };
        for _ in 0..8 {
            s.templates.push(FrameDependencyTemplate::default());
        }

        assert_eq!(s.template_index(60), 0);
        assert_eq!(s.template_index(63), 3);
        assert_eq!(s.template_index(0), 4);
        assert_eq!(s.template_index(3), 7);
    }

    #[test]
    fn all_decode_targets_bitmask_saturates_at_capacity() {
        let mut s = TemplateDependencyStructure::default();
        assert_eq!(s.all_decode_targets_bitmask(), 0);

        s.decode_target_count = 5;
        assert_eq!(s.all_decode_targets_bitmask(), 0b11111);

        s.decode_target_count =
            u8::try_from(MAX_DECODE_TARGETS).expect("MAX_DECODE_TARGETS fits a u8");
        assert_eq!(s.all_decode_targets_bitmask(), u32::MAX);
    }

    #[test]
    fn dti_bits_roundtrip() {
        for v in 0..4u32 {
            assert_eq!(DecodeTargetIndication::from_bits(v).bits(), v);
        }
    }
}
