//! Encoder for the AV1 Dependency Descriptor.
//!
//! Overflow is explicit here: `#![deny(clippy::arithmetic_side_effects)]`.
//! Several fields are coded as `value - 1`, which has no encoding for zero; a
//! wrap would write 0xFFFF.. and the peer would decode a different descriptor
//! than the one that was built.

use thiserror::Error;

use super::bits::{BitWriter, Overflow};
use super::model::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DdWriteError {
    #[error("buffer too small")]
    Overflow,
    #[error("no template structure")]
    NoStructure,
    #[error("template id {0} not in structure")]
    UnknownTemplateId(u8),
    #[error("descriptor inconsistent with structure")]
    Inconsistent,
    #[error("value out of range")]
    OutOfRange,
}

impl From<Overflow> for DdWriteError {
    fn from(_: Overflow) -> Self {
        Self::Overflow
    }
}

const NEXT_LAYER_SAME: u32 = 0;
const NEXT_LAYER_TEMPORAL: u32 = 1;
const NEXT_LAYER_SPATIAL: u32 = 2;
const NEXT_LAYER_NO_MORE: u32 = 3;

#[derive(Debug, Default, Clone)]
pub struct DependencyDescriptorWriter {
    structure: Option<Box<TemplateDependencyStructure>>,
}

impl DependencyDescriptorWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn structure(&self) -> Option<&TemplateDependencyStructure> {
        self.structure.as_deref()
    }

    pub fn reset(&mut self) {
        self.structure = None;
    }

    pub fn write(
        &mut self,
        dd: &DependencyDescriptor,
        out: &mut [u8],
    ) -> Result<usize, DdWriteError> {
        let active = match dd.attached_structure.as_deref() {
            Some(s) => s,
            None => self.structure.as_deref().ok_or(DdWriteError::NoStructure)?,
        };

        validate(dd, active)?;

        let mut w = BitWriter::new(out);
        w.write_bit(dd.start_of_frame)?;
        w.write_bit(dd.end_of_frame)?;
        w.write_bits(u32::from(dd.template_id), 6)?;
        w.write_bits(u32::from(dd.frame_number), 16)?;

        let has_extended = dd.attached_structure.is_some()
            || dd.active_decode_targets.is_some()
            || !dd.fields.is_empty();

        if has_extended {
            w.write_bit(dd.attached_structure.is_some())?;
            w.write_bit(dd.active_decode_targets.is_some())?;
            w.write_bit(dd.fields.custom_dtis())?;
            w.write_bit(dd.fields.custom_fdiffs())?;
            w.write_bit(dd.fields.custom_chains())?;

            if let Some(s) = dd.attached_structure.as_deref() {
                write_structure(&mut w, s)?;
            }
            if let Some(mask) = dd.active_decode_targets {
                w.write_bits(mask, u32::from(active.decode_target_count))?;
            }
            write_frame_dependencies(&mut w, dd)?;
        }

        let len = w.finish();
        debug_assert!(len >= MANDATORY_LEN);
        debug_assert!(len <= MAX_DD_LEN);

        if let Some(s) = dd.attached_structure.as_deref() {
            self.structure = Some(Box::new(s.clone()));
        }

        Ok(len)
    }
}

fn validate(
    dd: &DependencyDescriptor,
    active: &TemplateDependencyStructure,
) -> Result<(), DdWriteError> {
    if active.template(dd.template_id).is_none() {
        return Err(DdWriteError::UnknownTemplateId(dd.template_id));
    }
    if dd.frame_dependencies.dtis.len() != usize::from(active.decode_target_count) {
        return Err(DdWriteError::Inconsistent);
    }
    if dd.frame_dependencies.chain_diffs.len() != usize::from(active.chain_count) {
        return Err(DdWriteError::Inconsistent);
    }
    if let Some(mask) = dd.active_decode_targets {
        let all = active.all_decode_targets_bitmask();
        if mask & !all != 0 {
            return Err(DdWriteError::OutOfRange);
        }
    }
    Ok(())
}

fn write_frame_dependencies(
    w: &mut BitWriter,
    dd: &DependencyDescriptor,
) -> Result<(), DdWriteError> {
    if dd.fields.custom_dtis() {
        for dti in &dd.frame_dependencies.dtis {
            w.write_bits(dti.bits(), 2)?;
        }
    }

    if dd.fields.custom_fdiffs() {
        for &fdiff in &dd.frame_dependencies.frame_diffs {
            if fdiff == 0 || u32::from(fdiff) > 1 << 12 {
                return Err(DdWriteError::OutOfRange);
            }
            // Coded as `minus_1`, so zero has no encoding. Wrapping would write
            // 0xFFFF..; clamping writes the smallest legal value instead, and the
            // assert catches the caller in sim.
            debug_assert!(fdiff > 0, "frame diff of 0 has no minus-1 encoding");
            let value = u32::from(fdiff).saturating_sub(1);
            let size = if u32::from(fdiff) <= 1 << 4 {
                1
            } else if u32::from(fdiff) <= 1 << 8 {
                2
            } else {
                3
            };
            w.write_bits(size, 2)?;
            w.write_bits(value, 4u32.saturating_mul(size))?;
        }
        w.write_bits(0, 2)?;
    }

    if dd.fields.custom_chains() {
        for &chain_diff in &dd.frame_dependencies.chain_diffs {
            w.write_bits(u32::from(chain_diff), 8)?;
        }
    }

    Ok(())
}

fn write_structure(w: &mut BitWriter, s: &TemplateDependencyStructure) -> Result<(), DdWriteError> {
    if s.templates.is_empty()
        || s.decode_target_count == 0
        || usize::from(s.decode_target_count) > MAX_DECODE_TARGETS
        || s.chain_count > s.decode_target_count
    {
        return Err(DdWriteError::Inconsistent);
    }
    if s.templates[0].spatial_id != 0 || s.templates[0].temporal_id != 0 {
        return Err(DdWriteError::Inconsistent);
    }

    w.write_bits(u32::from(s.template_id_offset), 6)?;
    debug_assert!(
        s.decode_target_count > 0,
        "a descriptor with no decode targets has no minus-1 encoding"
    );
    w.write_bits(u32::from(s.decode_target_count).saturating_sub(1), 5)?;

    write_template_layers(w, s)?;
    write_template_dtis(w, s)?;
    write_template_fdiffs(w, s)?;
    write_template_chains(w, s)?;

    w.write_bit(!s.resolutions.is_empty())?;
    if !s.resolutions.is_empty() {
        write_resolutions(w, s)?;
    }

    Ok(())
}

fn write_template_layers(
    w: &mut BitWriter,
    s: &TemplateDependencyStructure,
) -> Result<(), DdWriteError> {
    for pair in s.templates.windows(2) {
        let (prev, next) = (&pair[0], &pair[1]);
        let idc = if next.spatial_id == prev.spatial_id && next.temporal_id == prev.temporal_id {
            NEXT_LAYER_SAME
        } else if next.spatial_id == prev.spatial_id
            && next.temporal_id == prev.temporal_id.saturating_add(1)
        {
            NEXT_LAYER_TEMPORAL
        } else if next.spatial_id == prev.spatial_id.saturating_add(1) && next.temporal_id == 0 {
            NEXT_LAYER_SPATIAL
        } else {
            debug_assert!(false, "templates not expressible as next_layer_idc");
            return Err(DdWriteError::Inconsistent);
        };
        w.write_bits(idc, 2)?;
    }
    w.write_bits(NEXT_LAYER_NO_MORE, 2)?;
    Ok(())
}

fn write_template_dtis(
    w: &mut BitWriter,
    s: &TemplateDependencyStructure,
) -> Result<(), DdWriteError> {
    for t in &s.templates {
        if t.dtis.len() != usize::from(s.decode_target_count) {
            return Err(DdWriteError::Inconsistent);
        }
        for dti in &t.dtis {
            w.write_bits(dti.bits(), 2)?;
        }
    }
    Ok(())
}

fn write_template_fdiffs(
    w: &mut BitWriter,
    s: &TemplateDependencyStructure,
) -> Result<(), DdWriteError> {
    for t in &s.templates {
        for &fdiff in &t.frame_diffs {
            if fdiff == 0 || u32::from(fdiff) > 1 << 4 {
                return Err(DdWriteError::OutOfRange);
            }
            w.write_bit(true)?;
            debug_assert!(fdiff > 0, "frame diff of 0 has no minus-1 encoding");
            w.write_bits(u32::from(fdiff).saturating_sub(1), 4)?;
        }
        w.write_bit(false)?;
    }
    Ok(())
}

fn write_template_chains(
    w: &mut BitWriter,
    s: &TemplateDependencyStructure,
) -> Result<(), DdWriteError> {
    let dt_cnt = u32::from(s.decode_target_count);
    w.write_ns(u32::from(s.chain_count), dt_cnt.saturating_add(1))?;
    if s.chain_count == 0 {
        return Ok(());
    }

    if s.decode_target_protected_by.len() != usize::from(s.decode_target_count) {
        return Err(DdWriteError::Inconsistent);
    }
    for &protected_by in &s.decode_target_protected_by {
        if protected_by >= s.chain_count {
            return Err(DdWriteError::OutOfRange);
        }
        w.write_ns(u32::from(protected_by), u32::from(s.chain_count))?;
    }

    for t in &s.templates {
        if t.chain_diffs.len() != usize::from(s.chain_count) {
            return Err(DdWriteError::Inconsistent);
        }
        for &diff in &t.chain_diffs {
            if u32::from(diff) >= 1 << 4 {
                return Err(DdWriteError::OutOfRange);
            }
            w.write_bits(u32::from(diff), 4)?;
        }
    }

    Ok(())
}

fn write_resolutions(
    w: &mut BitWriter,
    s: &TemplateDependencyStructure,
) -> Result<(), DdWriteError> {
    if s.resolutions.len() != s.spatial_layer_count() {
        return Err(DdWriteError::Inconsistent);
    }
    for r in &s.resolutions {
        if r.width == 0 || r.height == 0 {
            return Err(DdWriteError::OutOfRange);
        }
        debug_assert!(
            r.width > 0 && r.height > 0,
            "a zero render dimension has no minus-1 encoding"
        );
        w.write_bits(u32::from(r.width).saturating_sub(1), 16)?;
        w.write_bits(u32::from(r.height).saturating_sub(1), 16)?;
    }
    Ok(())
}
