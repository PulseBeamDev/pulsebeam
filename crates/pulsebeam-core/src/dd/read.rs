use arrayvec::ArrayVec;
use thiserror::Error;

use super::bits::{BitReader, Truncated};
use super::model::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DdReadError {
    #[error("truncated")]
    Truncated,
    #[error("too long: {0} bytes")]
    TooLong(usize),
    #[error("no template structure received yet")]
    NoStructure,
    #[error("unknown template id {0}")]
    UnknownTemplateId(u8),
    #[error("too many templates")]
    TooManyTemplates,
    #[error("too many frame diffs")]
    TooManyFrameDiffs,
    #[error("too many spatial layers")]
    TooManySpatialLayers,
    #[error("too many temporal layers")]
    TooManyTemporalLayers,
    #[error("{0} trailing bytes")]
    TrailingBytes(usize),
}

impl From<Truncated> for DdReadError {
    fn from(_: Truncated) -> Self {
        Self::Truncated
    }
}

pub fn read_mandatory(buf: &[u8]) -> Result<MandatoryFields, DdReadError> {
    const _: () = assert!(
        MANDATORY_LEN == 3,
        "the slice pattern below reads three bytes"
    );

    let [flags, frame_hi, frame_lo, ..] = *buf else {
        return Err(DdReadError::Truncated);
    };

    Ok(MandatoryFields {
        start_of_frame: flags & 0b1000_0000 != 0,
        end_of_frame: flags & 0b0100_0000 != 0,
        template_id: flags & 0b0011_1111,
        frame_number: u16::from_be_bytes([frame_hi, frame_lo]),
    })
}

#[derive(Debug, Default, Clone)]
pub struct DependencyDescriptorReader {
    structure: Option<Box<TemplateDependencyStructure>>,
    active_decode_targets: u32,
}

impl DependencyDescriptorReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn structure(&self) -> Option<&TemplateDependencyStructure> {
        self.structure.as_deref()
    }

    pub fn active_decode_targets(&self) -> u32 {
        self.active_decode_targets
    }

    pub fn reset(&mut self) {
        self.structure = None;
        self.active_decode_targets = 0;
    }

    pub fn read(&mut self, buf: &[u8]) -> Result<DependencyDescriptor, DdReadError> {
        if buf.len() > MAX_DD_LEN {
            return Err(DdReadError::TooLong(buf.len()));
        }
        let mandatory = read_mandatory(buf)?;

        let mut r = BitReader::with_offset(buf, MANDATORY_LEN * 8);
        let mut fields = DdFields::empty();
        let mut structure_present = false;
        let mut targets_present = false;

        if buf.len() > MANDATORY_LEN {
            structure_present = r.read_bit()? != 0;
            targets_present = r.read_bit()? != 0;
            fields.set_custom_dtis(r.read_bit()? != 0);
            fields.set_custom_fdiffs(r.read_bit()? != 0);
            fields.set_custom_chains(r.read_bit()? != 0);
        }

        let attached = if structure_present {
            Some(Box::new(read_structure(&mut r)?))
        } else {
            None
        };

        let active = match attached.as_deref() {
            Some(s) => s,
            None => self.structure.as_deref().ok_or(DdReadError::NoStructure)?,
        };

        let active_decode_targets = if targets_present {
            Some(r.read_bits(u32::from(active.decode_target_count))?)
        } else {
            None
        };

        let frame_dependencies = read_frame_dependencies(&mut r, active, &mandatory, fields)?;
        let resolution = active
            .resolutions
            .get(usize::from(frame_dependencies.spatial_id))
            .copied();

        let trailing = r.remaining() / 8;
        if trailing > 0 {
            return Err(DdReadError::TrailingBytes(trailing));
        }

        if let Some(s) = attached.as_deref() {
            self.active_decode_targets = s.all_decode_targets_bitmask();
            self.structure = Some(Box::new(s.clone()));
        }
        if let Some(mask) = active_decode_targets {
            self.active_decode_targets = mask;
        }

        Ok(DependencyDescriptor {
            start_of_frame: mandatory.start_of_frame,
            end_of_frame: mandatory.end_of_frame,
            template_id: mandatory.template_id,
            frame_number: mandatory.frame_number,
            attached_structure: attached,
            active_decode_targets,
            fields,
            frame_dependencies,
            resolution,
        })
    }
}

fn read_frame_dependencies(
    r: &mut BitReader,
    active: &TemplateDependencyStructure,
    mandatory: &MandatoryFields,
    fields: DdFields,
) -> Result<FrameDependencyTemplate, DdReadError> {
    let index = active.template_index(mandatory.template_id);
    let mut deps = active
        .templates
        .get(index)
        .cloned()
        .ok_or(DdReadError::UnknownTemplateId(mandatory.template_id))?;

    if fields.custom_dtis() {
        for dti in &mut deps.dtis {
            *dti = DecodeTargetIndication::from_bits(r.read_bits(2)?);
        }
    }

    if fields.custom_fdiffs() {
        deps.frame_diffs.clear();
        let mut size = r.read_bits(2)?;
        while size > 0 {
            let fdiff = r
                .read_bits_u16(4u32.saturating_mul(size))?
                .saturating_add(1);
            deps.frame_diffs
                .try_push(fdiff)
                .map_err(|_| DdReadError::TooManyFrameDiffs)?;
            size = r.read_bits(2)?;
        }
    }

    if fields.custom_chains() {
        for chain_diff in &mut deps.chain_diffs {
            *chain_diff = r.read_bits_u8(8)?;
        }
    }

    Ok(deps)
}

fn read_structure(r: &mut BitReader) -> Result<TemplateDependencyStructure, DdReadError> {
    let template_id_offset = r.read_bits_u8(6)?;
    let decode_target_count = r.read_bits_u8(5)?.saturating_add(1);
    debug_assert!(decode_target_count as usize <= MAX_DECODE_TARGETS);

    let mut s = TemplateDependencyStructure {
        template_id_offset,
        decode_target_count,
        ..Default::default()
    };

    read_template_layers(r, &mut s)?;
    read_template_dtis(r, &mut s)?;
    read_template_fdiffs(r, &mut s)?;
    read_template_chains(r, &mut s)?;

    if r.read_bit()? != 0 {
        read_resolutions(r, &mut s)?;
    }

    Ok(s)
}

fn read_template_layers(
    r: &mut BitReader,
    s: &mut TemplateDependencyStructure,
) -> Result<(), DdReadError> {
    const SAME_LAYER: u32 = 0;
    const NEXT_TEMPORAL: u32 = 1;
    const NEXT_SPATIAL: u32 = 2;
    const NO_MORE: u32 = 3;

    let mut spatial_id = 0u8;
    let mut temporal_id = 0u8;

    loop {
        s.templates
            .try_push(FrameDependencyTemplate {
                spatial_id,
                temporal_id,
                ..Default::default()
            })
            .map_err(|_| DdReadError::TooManyTemplates)?;

        let next_layer_idc = r.read_bits(2)?;
        match next_layer_idc {
            SAME_LAYER => {}
            NEXT_TEMPORAL => {
                temporal_id = temporal_id.saturating_add(1);
                if usize::from(temporal_id) >= MAX_TEMPORAL_IDS {
                    return Err(DdReadError::TooManyTemporalLayers);
                }
            }
            NEXT_SPATIAL => {
                temporal_id = 0;
                spatial_id = spatial_id.saturating_add(1);
                if usize::from(spatial_id) >= MAX_SPATIAL_IDS {
                    return Err(DdReadError::TooManySpatialLayers);
                }
            }
            _ => {
                debug_assert_eq!(
                    next_layer_idc, NO_MORE,
                    "read_bits(2) yields 0..4 and the other three are matched above"
                );
                break;
            }
        }
    }

    debug_assert!(!s.templates.is_empty());
    Ok(())
}

fn read_template_dtis(
    r: &mut BitReader,
    s: &mut TemplateDependencyStructure,
) -> Result<(), DdReadError> {
    let dt_cnt = usize::from(s.decode_target_count);
    for t in &mut s.templates {
        for _ in 0..dt_cnt {
            let dti = DecodeTargetIndication::from_bits(r.read_bits(2)?);
            debug_assert!(t.dtis.len() < MAX_DECODE_TARGETS);
            t.dtis.push(dti);
        }
    }
    Ok(())
}

fn read_template_fdiffs(
    r: &mut BitReader,
    s: &mut TemplateDependencyStructure,
) -> Result<(), DdReadError> {
    for t in &mut s.templates {
        while r.read_bit()? != 0 {
            let fdiff = r.read_bits_u16(4)?.saturating_add(1);
            t.frame_diffs
                .try_push(fdiff)
                .map_err(|_| DdReadError::TooManyFrameDiffs)?;
        }
    }
    Ok(())
}

fn read_template_chains(
    r: &mut BitReader,
    s: &mut TemplateDependencyStructure,
) -> Result<(), DdReadError> {
    let dt_cnt = u32::from(s.decode_target_count);
    let chain_count = r.read_ns_u8(dt_cnt.saturating_add(1))?;
    debug_assert!(u32::from(chain_count) <= dt_cnt);
    s.chain_count = chain_count;
    if chain_count == 0 {
        return Ok(());
    }

    for _ in 0..dt_cnt {
        let protected_by = r.read_ns_u8(u32::from(chain_count))?;
        debug_assert!(s.decode_target_protected_by.len() < MAX_DECODE_TARGETS);
        s.decode_target_protected_by.push(protected_by);
    }

    for t in &mut s.templates {
        for _ in 0..chain_count {
            let diff = r.read_bits_u8(4)?;
            debug_assert!(t.chain_diffs.len() < MAX_CHAINS);
            t.chain_diffs.push(diff);
        }
    }

    Ok(())
}

fn read_resolutions(
    r: &mut BitReader,
    s: &mut TemplateDependencyStructure,
) -> Result<(), DdReadError> {
    let spatial_layers = s.spatial_layer_count();
    if spatial_layers > MAX_SPATIAL_IDS {
        return Err(DdReadError::TooManySpatialLayers);
    }

    let mut resolutions = ArrayVec::new();
    for _ in 0..spatial_layers {
        // `x_minus_1` is 16 bits, so the coded maximum is 65536 — one more
        // than the field it lands in. `+ 1` then `as u16` turned the largest
        // representable resolution into zero; clamping keeps it the largest.
        let width = read_dimension(r)?;
        let height = read_dimension(r)?;
        resolutions.push(RenderResolution { width, height });
    }
    s.resolutions = resolutions;

    Ok(())
}

/// Decode a `*_minus_1` render dimension, clamping the one coded value that
/// does not fit the field it lands in.
fn read_dimension(r: &mut BitReader<'_>) -> Result<u16, DdReadError> {
    let coded = r.read_bits(16)?.saturating_add(1);
    Ok(u16::try_from(coded).unwrap_or(u16::MAX))
}
