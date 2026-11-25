//! AV1 RTP OBU elements (AOMedia RTP Payload Format for AV1, version 1.0).

use std::ops::Range;

use pulsebeam_core::h264::MAX_ACCESS_UNIT_SIZE;

use super::{Packet, PacketBudget, PacketizeError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Framing,
    Limit,
}

/// Z/Y delimit OBU fragments, not RTP frames. W controls element lengths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Aggregation {
    pub continuation: bool,
    pub continues: bool,
    pub new_sequence: bool,
    count: u8,
}

impl Aggregation {
    pub fn parse(payload: &[u8]) -> Result<(Self, &[u8]), Error> {
        let (&byte, elements) = payload.split_first().ok_or(Error::Framing)?;
        let header = Self {
            continuation: byte & 0x80 != 0,
            continues: byte & 0x40 != 0,
            new_sequence: byte & 8 != 0,
            count: (byte >> 4) & 3,
        };
        if elements.is_empty() || (header.continuation && header.new_sequence) {
            return Err(Error::Framing);
        }
        Ok((header, elements))
    }

    fn next_element<'a>(&self, remaining: &mut &'a [u8], index: usize) -> Result<&'a [u8], Error> {
        let count = usize::from(self.count);
        let size = if count != 0 && index == count.saturating_sub(1) {
            remaining.len()
        } else {
            if count != 0 && index >= count {
                return Err(Error::Framing);
            }
            read_leb(remaining)?
        };
        let (element, rest) = remaining.split_at_checked(size).ok_or(Error::Framing)?;
        if element.is_empty() {
            return Err(Error::Framing);
        }
        *remaining = rest;
        Ok(element)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layer {
    pub temporal_id: u8,
    pub spatial_id: u8,
}

#[derive(Clone, Copy)]
struct Header {
    byte: u8,
    extension: Option<u8>,
}

impl Header {
    fn parse(bytes: &mut &[u8]) -> Result<Self, Error> {
        let (&byte, rest) = bytes.split_first().ok_or(Error::Framing)?;
        if byte & 0x81 != 0 {
            return Err(Error::Framing);
        }
        *bytes = rest;
        let extension = if byte & 4 != 0 {
            let (&extension, rest) = bytes.split_first().ok_or(Error::Framing)?;
            if extension & 7 != 0 {
                return Err(Error::Framing);
            }
            *bytes = rest;
            Some(extension)
        } else {
            None
        };
        Ok(Self { byte, extension })
    }

    fn len(self) -> usize {
        if self.extension.is_some() { 2 } else { 1 }
    }

    fn kind(self) -> u8 {
        (self.byte >> 3) & 15
    }

    fn carried_in_rtp(self) -> bool {
        !matches!(self.kind(), 2 | 8 | 15)
    }

    fn layer(self) -> Option<Layer> {
        self.extension.map(|value| Layer {
            temporal_id: value >> 5,
            spatial_id: (value >> 3) & 3,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Reconstructed {
    /// Low-overhead OBU stream with a size field on every OBU.
    pub data: Vec<u8>,
    /// Sequence-header OBU including its header and size field, when present.
    pub sequence_header: Option<Range<usize>>,
    pub layer: Option<Layer>,
}

pub(crate) struct Reconstruction {
    limit: usize,
    unit: Reconstructed,
    fragment: Vec<u8>,
    has_frame_header: bool,
}

impl Reconstruction {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            unit: Reconstructed {
                data: Vec::new(),
                sequence_header: None,
                layer: None,
            },
            fragment: Vec::new(),
            has_frame_header: false,
        }
    }

    pub(crate) fn push(&mut self, payload: &[u8]) -> Result<(), Error> {
        let (header, mut bytes) = Aggregation::parse(payload)?;
        if header.continuation == self.fragment.is_empty() {
            return Err(Error::Framing);
        }
        let mut index = 0_usize;
        while !bytes.is_empty() {
            let element = header.next_element(&mut bytes, index)?;
            let continuation = index == 0 && header.continuation;
            let continues = bytes.is_empty() && header.continues;
            if continuation || continues {
                let space = self
                    .limit
                    .saturating_sub(self.unit.data.len())
                    .saturating_sub(self.fragment.len());
                if element.len() > space {
                    return Err(Error::Limit);
                }
                self.fragment.extend_from_slice(element);
                if !continues {
                    let complete = std::mem::take(&mut self.fragment);
                    self.append(&complete)?;
                }
            } else {
                self.append(element)?;
            }
            index = index.saturating_add(1);
        }
        if header.count != 0 && index != usize::from(header.count) {
            return Err(Error::Framing);
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<Reconstructed, Error> {
        if !self.fragment.is_empty() || !self.has_frame_header {
            return Err(Error::Framing);
        }
        Ok(self.unit)
    }

    fn append(&mut self, mut bytes: &[u8]) -> Result<(), Error> {
        let header = Header::parse(&mut bytes)?;
        if header.byte & 2 != 0 && read_leb(&mut bytes)? != bytes.len() {
            return Err(Error::Framing);
        }
        if !header.carried_in_rtp() {
            // Delimiter, tile list and padding are not part of the RTP unit.
            return Ok(());
        }
        let layer = header.layer();
        if self.unit.layer.zip(layer).is_some_and(|(a, b)| a != b) {
            return Err(Error::Framing);
        }
        self.unit.layer = self.unit.layer.or(layer);
        let size = header.len().saturating_add(leb_len(bytes.len()));
        if size.saturating_add(bytes.len()) > self.limit.saturating_sub(self.unit.data.len()) {
            return Err(Error::Limit);
        }
        let start = self.unit.data.len();
        self.unit.data.push(header.byte | 2);
        self.unit.data.extend(header.extension);
        write_leb(bytes.len(), &mut self.unit.data);
        self.unit.data.extend_from_slice(bytes);
        match header.kind() {
            1 => self.unit.sequence_header = Some(start..self.unit.data.len()),
            3 | 6 => self.has_frame_header = true,
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Obu<'a> {
    header: Header,
    body: &'a [u8],
}

impl Obu<'_> {
    fn wire_len(self) -> usize {
        self.header.len().saturating_add(self.body.len())
    }

    fn write(self, range: Range<usize>, output: &mut Vec<u8>) -> Option<()> {
        let prefix = [self.header.byte & !2, self.header.extension.unwrap_or(0)];
        let header_len = self.header.len();
        if range.start < header_len {
            output.extend_from_slice(prefix.get(range.start..range.end.min(header_len))?);
        }
        if range.end > header_len {
            output.extend_from_slice(self.body.get(
                range.start.saturating_sub(header_len)..range.end.saturating_sub(header_len),
            )?);
        }
        Some(())
    }
}

fn next_obu<'a>(input: &mut &'a [u8]) -> Result<Option<Obu<'a>>, Error> {
    while !input.is_empty() {
        let header = Header::parse(input)?;
        if header.byte & 2 == 0 {
            return Err(Error::Framing);
        }
        let len = read_leb(input)?;
        let (body, rest) = input.split_at_checked(len).ok_or(Error::Framing)?;
        *input = rest;
        if header.carried_in_rtp() {
            return Ok(Some(Obu { header, body }));
        }
    }
    Ok(None)
}

/// Packetize one spatial frame in AV1 low-overhead OBU format. The RTP
/// marker belongs to the last transmitted frame in a picture, not every frame.
pub fn packetize(
    budget: PacketBudget,
    sequence: u32,
    timestamp: u32,
    new_sequence: bool,
    end_of_picture: bool,
    mut frame: &[u8],
) -> Result<Packets<'_>, PacketizeError> {
    let capacity = budget.payload_bytes().saturating_sub(1);
    if capacity == 0 || frame.len() > MAX_ACCESS_UNIT_SIZE {
        return Err(PacketizeError::TooLarge);
    }
    let first = next_obu(&mut frame)
        .map_err(|_| PacketizeError::InvalidFraming)?
        .ok_or(PacketizeError::Empty)?;
    let mut rest = frame;
    let mut current = Some(first);
    let mut layer = None;
    let mut has_sequence = false;
    let mut has_frame = false;
    while let Some(obu) = current {
        let next_layer = obu.header.layer();
        if layer.zip(next_layer).is_some_and(|(a, b)| a != b) {
            return Err(PacketizeError::InvalidFraming);
        }
        layer = layer.or(next_layer);
        has_sequence |= obu.header.kind() == 1;
        has_frame |= matches!(obu.header.kind(), 3 | 6);
        current = next_obu(&mut rest).map_err(|_| PacketizeError::InvalidFraming)?;
    }
    if !has_frame || (new_sequence && !has_sequence) {
        return Err(PacketizeError::InvalidFraming);
    }
    Ok(Packets {
        capacity,
        sequence,
        timestamp,
        new_sequence,
        end_of_picture,
        first: true,
        obu: Some(first),
        rest: frame,
        offset: 0,
    })
}

pub struct Packets<'a> {
    capacity: usize,
    sequence: u32,
    timestamp: u32,
    new_sequence: bool,
    end_of_picture: bool,
    first: bool,
    obu: Option<Obu<'a>>,
    rest: &'a [u8],
    offset: usize,
}

impl Iterator for Packets<'_> {
    type Item = Packet<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        let obu = self.obu?;
        let first = std::mem::take(&mut self.first);
        let new_sequence = u8::from(first && self.new_sequence) << 3;
        let complete = self.offset == 0 && obu.wire_len() <= self.capacity;
        let payload = if complete {
            let mut elements = [obu; 3];
            let mut total = obu.wire_len();
            let mut previous = obu;
            let mut count = 1_u8;
            self.obu = next_obu(&mut self.rest).ok()?;
            for slot in elements.iter_mut().skip(1) {
                let Some(next) = self.obu else { break };
                let combined = total
                    .checked_add(leb_len(previous.wire_len()))?
                    .checked_add(next.wire_len())?;
                if combined > self.capacity {
                    break;
                }
                *slot = next;
                previous = next;
                count = count.saturating_add(1);
                total = combined;
                self.obu = next_obu(&mut self.rest).ok()?;
            }
            let mut payload = Vec::with_capacity(total.checked_add(1)?);
            payload.push(count << 4 | new_sequence);
            let mut elements = elements.into_iter().take(usize::from(count)).peekable();
            while let Some(element) = elements.next() {
                if elements.peek().is_some() {
                    write_leb(element.wire_len(), &mut payload);
                }
                element.write(0..element.wire_len(), &mut payload)?;
            }
            payload
        } else {
            let size = self.capacity.min(obu.wire_len().checked_sub(self.offset)?);
            let end = self.offset.checked_add(size)?;
            let continues = end < obu.wire_len();
            let mut payload = Vec::with_capacity(size.checked_add(1)?);
            payload.push(
                0x10 | u8::from(self.offset != 0) << 7 | u8::from(continues) << 6 | new_sequence,
            );
            obu.write(self.offset..end, &mut payload)?;
            if continues {
                self.offset = end;
            } else {
                self.offset = 0;
                self.obu = next_obu(&mut self.rest).ok()?;
            }
            payload
        };
        let last = self.obu.is_none();
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

fn read_leb(bytes: &mut &[u8]) -> Result<usize, Error> {
    let mut value = 0_u64;
    for shift in (0..56).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or(Error::Framing)?;
        *bytes = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return u32::try_from(value)
                .map_err(|_| Error::Framing)
                .map(|value| value as usize);
        }
    }
    Err(Error::Framing)
}

fn leb_len(mut value: usize) -> usize {
    let mut bytes = 1_usize;
    while value >= 128 {
        value >>= 7;
        bytes = bytes.saturating_add(1);
    }
    bytes
}

fn write_leb(mut length: usize, out: &mut Vec<u8>) {
    while length >= 128 {
        out.push(length.to_le_bytes()[0] | 0x80);
        length >>= 7;
    }
    out.push(length.to_le_bytes()[0]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstructs_aggregation_and_fragmented_extended_obu() {
        let mut result = Reconstruction::new(32);
        result.push(&[0x68, 2, 0x08, 0xab, 0x34]).unwrap();
        result.push(&[0x90, 0x28, 0x49, 0x83]).unwrap();
        assert_eq!(
            result.finish().unwrap(),
            Reconstructed {
                data: vec![0x0a, 1, 0xab, 0x36, 0x28, 2, 0x49, 0x83],
                sequence_header: Some(0..3),
                layer: Some(Layer {
                    temporal_id: 1,
                    spatial_id: 1
                }),
            }
        );
    }

    #[test]
    fn packetizes_sized_obus_with_exact_payload_budget() {
        let stream = [
            0x12, 0, 0x0a, 1, 0xab, 0x36, 0x28, 4, 0x49, 0x83, 0xfe, 0xdc,
        ];
        let packets: Vec<_> = packetize(
            PacketBudget::new(17, 0).unwrap(),
            u32::MAX,
            9,
            true,
            false,
            &stream,
        )
        .unwrap()
        .collect();
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0].payload, [0x18, 0x08, 0xab]);
        assert_eq!(packets[1].payload, [0x50, 0x34, 0x28, 0x49, 0x83]);
        assert_eq!(packets[2].payload, [0x90, 0xfe, 0xdc]);
        assert_eq!(packets[1].sequence, 0);
        assert!(packets[0].first && packets[2].last && !packets[2].marker);
        let combined = packetize(PacketBudget::new(22, 0).unwrap(), 1, 9, true, true, &stream)
            .unwrap()
            .next()
            .unwrap();
        assert_eq!(
            combined.payload,
            [0x28, 2, 0x08, 0xab, 0x34, 0x28, 0x49, 0x83, 0xfe, 0xdc]
        );
        assert!(combined.first && combined.last && combined.marker);
        let mut reader = Reconstruction::new(stream.len());
        for packet in packets {
            reader.push(&packet.payload).unwrap();
        }
        assert_eq!(reader.finish().unwrap().data, stream[2..]);
        assert!(matches!(
            packetize(
                PacketBudget::new(20, 0).unwrap(),
                0,
                0,
                true,
                true,
                &stream[..stream.len() - 1]
            ),
            Err(PacketizeError::InvalidFraming)
        ));
    }

    #[test]
    fn rejects_truncation_missing_fragments_and_excess_size() {
        assert_eq!(
            Reconstruction::new(8).push(&[0x00, 0x81]),
            Err(Error::Framing)
        );
        assert_eq!(
            Reconstruction::new(8).push(&[0x20, 1, 0x30]),
            Err(Error::Framing)
        );
        assert_eq!(
            Reconstruction::new(8).push(&[0x90, 0x53]),
            Err(Error::Framing)
        );
        assert_eq!(
            Reconstruction::new(2).push(&[0x10, 0x30, 0x53]),
            Err(Error::Limit)
        );
        let mut incomplete = Reconstruction::new(8);
        incomplete.push(&[0x50, 0x30, 0x53]).unwrap();
        assert_eq!(incomplete.finish(), Err(Error::Framing));
    }
}
