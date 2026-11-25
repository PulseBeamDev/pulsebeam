//! VP9 RTP payload descriptors and spatial-frame packetization (RFC 9628).

use pulsebeam_core::h264::MAX_ACCESS_UNIT_SIZE;

use super::{Packet, PacketBudget, PacketizeError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PictureId {
    Short(u8),
    Long(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layer {
    pub temporal_id: u8,
    pub switching_up: bool,
    pub spatial_id: u8,
    pub depends_on_lower_spatial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Differences {
    entries: [u8; 3],
    count: u8,
}

impl Differences {
    pub fn new(differences: &[u8]) -> Option<Self> {
        if differences.len() > 3 || differences.contains(&0) {
            return None;
        }
        let mut entries = [0; 3];
        entries
            .get_mut(..differences.len())?
            .copy_from_slice(differences);
        Some(Self {
            entries,
            count: u8::try_from(differences.len()).ok()?,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        self.entries
            .get(..usize::from(self.count))
            .unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum References {
    Flexible(Differences),
    Structured { tl0_picture_index: Option<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PictureGroupEntry {
    pub temporal_id: u8,
    pub switching_up: bool,
    pub references: Differences,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scalability {
    pub spatial_layers: u8,
    pub resolutions: Option<Vec<(u16, u16)>>,
    pub group: Vec<PictureGroupEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub picture_id: Option<PictureId>,
    pub predicted: bool,
    pub layer: Option<Layer>,
    pub references: References,
    pub frame_start: bool,
    pub frame_end: bool,
    pub scalability: Option<Scalability>,
    pub not_reference_for_upper_spatial: bool,
}

impl Descriptor {
    pub fn parse(payload: &[u8]) -> Option<(Self, &[u8])> {
        let mut bytes = payload;
        let flags = take(&mut bytes)?;
        let predicted = flags & 0x40 != 0;
        let flexible = flags & 0x10 != 0;
        let picture_id = if flags & 0x80 != 0 {
            let first = take(&mut bytes)?;
            Some(if first & 0x80 == 0 {
                PictureId::Short(first)
            } else {
                PictureId::Long(u16::from_be_bytes([first & 0x7f, take(&mut bytes)?]))
            })
        } else if flexible {
            return None;
        } else {
            None
        };
        let layer = if flags & 0x20 != 0 {
            let data = take(&mut bytes)?;
            let spatial_id = (data >> 1) & 7;
            let depends_on_lower_spatial = data & 1 != 0;
            if depends_on_lower_spatial && spatial_id == 0 {
                return None;
            }
            Some(Layer {
                temporal_id: data >> 5,
                switching_up: data & 0x10 != 0,
                spatial_id,
                depends_on_lower_spatial,
            })
        } else {
            None
        };
        let references = if flexible {
            let mut differences = Differences {
                entries: [0; 3],
                count: 0,
            };
            if predicted {
                loop {
                    let wire = take(&mut bytes)?;
                    let diff = wire >> 1;
                    if diff == 0 {
                        return None;
                    }
                    *differences
                        .entries
                        .get_mut(usize::from(differences.count))? = diff;
                    differences.count = differences.count.saturating_add(1);
                    if wire & 1 == 0 {
                        break;
                    }
                }
            }
            References::Flexible(differences)
        } else {
            References::Structured {
                tl0_picture_index: if layer.is_some() {
                    Some(take(&mut bytes)?)
                } else {
                    None
                },
            }
        };
        let scalability = if flags & 0x02 != 0 {
            Some(parse_scalability(&mut bytes)?)
        } else {
            None
        };
        (!bytes.is_empty()).then_some((
            Self {
                picture_id,
                predicted,
                layer,
                references,
                frame_start: flags & 0x08 != 0,
                frame_end: flags & 0x04 != 0,
                scalability,
                not_reference_for_upper_spatial: flags & 1 != 0,
            },
            bytes,
        ))
    }
}

/// Packetize one encoded spatial frame. All frames of a picture share the RTP
/// timestamp and picture ID; only its final transmitted frame sets the marker.
pub fn packetize<'a>(
    budget: PacketBudget,
    sequence: u32,
    timestamp: u32,
    end_of_picture: bool,
    descriptor: &Descriptor,
    frame: &'a [u8],
) -> Result<Packets<'a>, PacketizeError> {
    if frame.is_empty() {
        return Err(PacketizeError::Empty);
    }
    if frame.len() > MAX_ACCESS_UNIT_SIZE {
        return Err(PacketizeError::TooLarge);
    }
    let tail = encode_base(descriptor).ok_or(PacketizeError::InvalidFraming)?;
    let mut head = tail.clone();
    if let Some(ss) = &descriptor.scalability {
        encode_scalability(ss, &mut head).ok_or(PacketizeError::InvalidFraming)?;
        *head.first_mut().ok_or(PacketizeError::InvalidFraming)? |= 2;
    }
    if budget.payload_bytes() <= head.len() {
        return Err(PacketizeError::TooLarge);
    }
    Ok(Packets {
        budget,
        sequence,
        timestamp,
        end_of_picture,
        head,
        tail,
        frame,
        offset: 0,
    })
}

pub struct Packets<'a> {
    budget: PacketBudget,
    sequence: u32,
    timestamp: u32,
    end_of_picture: bool,
    head: Vec<u8>,
    tail: Vec<u8>,
    frame: &'a [u8],
    offset: usize,
}

impl Iterator for Packets<'_> {
    type Item = Packet<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset == self.frame.len() {
            return None;
        }
        let first = self.offset == 0;
        let header = if first { &self.head } else { &self.tail };
        let size = self
            .budget
            .payload_bytes()
            .checked_sub(header.len())?
            .min(self.frame.len().checked_sub(self.offset)?);
        let end = self.offset.checked_add(size)?;
        let last = end == self.frame.len();
        let mut payload = Vec::with_capacity(header.len().checked_add(size)?);
        let (&flags, rest) = header.split_first()?;
        payload.push(flags | u8::from(first) << 3 | u8::from(last) << 2);
        payload.extend_from_slice(rest);
        payload.extend_from_slice(self.frame.get(self.offset..end)?);
        self.offset = end;
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        Some(Packet {
            sequence,
            timestamp: self.timestamp,
            marker: last && self.end_of_picture,
            first,
            last,
            payload,
        })
    }
}

fn encode_base(descriptor: &Descriptor) -> Option<Vec<u8>> {
    let mut flags =
        u8::from(descriptor.predicted) << 6 | u8::from(descriptor.not_reference_for_upper_spatial);
    let mut out = vec![0];
    if let Some(picture_id) = descriptor.picture_id {
        flags |= 0x80;
        match picture_id {
            PictureId::Short(value) if value < 128 => out.push(value),
            PictureId::Long(value) if value < 32768 => {
                let [hi, lo] = value.to_be_bytes();
                out.extend([hi | 0x80, lo]);
            }
            _ => return None,
        }
    }
    if let Some(layer) = descriptor.layer {
        if layer.temporal_id > 7
            || layer.spatial_id > 7
            || (layer.depends_on_lower_spatial && layer.spatial_id == 0)
        {
            return None;
        }
        flags |= 0x20;
        out.push(
            layer.temporal_id << 5
                | u8::from(layer.switching_up) << 4
                | layer.spatial_id << 1
                | u8::from(layer.depends_on_lower_spatial),
        );
    }
    match descriptor.references {
        References::Flexible(differences) if descriptor.picture_id.is_some() => {
            let diffs = differences.as_slice();
            if diffs.is_empty() == descriptor.predicted || diffs.iter().any(|&diff| diff > 127) {
                return None;
            }
            flags |= 0x10;
            let mut diffs = diffs.iter().peekable();
            while let Some(diff) = diffs.next() {
                out.push(diff << 1 | u8::from(diffs.peek().is_some()));
            }
        }
        References::Structured { tl0_picture_index }
            if tl0_picture_index.is_some() == descriptor.layer.is_some() =>
        {
            out.extend(tl0_picture_index);
        }
        _ => return None,
    }
    *out.first_mut()? = flags;
    Some(out)
}

fn encode_scalability(ss: &Scalability, out: &mut Vec<u8>) -> Option<()> {
    let count = u8::try_from(ss.group.len()).ok()?;
    if !(1..=8).contains(&ss.spatial_layers) {
        return None;
    }
    out.push(
        ss.spatial_layers.saturating_sub(1) << 5
            | u8::from(ss.resolutions.is_some()) << 4
            | u8::from(count != 0) << 3,
    );
    if let Some(sizes) = &ss.resolutions {
        if sizes.len() != usize::from(ss.spatial_layers) {
            return None;
        }
        for &(width, height) in sizes {
            out.extend(width.to_be_bytes());
            out.extend(height.to_be_bytes());
        }
    }
    if count != 0 {
        out.push(count);
        for entry in &ss.group {
            if entry.temporal_id > 7 {
                return None;
            }
            out.push(
                entry.temporal_id << 5
                    | u8::from(entry.switching_up) << 4
                    | entry.references.count << 2,
            );
            out.extend_from_slice(entry.references.as_slice());
        }
    }
    Some(())
}

fn parse_scalability(bytes: &mut &[u8]) -> Option<Scalability> {
    let flags = take(bytes)?;
    let spatial_layers = (flags >> 5).saturating_add(1);
    let resolutions = if flags & 0x10 != 0 {
        let mut sizes = Vec::with_capacity(usize::from(spatial_layers));
        for _ in 0..spatial_layers {
            let width = u16::from_be_bytes([take(bytes)?, take(bytes)?]);
            let height = u16::from_be_bytes([take(bytes)?, take(bytes)?]);
            sizes.push((width, height));
        }
        Some(sizes)
    } else {
        None
    };
    let mut group = Vec::new();
    if flags & 8 != 0 {
        let count = usize::from(take(bytes)?);
        if bytes.len() <= count {
            return None;
        }
        group.reserve_exact(count);
        for _ in 0..count {
            let entry = take(bytes)?;
            let count = (entry >> 2) & 3;
            let mut entries = [0; 3];
            for diff in entries.iter_mut().take(usize::from(count)) {
                *diff = take(bytes)?;
                if *diff == 0 {
                    return None;
                }
            }
            group.push(PictureGroupEntry {
                temporal_id: entry >> 5,
                switching_up: entry & 0x10 != 0,
                references: Differences { entries, count },
            });
        }
    }
    Some(Scalability {
        spatial_layers,
        resolutions,
        group,
    })
}

fn take(bytes: &mut &[u8]) -> Option<u8> {
    let (&byte, rest) = bytes.split_first()?;
    *bytes = rest;
    Some(byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flexible_references_and_layer_boundaries() {
        let wire = [0xfc, 0x81, 0x23, 0x53, 0x03, 0x06, 0x82];
        let (descriptor, payload) = Descriptor::parse(&wire).unwrap();
        assert_eq!(payload, [0x82]);
        assert_eq!(descriptor.picture_id, Some(PictureId::Long(0x123)));
        assert!(descriptor.frame_start && descriptor.frame_end && descriptor.predicted);
        assert_eq!(
            descriptor.layer,
            Some(Layer {
                temporal_id: 2,
                switching_up: true,
                spatial_id: 1,
                depends_on_lower_spatial: true,
            })
        );
        let References::Flexible(refs) = descriptor.references else {
            panic!("flexible")
        };
        assert_eq!(refs.as_slice(), &[1, 3]);
        for n in 0..wire.len() {
            assert!(Descriptor::parse(&wire[..n]).is_none(), "prefix {n}");
        }
        assert!(Descriptor::parse(&[0xd0, 0, 3, 3, 3, 2, 0x82]).is_none());
    }

    #[test]
    fn spatial_frame_end_is_not_necessarily_picture_end() {
        let source = [0xfc, 0x81, 0x23, 0x53, 0x03, 0x06, 0x82];
        let (descriptor, _) = Descriptor::parse(&source).unwrap();
        let budget = PacketBudget::new(20, 0).unwrap();
        let packets: Vec<_> = packetize(budget, u32::MAX, 123, false, &descriptor, &[1, 2, 3])
            .unwrap()
            .collect();
        assert_eq!(packets.len(), 2);
        assert_eq!(
            packets[0].payload,
            [0xf8, 0x81, 0x23, 0x53, 0x03, 0x06, 1, 2]
        );
        assert_eq!(packets[1].payload, [0xf4, 0x81, 0x23, 0x53, 0x03, 0x06, 3]);
        assert_eq!(packets[1].sequence, 0);
        assert!(packets[1].last && !packets[1].marker);
        let packet = packetize(budget, 1, 123, true, &descriptor, &[4])
            .unwrap()
            .next()
            .unwrap();
        assert!(packet.first && packet.last && packet.marker);
    }

    #[test]
    fn structured_references_keep_scalability_and_leave_payload_intact() {
        let wire = [
            0xae, 0x7f, 0x10, 0xff, 0x38, 0x01, 0x40, 0x00, 0xb4, 0x02, 0x80, 0x01, 0x68, 0x02,
            0x00, 0x34, 0x01, 0x82,
        ];
        let (descriptor, payload) = Descriptor::parse(&wire).unwrap();
        assert_eq!(payload, [0x82]);
        assert_eq!(descriptor.picture_id, Some(PictureId::Short(127)));
        assert_eq!(
            descriptor.references,
            References::Structured {
                tl0_picture_index: Some(255)
            }
        );
        let ss = descriptor.scalability.unwrap();
        assert_eq!(ss.spatial_layers, 2);
        assert_eq!(ss.resolutions.unwrap(), [(320, 180), (640, 360)]);
        assert_eq!(ss.group.len(), 2);
        assert_eq!(ss.group[1].references.as_slice(), &[1]);
        for n in 0..wire.len() {
            assert!(Descriptor::parse(&wire[..n]).is_none(), "prefix {n}");
        }
    }
}
