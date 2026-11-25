//! VP8 RTP payload descriptors and partition packetization (RFC 7741).

use pulsebeam_core::h264::MAX_ACCESS_UNIT_SIZE;

use super::{Packet, PacketBudget, PacketizeError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TemporalLayer {
    pub id: u8,
    pub layer_sync: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub non_reference: bool,
    pub partition_start: bool,
    pub partition_id: u8,
    pub picture_id: Option<u16>,
    pub tl0_picture_index: Option<u8>,
    pub temporal: Option<TemporalLayer>,
    pub key_index: Option<u8>,
}

impl Descriptor {
    pub fn parse(payload: &[u8]) -> Option<(Self, &[u8])> {
        let (&first, mut bytes) = payload.split_first()?;
        let mut descriptor = Self {
            non_reference: first & 0x20 != 0,
            partition_start: first & 0x10 != 0,
            partition_id: first & 7,
            picture_id: None,
            tl0_picture_index: None,
            temporal: None,
            key_index: None,
        };
        if first & 0x80 != 0 {
            let extension = take(&mut bytes)?;
            if extension & 0x60 == 0x40 {
                return None;
            }
            if extension & 0x80 != 0 {
                let picture = take(&mut bytes)?;
                descriptor.picture_id = Some(if picture & 0x80 == 0 {
                    u16::from(picture)
                } else {
                    u16::from_be_bytes([picture & 0x7f, take(&mut bytes)?])
                });
            }
            if extension & 0x40 != 0 {
                descriptor.tl0_picture_index = Some(take(&mut bytes)?);
            }
            if extension & 0x30 != 0 {
                let layer = take(&mut bytes)?;
                if extension & 0x20 != 0 {
                    descriptor.temporal = Some(TemporalLayer {
                        id: layer >> 6,
                        layer_sync: layer & 0x20 != 0,
                    });
                }
                if extension & 0x10 != 0 {
                    descriptor.key_index = Some(layer & 0x1f);
                }
            }
        }
        (!bytes.is_empty()).then_some((descriptor, bytes))
    }

    pub fn starts_frame(self) -> bool {
        self.partition_start && self.partition_id == 0
    }
}

fn take(bytes: &mut &[u8]) -> Option<u8> {
    let (&byte, rest) = bytes.split_first()?;
    *bytes = rest;
    Some(byte)
}

/// Packetize supplied VP8 partitions with the mandatory payload descriptor.
/// The iterator borrows the encoded frame and allocates only the next payload.
pub fn packetize<'a>(
    budget: PacketBudget,
    sequence: u32,
    timestamp: u32,
    partitions: &'a [&'a [u8]],
) -> Result<Packets<'a>, PacketizeError> {
    if partitions.is_empty() || partitions.iter().any(|partition| partition.is_empty()) {
        return Err(PacketizeError::Empty);
    }
    let chunk_size = budget.payload_bytes().saturating_sub(1);
    if chunk_size == 0
        || partitions.len() > 8
        || partitions
            .iter()
            .try_fold(0_usize, |len, partition| len.checked_add(partition.len()))
            .is_none_or(|len| len > MAX_ACCESS_UNIT_SIZE)
    {
        return Err(PacketizeError::TooLarge);
    }
    Ok(Packets {
        chunk_size,
        sequence,
        timestamp,
        partitions,
        partition: 0,
        offset: 0,
    })
}

pub struct Packets<'a> {
    chunk_size: usize,
    sequence: u32,
    timestamp: u32,
    partitions: &'a [&'a [u8]],
    partition: u8,
    offset: usize,
}

impl Iterator for Packets<'_> {
    type Item = Packet<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        let input = self.partitions.get(usize::from(self.partition))?;
        let start = self.offset == 0;
        let first = start && self.partition == 0;
        let end = self.offset.saturating_add(self.chunk_size).min(input.len());
        let chunk = input.get(self.offset..end)?;
        let mut payload = Vec::with_capacity(chunk.len().saturating_add(1));
        payload.push(self.partition | if start { 0x10 } else { 0 });
        payload.extend_from_slice(chunk);
        self.offset = end;
        if end == input.len() {
            self.partition = self.partition.saturating_add(1);
            self.offset = 0;
        }
        let last = usize::from(self.partition) == self.partitions.len();
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        Some(Packet {
            sequence,
            timestamp: self.timestamp,
            marker: last,
            first,
            last,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_extended_wire_descriptor_without_consuming_frame_bytes() {
        let wire = [0xb0, 0xf0, 0x92, 0x34, 0xfe, 0xa7, 0x00];
        let (descriptor, data) = Descriptor::parse(&wire).unwrap();
        assert_eq!(data, [0]);
        assert!(descriptor.starts_frame());
        assert_eq!(
            descriptor,
            Descriptor {
                non_reference: true,
                partition_start: true,
                partition_id: 0,
                picture_id: Some(0x1234),
                tl0_picture_index: Some(254),
                temporal: Some(TemporalLayer {
                    id: 2,
                    layer_sync: true,
                }),
                key_index: Some(7),
            }
        );
        for len in 0..wire.len() {
            assert!(Descriptor::parse(&wire[..len]).is_none(), "prefix {len}");
        }
    }

    #[test]
    fn writes_partition_starts_and_wraps_without_exceeding_budget() {
        let budget = PacketBudget::new(15, 0).unwrap();
        let packets = packetize(budget, u32::MAX, 9, &[&[0, 1, 2], &[3, 4]])
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0].payload, [0x10, 0, 1]);
        assert_eq!(packets[1].payload, [0x00, 2]);
        assert_eq!(packets[2].payload, [0x11, 3, 4]);
        assert_eq!(
            packets.iter().map(|p| p.sequence).collect::<Vec<_>>(),
            [u32::MAX, 0, 1]
        );
        assert!(packets[0].first && !packets[0].marker);
        assert!(!packets[1].first && !packets[1].last);
        assert!(packets[2].last && packets[2].marker);
    }
}
