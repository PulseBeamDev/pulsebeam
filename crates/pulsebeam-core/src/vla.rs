//! Transport-independent Video Layers Allocation RTP extension decoding.

use arrayvec::ArrayVec;

pub const URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00";
pub const MAX_VLA_LEN: usize = 255;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoLayersAllocation {
    pub current_simulcast_stream_index: u8,
    pub simulcast_streams: Vec<SimulcastStreamAllocation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulcastStreamAllocation {
    pub spatial_layers: Vec<SpatialLayerAllocation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpatialLayerAllocation {
    /// Empty for an inactive spatial position.
    pub temporal_layers: Vec<TemporalLayerAllocation>,
    pub resolution_and_framerate: Option<ResolutionAndFramerate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemporalLayerAllocation {
    /// Includes all lower temporal layers within this spatial layer.
    pub cumulative_kbps: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionAndFramerate {
    /// The wire carries dimension minus one, allowing 1 through 65536 pixels.
    pub width: u32,
    pub height: u32,
    pub framerate: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VlaReadError {
    #[error("video layers allocation exceeds {MAX_VLA_LEN} bytes")]
    TooLong,
    #[error("truncated video layers allocation")]
    Truncated,
    #[error("invalid video layers allocation")]
    Invalid,
    #[error("video layers allocation has trailing bytes")]
    TrailingBytes,
}

impl VideoLayersAllocation {
    /// Decodes at most four streams, four spatial positions per stream, and
    /// four cumulative temporal rates per active spatial position.
    pub fn parse(bytes: &[u8]) -> Result<Self, VlaReadError> {
        let mut simulcast_streams = Vec::new();
        let current_simulcast_stream_index = read_allocation(bytes, Some(&mut simulcast_streams))?;
        Ok(Self {
            current_simulcast_stream_index,
            simulcast_streams,
        })
    }
}

/// Validates wire syntax without building a decoded topology.
pub(crate) fn validate(bytes: &[u8]) -> Result<(), VlaReadError> {
    read_allocation(bytes, None).map(|_| ())
}

fn read_allocation(
    bytes: &[u8],
    mut streams: Option<&mut Vec<SimulcastStreamAllocation>>,
) -> Result<u8, VlaReadError> {
    if bytes.len() > MAX_VLA_LEN {
        return Err(VlaReadError::TooLong);
    }
    let mut reader = Reader(bytes);
    let header = reader.byte()?;
    if header == 0 && reader.0.is_empty() {
        return Ok(0);
    }
    let current_simulcast_stream_index = header >> 6;
    let stream_count = ((header >> 4) & 3).saturating_add(1);
    if current_simulcast_stream_index >= stream_count {
        return Err(VlaReadError::Invalid);
    }
    let stream_count = usize::from(stream_count);
    let shared_mask = header & 15;
    let masks: ArrayVec<u8, 4> = if shared_mask != 0 {
        std::iter::repeat_n(shared_mask, stream_count).collect()
    } else {
        let packed = reader.take(stream_count.div_ceil(2))?;
        if !stream_count.is_multiple_of(2) && packed.last().is_some_and(|byte| byte & 15 != 0) {
            return Err(VlaReadError::Invalid);
        }
        packed
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 15])
            .take(stream_count)
            .collect()
    };
    let active_count = masks.iter().fold(0_usize, |count, mask| {
        count.saturating_add(mask.count_ones() as usize)
    });
    let counts = reader.take(active_count.div_ceil(4))?;
    let unused_count_bits = match active_count % 4 {
        0 => 0,
        1 => 6,
        2 => 4,
        _ => 2,
    };
    if counts
        .last()
        .is_some_and(|byte| byte.trailing_zeros() < unused_count_bits)
    {
        return Err(VlaReadError::Invalid);
    }
    let mut temporal_counts = counts
        .iter()
        .flat_map(|byte| [byte >> 6, byte >> 4, byte >> 2, *byte])
        .map(|bits| (bits & 3).saturating_add(1));
    for mask in masks {
        let spatial_count = u8::BITS.saturating_sub(mask.leading_zeros());
        let mut spatial_layers = Vec::new();
        for spatial in 0..spatial_count {
            let active = mask & 1u8.checked_shl(spatial).ok_or(VlaReadError::Invalid)? != 0;
            let mut temporal_layers = Vec::new();
            if active {
                let count = temporal_counts.next().ok_or(VlaReadError::Truncated)?;
                for _ in 0..count {
                    let cumulative_kbps = reader.rate()?;
                    if streams.is_some() {
                        temporal_layers.push(TemporalLayerAllocation { cumulative_kbps });
                    }
                }
            }
            if streams.is_some() {
                spatial_layers.push(SpatialLayerAllocation {
                    temporal_layers,
                    resolution_and_framerate: None,
                });
            }
        }
        if let Some(streams) = streams.as_deref_mut() {
            streams.push(SimulcastStreamAllocation { spatial_layers });
        }
    }
    if !reader.0.is_empty() {
        let geometry = reader.take(active_count.saturating_mul(5))?;
        if !reader.0.is_empty() {
            return Err(VlaReadError::TrailingBytes);
        }
        if let Some(streams) = streams {
            let mut geometry = geometry.as_chunks::<5>().0.iter();
            for spatial in streams
                .iter_mut()
                .flat_map(|stream| &mut stream.spatial_layers)
            {
                if spatial.temporal_layers.is_empty() {
                    continue;
                }
                let Some(&[width_hi, width_lo, height_hi, height_lo, framerate]) = geometry.next()
                else {
                    return Err(VlaReadError::Truncated);
                };
                spatial.resolution_and_framerate = Some(ResolutionAndFramerate {
                    width: u32::from(u16::from_be_bytes([width_hi, width_lo])).saturating_add(1),
                    height: u32::from(u16::from_be_bytes([height_hi, height_lo])).saturating_add(1),
                    framerate,
                });
            }
        }
    }
    Ok(current_simulcast_stream_index)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, VlaReadError> {
        let (&byte, rest) = self.0.split_first().ok_or(VlaReadError::Truncated)?;
        self.0 = rest;
        Ok(byte)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], VlaReadError> {
        let (bytes, rest) = self
            .0
            .split_at_checked(length)
            .ok_or(VlaReadError::Truncated)?;
        self.0 = rest;
        Ok(bytes)
    }

    fn rate(&mut self) -> Result<u64, VlaReadError> {
        let mut value = 0;
        // allocation00 uses the AV1 leb128 syntax: up to eight bytes, u32 range.
        for shift in (0..56).step_by(7) {
            let byte = self.byte()?;
            value |= u64::from(byte & 127)
                .checked_shl(shift)
                .ok_or(VlaReadError::Invalid)?;
            if byte & 128 == 0 {
                return u32::try_from(value)
                    .map(u64::from)
                    .map_err(|_| VlaReadError::Invalid);
            }
        }
        Err(VlaReadError::Invalid)
    }
}

#[cfg(test)]
mod tests;
