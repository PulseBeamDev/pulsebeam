//! Virtual-time cursor over original wire records. No media interpretation.

use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketKind {
    Rtp,
    Rtcp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Packet<'a> {
    pub epoch: u64,
    pub index: usize,
    pub send_after: Duration,
    pub kind: PacketKind,
    pub bytes: &'a [u8],
}

pub struct Cursor<'a> {
    packets: Vec<Packet<'a>>,
    epoch: u64,
    next: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(mut bytes: &'a [u8], index: &str) -> Result<Self, CaptureError> {
        let mut lines = index.lines();
        if lines.next() != Some("relative_us\tkind\toffset\tsize") {
            return Err(CaptureError);
        }
        let mut packets = Vec::new();
        let mut offset = 0usize;
        let mut last_time = Duration::ZERO;
        for line in lines {
            let mut fields = line.split('\t');
            let send_after = Duration::from_micros(number(fields.next())?);
            let kind = match fields.next() {
                Some("rtp") => PacketKind::Rtp,
                Some("rtcp") => PacketKind::Rtcp,
                _ => return Err(CaptureError),
            };
            let start: usize = number(fields.next())?;
            let size: usize = number(fields.next())?;
            if start != offset || send_after < last_time || size == 0 || fields.next().is_some() {
                return Err(CaptureError);
            }
            let (packet, rest) = bytes.split_at_checked(size).ok_or(CaptureError)?;
            packets.push(Packet {
                epoch: 0,
                index: packets.len(),
                kind,
                send_after,
                bytes: packet,
            });
            last_time = send_after;
            bytes = rest;
            offset = offset.checked_add(size).ok_or(CaptureError)?;
        }
        if !bytes.is_empty() || packets.is_empty() {
            return Err(CaptureError);
        }
        Ok(Self {
            packets,
            epoch: 0,
            next: 0,
        })
    }

    /// Return the next due packet, keeping recorded order at equal send times.
    pub fn next_due(&mut self, elapsed: Duration) -> Option<Packet<'a>> {
        let mut packet = *self.packets.get(self.next)?;
        if elapsed < packet.send_after {
            return None;
        }
        packet.epoch = self.epoch;
        self.next = self.next.checked_add(1)?;
        Some(packet)
    }

    /// Start a new capture lifetime. Never splice the old and new RTP timelines.
    pub fn rewind(&mut self) -> Result<u64, CaptureError> {
        self.epoch = self.epoch.checked_add(1).ok_or(CaptureError)?;
        self.next = 0;
        Ok(self.epoch)
    }
}

pub struct DescriptorRtp<'a> {
    pub sequence: u16,
    pub timestamp: u32,
    pub marker: bool,
    pub payload: &'a [u8],
    pub descriptor: &'a [u8],
}

/// Read the capture layout with no padding or CSRCs and DD at extension ID 9.
pub fn descriptor_rtp(bytes: &[u8]) -> Option<DescriptorRtp<'_>> {
    let (header, body) = bytes.split_at_checked(16)?;
    let &[
        0x90,
        flags,
        seq_hi,
        seq_lo,
        a,
        b,
        c,
        d,
        ..,
        ext_hi,
        ext_lo,
        len_hi,
        len_lo,
    ] = header
    else {
        return None;
    };
    let two_byte = match [ext_hi, ext_lo] {
        [0xbe, 0xde] => false,
        [0x10, 0x00] => true,
        _ => return None,
    };
    let ext_len = usize::from(u16::from_be_bytes([len_hi, len_lo])) * 4;
    let (mut extensions, payload) = body.split_at_checked(ext_len)?;
    while let Some((&first, tail)) = extensions.split_first() {
        extensions = tail;
        if first == 0 {
            continue;
        }
        let (id, len) = if two_byte {
            let (&len, tail) = extensions.split_first()?;
            extensions = tail;
            (first, usize::from(len))
        } else {
            (first >> 4, usize::from(first & 15) + 1)
        };
        let (extension, tail) = extensions.split_at_checked(len)?;
        if id == 9 {
            return Some(DescriptorRtp {
                sequence: u16::from_be_bytes([seq_hi, seq_lo]),
                timestamp: u32::from_be_bytes([a, b, c, d]),
                marker: flags & 0x80 != 0,
                payload,
                descriptor: extension,
            });
        }
        extensions = tail;
    }
    None
}

fn number<T: std::str::FromStr>(field: Option<&str>) -> Result<T, CaptureError> {
    field.ok_or(CaptureError)?.parse().map_err(|_| CaptureError)
}
