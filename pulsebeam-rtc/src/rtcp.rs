use crate::packet::{PacketError, RtcpCompound, RtcpPacket};

pub use crate::packet::{RtcpCompound as Compound, RtcpPacket as Packet};

const REPORT_BLOCK_BYTES: usize = 24;

fn u8_at(bytes: &[u8], offset: usize) -> Result<u8, PacketError> {
    bytes.get(offset).copied().ok_or(PacketError::InvalidLength)
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, PacketError> {
    let end = offset.checked_add(2).ok_or(PacketError::InvalidLength)?;
    bytes
        .get(offset..end)
        .and_then(|value| value.try_into().ok())
        .map(u16::from_be_bytes)
        .ok_or(PacketError::InvalidLength)
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, PacketError> {
    let end = offset.checked_add(4).ok_or(PacketError::InvalidLength)?;
    bytes
        .get(offset..end)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or(PacketError::InvalidLength)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReportBlock<'a> {
    bytes: &'a [u8],
    ssrc: u32,
    fraction_lost: u8,
    cumulative_lost: i32,
    highest_sequence: u32,
    jitter: u32,
    last_sender_report: u32,
    delay_since_last_sender_report: u32,
}

impl<'a> ReportBlock<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.len() != REPORT_BLOCK_BYTES {
            return Err(PacketError::InvalidLength);
        }
        let lost = (u32::from(u8_at(bytes, 5)?) << 16)
            | (u32::from(u8_at(bytes, 6)?) << 8)
            | u32::from(u8_at(bytes, 7)?);
        let cumulative_lost = if lost & 0x80_0000 != 0 {
            i32::from_be_bytes((lost | 0xff00_0000).to_be_bytes())
        } else {
            i32::from_be_bytes(lost.to_be_bytes())
        };
        Ok(Self {
            bytes,
            ssrc: u32_at(bytes, 0)?,
            fraction_lost: u8_at(bytes, 4)?,
            cumulative_lost,
            highest_sequence: u32_at(bytes, 8)?,
            jitter: u32_at(bytes, 12)?,
            last_sender_report: u32_at(bytes, 16)?,
            delay_since_last_sender_report: u32_at(bytes, 20)?,
        })
    }

    fn parse_optional(bytes: &'a [u8]) -> Option<Self> {
        Self::parse(bytes).ok()
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }
    pub fn fraction_lost(&self) -> u8 {
        self.fraction_lost
    }
    pub fn cumulative_lost(&self) -> i32 {
        self.cumulative_lost
    }
    pub fn highest_sequence(&self) -> u32 {
        self.highest_sequence
    }
    pub fn jitter(&self) -> u32 {
        self.jitter
    }
    pub fn last_sender_report(&self) -> u32 {
        self.last_sender_report
    }
    pub fn delay_since_last_sender_report(&self) -> u32 {
        self.delay_since_last_sender_report
    }
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SenderReport<'a> {
    sender_ssrc: u32,
    ntp_seconds: u32,
    ntp_fraction: u32,
    rtp_timestamp: u32,
    packet_count: u32,
    octet_count: u32,
    reports: &'a [u8],
}

impl<'a> SenderReport<'a> {
    fn parse(packet: RtcpPacket<'a>) -> Result<Self, PacketError> {
        let reports_len = usize::from(packet.count())
            .checked_mul(REPORT_BLOCK_BYTES)
            .ok_or(PacketError::InvalidLength)?;
        let body = packet.body();
        let expected = 24usize
            .checked_add(reports_len)
            .ok_or(PacketError::InvalidLength)?;
        if body.len() != expected {
            return Err(PacketError::InvalidLength);
        }
        let reports = body.get(24..).ok_or(PacketError::InvalidLength)?;
        for block in reports.chunks_exact(REPORT_BLOCK_BYTES) {
            ReportBlock::parse(block)?;
        }
        Ok(Self {
            sender_ssrc: u32_at(body, 0)?,
            ntp_seconds: u32_at(body, 4)?,
            ntp_fraction: u32_at(body, 8)?,
            rtp_timestamp: u32_at(body, 12)?,
            packet_count: u32_at(body, 16)?,
            octet_count: u32_at(body, 20)?,
            reports,
        })
    }
    pub fn sender_ssrc(&self) -> u32 {
        self.sender_ssrc
    }
    pub fn ntp_seconds(&self) -> u32 {
        self.ntp_seconds
    }
    pub fn ntp_fraction(&self) -> u32 {
        self.ntp_fraction
    }
    pub fn rtp_timestamp(&self) -> u32 {
        self.rtp_timestamp
    }
    pub fn packet_count(&self) -> u32 {
        self.packet_count
    }
    pub fn octet_count(&self) -> u32 {
        self.octet_count
    }
    pub fn reports(&self) -> impl Iterator<Item = ReportBlock<'a>> {
        self.reports
            .chunks_exact(REPORT_BLOCK_BYTES)
            .filter_map(ReportBlock::parse_optional)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverReport<'a> {
    receiver_ssrc: u32,
    reports: &'a [u8],
}

impl<'a> ReceiverReport<'a> {
    fn parse(packet: RtcpPacket<'a>) -> Result<Self, PacketError> {
        let reports_len = usize::from(packet.count())
            .checked_mul(REPORT_BLOCK_BYTES)
            .ok_or(PacketError::InvalidLength)?;
        let body = packet.body();
        let expected = 4usize
            .checked_add(reports_len)
            .ok_or(PacketError::InvalidLength)?;
        if body.len() != expected {
            return Err(PacketError::InvalidLength);
        }
        let reports = body.get(4..).ok_or(PacketError::InvalidLength)?;
        for block in reports.chunks_exact(REPORT_BLOCK_BYTES) {
            ReportBlock::parse(block)?;
        }
        Ok(Self {
            receiver_ssrc: u32_at(body, 0)?,
            reports,
        })
    }
    pub fn receiver_ssrc(&self) -> u32 {
        self.receiver_ssrc
    }
    pub fn reports(&self) -> impl Iterator<Item = ReportBlock<'a>> {
        self.reports
            .chunks_exact(REPORT_BLOCK_BYTES)
            .filter_map(ReportBlock::parse_optional)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Feedback<'a> {
    sender_ssrc: u32,
    media_ssrc: u32,
    fmt: u8,
    fci: &'a [u8],
}

impl<'a> Feedback<'a> {
    fn parse(packet: RtcpPacket<'a>, min_body: usize) -> Result<Self, PacketError> {
        let body = packet.body();
        if body.len() < min_body || body.len() < 8 {
            return Err(PacketError::InvalidLength);
        }
        Ok(Self {
            sender_ssrc: u32_at(body, 0)?,
            media_ssrc: u32_at(body, 4)?,
            fmt: packet.count(),
            fci: body.get(8..).ok_or(PacketError::InvalidLength)?,
        })
    }
    pub fn sender_ssrc(&self) -> u32 {
        self.sender_ssrc
    }
    pub fn media_ssrc(&self) -> u32 {
        self.media_ssrc
    }
    pub fn fmt(&self) -> u8 {
        self.fmt
    }
    pub fn fci(&self) -> &'a [u8] {
        self.fci
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nack<'a> {
    feedback: Feedback<'a>,
}
impl<'a> Nack<'a> {
    fn parse(packet: RtcpPacket<'a>) -> Result<Self, PacketError> {
        let feedback = Feedback::parse(packet, 8)?;
        if feedback.fci().is_empty() || !feedback.fci().len().is_multiple_of(4) {
            return Err(PacketError::InvalidLength);
        }
        Ok(Self { feedback })
    }
    pub fn sender_ssrc(&self) -> u32 {
        self.feedback.sender_ssrc()
    }
    pub fn media_ssrc(&self) -> u32 {
        self.feedback.media_ssrc()
    }
    pub fn pairs(&self) -> impl Iterator<Item = (u16, u16)> + 'a {
        self.feedback
            .fci()
            .chunks_exact(4)
            .filter_map(|bytes| Some((u16_at(bytes, 0).ok()?, u16_at(bytes, 2).ok()?)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pli<'a> {
    feedback: Feedback<'a>,
}
impl<'a> Pli<'a> {
    fn parse(packet: RtcpPacket<'a>) -> Result<Self, PacketError> {
        let feedback = Feedback::parse(packet, 8)?;
        if !feedback.fci().is_empty() {
            return Err(PacketError::InvalidLength);
        }
        Ok(Self { feedback })
    }
    pub fn sender_ssrc(&self) -> u32 {
        self.feedback.sender_ssrc()
    }
    pub fn media_ssrc(&self) -> u32 {
        self.feedback.media_ssrc()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fir<'a> {
    feedback: Feedback<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirEntry {
    ssrc: u32,
    sequence_number: u8,
}

impl FirEntry {
    pub const fn ssrc(self) -> u32 {
        self.ssrc
    }
    pub const fn sequence_number(self) -> u8 {
        self.sequence_number
    }
}
impl<'a> Fir<'a> {
    fn parse(packet: RtcpPacket<'a>) -> Result<Self, PacketError> {
        let feedback = Feedback::parse(packet, 8)?;
        if feedback.media_ssrc() != 0
            || feedback.fci().is_empty()
            || !feedback.fci().len().is_multiple_of(8)
        {
            return Err(PacketError::InvalidLength);
        }
        if feedback.fci().chunks_exact(8).any(|bytes| {
            bytes.get(5).copied().unwrap_or_default() != 0
                || bytes.get(6).copied().unwrap_or_default() != 0
                || bytes.get(7).copied().unwrap_or_default() != 0
        }) {
            return Err(PacketError::InvalidValue);
        }
        Ok(Self { feedback })
    }
    pub fn sender_ssrc(&self) -> u32 {
        self.feedback.sender_ssrc()
    }
    pub fn media_ssrc(&self) -> u32 {
        self.feedback.media_ssrc()
    }
    pub fn entries(&self) -> impl Iterator<Item = FirEntry> + 'a {
        self.feedback.fci().chunks_exact(8).filter_map(|bytes| {
            Some(FirEntry {
                ssrc: u32_at(bytes, 0).ok()?,
                sequence_number: *bytes.get(4)?,
            })
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TwccReferenceTime(u32);

impl TwccReferenceTime {
    pub const fn ticks(self) -> u32 {
        self.0
    }
    pub fn seconds(self) -> f64 {
        f64::from(self.0) / 64.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TwccRecvDelta(i16);

impl TwccRecvDelta {
    pub const fn ticks(self) -> i16 {
        self.0
    }
    pub fn micros(self) -> i32 {
        i32::from(self.0).saturating_mul(250)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TwccPacketStatus {
    NotReceived { sequence: u16 },
    Received { sequence: u16, delta: TwccRecvDelta },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Twcc<'a> {
    feedback: Feedback<'a>,
    base_sequence: u16,
    packet_count: u16,
    reference_time: TwccReferenceTime,
    feedback_count: u8,
    delta_start: usize,
}
impl<'a> Twcc<'a> {
    fn parse(packet: RtcpPacket<'a>) -> Result<Self, PacketError> {
        let feedback = Feedback::parse(packet, 16)?;
        let fci = feedback.fci();
        let base_sequence = u16_at(fci, 0)?;
        let packet_count = u16_at(fci, 2)?;
        let reference_time = TwccReferenceTime(
            (u32::from(u8_at(fci, 4)?) << 16)
                | (u32::from(u8_at(fci, 5)?) << 8)
                | u32::from(u8_at(fci, 6)?),
        );
        let feedback_count = u8_at(fci, 7)?;
        let (delta_start, _) = validate_twcc_chunks(fci, packet_count)?;
        Ok(Self {
            feedback,
            base_sequence,
            packet_count,
            reference_time,
            feedback_count,
            delta_start,
        })
    }
    pub fn sender_ssrc(&self) -> u32 {
        self.feedback.sender_ssrc()
    }
    pub fn media_ssrc(&self) -> u32 {
        self.feedback.media_ssrc()
    }
    pub fn base_sequence(&self) -> u16 {
        self.base_sequence
    }
    pub fn packet_count(&self) -> u16 {
        self.packet_count
    }
    pub const fn reference_time(&self) -> TwccReferenceTime {
        self.reference_time
    }
    pub const fn feedback_count(&self) -> u8 {
        self.feedback_count
    }
    pub fn statuses(&self) -> TwccStatuses<'a> {
        TwccStatuses {
            fci: self.feedback.fci(),
            base_sequence: self.base_sequence,
            packet_count: self.packet_count,
            chunk_offset: 8,
            status_index: 0,
            delta_offset: self.delta_start,
            current_chunk: 0,
            chunk_remaining: 0,
            current_slot: 0,
        }
    }
}

#[allow(
    clippy::arithmetic_side_effects,
    clippy::match_same_arms,
    reason = "TWCC wire limits and status counters are validated before arithmetic"
)]
fn validate_twcc_chunks(fci: &[u8], packet_count: u16) -> Result<(usize, u16), PacketError> {
    let mut offset = 8usize;
    let mut statuses = 0u16;
    while statuses < packet_count {
        let chunk = u16_at(fci, offset)?;
        offset = offset.checked_add(2).ok_or(PacketError::InvalidLength)?;
        let available = if chunk & 0x8000 == 0 {
            chunk & 0x1fff
        } else if chunk & 0x4000 == 0 {
            14
        } else {
            7
        };
        let count = available.min(packet_count - statuses);
        if available == 0 {
            return Err(PacketError::InvalidValue);
        }
        statuses = statuses
            .checked_add(count)
            .ok_or(PacketError::InvalidLength)?;
    }
    let delta_start = offset;
    let mut chunk_offset = 8usize;
    let mut delta_offset = delta_start;
    let mut checked = 0u16;
    while checked < packet_count {
        let chunk = u16_at(fci, chunk_offset)?;
        chunk_offset = chunk_offset
            .checked_add(2)
            .ok_or(PacketError::InvalidLength)?;
        let capacity = if chunk & 0x8000 == 0 {
            chunk & 0x1fff
        } else if chunk & 0x4000 == 0 {
            14
        } else {
            7
        };
        let count = capacity.min(packet_count - checked);
        if capacity == 0 {
            return Err(PacketError::InvalidValue);
        }
        for slot in 0..count {
            match twcc_chunk_symbol(chunk, slot) {
                0 => {}
                1 => {
                    delta_offset = delta_offset
                        .checked_add(1)
                        .ok_or(PacketError::InvalidLength)?;
                }
                2 => {
                    delta_offset = delta_offset
                        .checked_add(2)
                        .ok_or(PacketError::InvalidLength)?;
                }
                3 => return Err(PacketError::InvalidValue),
                _ => return Err(PacketError::InvalidValue),
            }
            if delta_offset > fci.len() {
                return Err(PacketError::InvalidLength);
            }
        }
        checked = checked
            .checked_add(count)
            .ok_or(PacketError::InvalidLength)?;
    }
    let trailing = fci.get(delta_offset..).ok_or(PacketError::InvalidLength)?;
    if !trailing.is_empty() {
        return Err(PacketError::InvalidValue);
    }
    Ok((delta_start, packet_count))
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "slot is bounded to the vector chunk width by the caller"
)]
fn twcc_chunk_symbol(chunk: u16, slot: u16) -> u8 {
    if chunk & 0x8000 == 0 {
        ((chunk >> 13) & 3) as u8
    } else if chunk & 0x4000 == 0 {
        ((chunk >> (13 - slot)) & 1) as u8
    } else {
        ((chunk >> (12 - slot * 2)) & 3) as u8
    }
}

pub struct TwccStatuses<'a> {
    fci: &'a [u8],
    base_sequence: u16,
    packet_count: u16,
    chunk_offset: usize,
    status_index: u16,
    delta_offset: usize,
    current_chunk: u16,
    chunk_remaining: u16,
    current_slot: u16,
}
#[allow(
    clippy::arithmetic_side_effects,
    reason = "the validated TWCC iterator advances only within bounded chunks and delta bytes"
)]
impl<'a> Iterator for TwccStatuses<'a> {
    type Item = Result<TwccPacketStatus, PacketError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.status_index >= self.packet_count {
            return None;
        }
        if self.chunk_remaining == 0 {
            self.current_chunk = match u16_at(self.fci, self.chunk_offset) {
                Ok(value) => value,
                Err(error) => return Some(Err(error)),
            };
            self.chunk_offset += 2;
            let capacity = if self.current_chunk & 0x8000 == 0 {
                self.current_chunk & 0x1fff
            } else if self.current_chunk & 0x4000 == 0 {
                14
            } else {
                7
            };
            self.chunk_remaining = capacity.min(self.packet_count - self.status_index);
            self.current_slot = 0;
            if capacity == 0 {
                return Some(Err(PacketError::InvalidValue));
            }
        }
        let symbol = twcc_chunk_symbol(self.current_chunk, self.current_slot);
        self.chunk_remaining -= 1;
        self.current_slot += 1;
        self.status_index += 1;
        let sequence = self.base_sequence.wrapping_add(self.status_index - 1);
        let result = match symbol {
            0 => Ok(TwccPacketStatus::NotReceived { sequence }),
            1 => self
                .fci
                .get(self.delta_offset)
                .copied()
                .map(|byte| {
                    self.delta_offset += 1;
                    TwccPacketStatus::Received {
                        sequence,
                        delta: TwccRecvDelta(i16::from(i8::from_ne_bytes([byte]))),
                    }
                })
                .ok_or(PacketError::InvalidLength),
            2 => self
                .delta_offset
                .checked_add(2)
                .and_then(|end| self.fci.get(self.delta_offset..end))
                .and_then(|v| v.try_into().ok())
                .map(|v: [u8; 2]| {
                    self.delta_offset += 2;
                    TwccPacketStatus::Received {
                        sequence,
                        delta: TwccRecvDelta(i16::from_be_bytes(v)),
                    }
                })
                .ok_or(PacketError::InvalidLength),
            _ => Err(PacketError::InvalidValue),
        };
        Some(result)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sdes<'a> {
    body: &'a [u8],
    count: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SdesItem<'a> {
    kind: u8,
    value: &'a [u8],
}
impl<'a> SdesItem<'a> {
    pub const fn kind(self) -> u8 {
        self.kind
    }
    pub const fn value(self) -> &'a [u8] {
        self.value
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SdesChunk<'a> {
    ssrc: u32,
    items: &'a [u8],
}
impl<'a> SdesChunk<'a> {
    pub const fn ssrc(self) -> u32 {
        self.ssrc
    }
    pub fn items(self) -> SdesItems<'a> {
        SdesItems { bytes: self.items }
    }
}
impl<'a> Sdes<'a> {
    fn parse(packet: RtcpPacket<'a>) -> Result<Self, PacketError> {
        let mut bytes = packet.body();
        for _ in 0..packet.count() {
            let (_, consumed) = parse_sdes_chunk(bytes)?;
            bytes = bytes.get(consumed..).ok_or(PacketError::InvalidLength)?;
        }
        if !bytes.is_empty() {
            return Err(PacketError::InvalidLength);
        }
        Ok(Self {
            body: packet.body(),
            count: packet.count(),
        })
    }
    pub fn count(&self) -> u8 {
        self.count
    }
    pub fn chunks(&self) -> SdesChunks<'a> {
        SdesChunks {
            bytes: self.body,
            remaining: self.count,
        }
    }
}

fn parse_sdes_chunk(bytes: &[u8]) -> Result<(&[u8], usize), PacketError> {
    if bytes.len() < 4 {
        return Err(PacketError::InvalidLength);
    }
    let mut offset = 4usize;
    loop {
        let kind = u8_at(bytes, offset)?;
        offset = offset.checked_add(1).ok_or(PacketError::InvalidLength)?;
        if kind == 0 {
            let chunk = bytes
                .get(..offset.checked_sub(1).ok_or(PacketError::InvalidLength)?)
                .ok_or(PacketError::InvalidLength)?;
            let padded = offset.checked_add(3).ok_or(PacketError::InvalidLength)? & !3;
            if padded > bytes.len() {
                return Err(PacketError::InvalidLength);
            }
            if bytes
                .get(offset..padded)
                .is_none_or(|padding| padding.iter().any(|byte| *byte != 0))
            {
                return Err(PacketError::InvalidValue);
            }
            return Ok((chunk, padded));
        }
        let length = usize::from(u8_at(bytes, offset)?);
        offset = offset.checked_add(1).ok_or(PacketError::InvalidLength)?;
        offset = offset
            .checked_add(length)
            .ok_or(PacketError::InvalidLength)?;
        if offset > bytes.len() {
            return Err(PacketError::InvalidLength);
        }
    }
}

pub struct SdesChunks<'a> {
    bytes: &'a [u8],
    remaining: u8,
}
impl<'a> Iterator for SdesChunks<'a> {
    type Item = SdesChunk<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let (chunk, consumed) = parse_sdes_chunk(self.bytes).ok()?;
        self.bytes = self.bytes.get(consumed..)?;
        self.remaining = self.remaining.checked_sub(1)?;
        let ssrc = u32_at(chunk, 0).ok()?;
        Some(SdesChunk {
            ssrc,
            items: chunk.get(4..)?,
        })
    }
}

pub struct SdesItems<'a> {
    bytes: &'a [u8],
}
impl<'a> Iterator for SdesItems<'a> {
    type Item = SdesItem<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let kind = *self.bytes.first()?;
        if kind == 0 {
            self.bytes = &[];
            return None;
        }
        let length = usize::from(*self.bytes.get(1)?);
        let end = 2usize.checked_add(length)?;
        let value = self.bytes.get(2..end)?;
        self.bytes = self.bytes.get(end..)?;
        Some(SdesItem { kind, value })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bye<'a> {
    ssrcs: &'a [u8],
    reason: Option<&'a [u8]>,
}
impl<'a> Bye<'a> {
    fn parse(packet: RtcpPacket<'a>) -> Result<Self, PacketError> {
        let count = usize::from(packet.count());
        if count == 0 {
            return Err(PacketError::InvalidLength);
        }
        let ssrc_bytes = count.checked_mul(4).ok_or(PacketError::InvalidLength)?;
        let body = packet.body();
        if body.len() < ssrc_bytes {
            return Err(PacketError::InvalidLength);
        }
        let ssrcs = body.get(..ssrc_bytes).ok_or(PacketError::InvalidLength)?;
        let rest = body.get(ssrc_bytes..).ok_or(PacketError::InvalidLength)?;
        let reason = if rest.is_empty() {
            None
        } else {
            let length = usize::from(u8_at(rest, 0)?);
            let end = 1usize
                .checked_add(length)
                .ok_or(PacketError::InvalidLength)?;
            let padding = rest
                .len()
                .checked_sub(end)
                .ok_or(PacketError::InvalidLength)?;
            if padding > 3
                || rest
                    .get(end..)
                    .is_none_or(|bytes| bytes.iter().any(|byte| *byte != 0))
            {
                return Err(PacketError::InvalidLength);
            }
            Some(rest.get(1..end).ok_or(PacketError::InvalidLength)?)
        };
        Ok(Self { ssrcs, reason })
    }
    pub fn ssrcs(&self) -> impl Iterator<Item = u32> + 'a {
        self.ssrcs
            .chunks_exact(4)
            .filter_map(|bytes| u32_at(bytes, 0).ok())
    }
    pub fn reason(&self) -> Option<&'a [u8]> {
        self.reason
    }
}

impl<'a> RtcpPacket<'a> {
    pub fn sender_report(&self) -> Result<Option<SenderReport<'a>>, PacketError> {
        (self.packet_type() == 200)
            .then(|| SenderReport::parse(*self))
            .transpose()
    }
    pub fn receiver_report(&self) -> Result<Option<ReceiverReport<'a>>, PacketError> {
        (self.packet_type() == 201)
            .then(|| ReceiverReport::parse(*self))
            .transpose()
    }
    pub fn sdes(&self) -> Result<Option<Sdes<'a>>, PacketError> {
        (self.packet_type() == 202)
            .then(|| Sdes::parse(*self))
            .transpose()
    }
    pub fn bye(&self) -> Result<Option<Bye<'a>>, PacketError> {
        (self.packet_type() == 203)
            .then(|| Bye::parse(*self))
            .transpose()
    }
    pub fn nack(&self) -> Result<Option<Nack<'a>>, PacketError> {
        (self.packet_type() == 205 && self.count() == 1)
            .then(|| Nack::parse(*self))
            .transpose()
    }
    pub fn pli(&self) -> Result<Option<Pli<'a>>, PacketError> {
        (self.packet_type() == 206 && self.count() == 1)
            .then(|| Pli::parse(*self))
            .transpose()
    }
    pub fn fir(&self) -> Result<Option<Fir<'a>>, PacketError> {
        (self.packet_type() == 206 && self.count() == 4)
            .then(|| Fir::parse(*self))
            .transpose()
    }
    pub fn twcc(&self) -> Result<Option<Twcc<'a>>, PacketError> {
        (self.packet_type() == 205 && self.count() == 15)
            .then(|| Twcc::parse(*self))
            .transpose()
    }
}

pub fn typed(packet: RtcpPacket<'_>) -> Result<(), PacketError> {
    match packet.packet_type() {
        200 => SenderReport::parse(packet).map(|_| ()),
        201 => ReceiverReport::parse(packet).map(|_| ()),
        202 => Sdes::parse(packet).map(|_| ()),
        203 => Bye::parse(packet).map(|_| ()),
        205 if packet.count() == 1 => Nack::parse(packet).map(|_| ()),
        205 if packet.count() == 15 => Twcc::parse(packet).map(|_| ()),
        206 if packet.count() == 1 => Pli::parse(packet).map(|_| ()),
        206 if packet.count() == 4 => Fir::parse(packet).map(|_| ()),
        _ => Ok(()),
    }
}

pub fn validate_compound(bytes: &[u8]) -> Result<(), PacketError> {
    let compound = RtcpCompound::parse(bytes)?;
    for packet in compound {
        typed(packet?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_sender_and_receiver_reports() {
        let mut sr = vec![0x80, 200, 0, 6];
        sr.extend_from_slice(&[0; 24]);
        let mut rr = vec![0x80, 201, 0, 1];
        rr.extend_from_slice(&[0; 4]);
        let mut bytes = sr;
        bytes.extend_from_slice(&rr);
        assert!(validate_compound(&bytes).is_ok());
    }
    #[test]
    fn rejects_short_typed_feedback() {
        let bytes = [0x81, 205, 0, 1, 0, 0, 0, 0];
        assert!(validate_compound(&bytes).is_err());
    }

    #[test]
    fn rejects_empty_nack_and_fir_and_nonzero_fir_media_ssrc() {
        let nack = [0x81, 205, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(RtcpPacket::parse(&nack).unwrap().nack().is_err());
        let fir = [
            0x84, 206, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0,
        ];
        assert_eq!(
            RtcpPacket::parse(&fir)
                .unwrap()
                .fir()
                .unwrap()
                .unwrap()
                .entries()
                .count(),
            1
        );
        let mut nonzero = fir;
        nonzero[11] = 1;
        assert!(RtcpPacket::parse(&nonzero).unwrap().fir().is_err());
    }

    #[test]
    fn reports_feedback_and_pli_fields_are_typed() {
        let mut report = [0u8; 24];
        report[5..8].copy_from_slice(&[0xff, 0xff, 0xff]);
        assert_eq!(ReportBlock::parse(&report).unwrap().cumulative_lost(), -1);
        let nack = [
            0x81, 205, 0, 3, 0, 0, 0, 1, 0, 0, 0, 2, 0x12, 0x34, 0xaa, 0x55,
        ];
        let pairs: Vec<_> = RtcpPacket::parse(&nack)
            .unwrap()
            .nack()
            .unwrap()
            .unwrap()
            .pairs()
            .collect();
        assert_eq!(pairs, vec![(0x1234, 0xaa55)]);
        let pli = [0x81, 206, 0, 2, 0, 0, 0, 1, 0, 0, 0, 2];
        assert!(RtcpPacket::parse(&pli).unwrap().pli().unwrap().is_some());
    }

    #[test]
    fn sdes_items_and_bye_reason_padding_are_typed() {
        let sdes = [0x81, 202, 0, 3, 0, 0, 0, 9, 1, 3, b'a', b'b', b'c', 0, 0, 0];
        let packet = RtcpPacket::parse(&sdes).unwrap();
        let chunk = packet.sdes().unwrap().unwrap().chunks().next().unwrap();
        let item = chunk.items().next().unwrap();
        assert_eq!(item.kind(), 1);
        assert_eq!(item.value(), b"abc");
        let mut malformed = sdes;
        malformed[14] = 1;
        assert!(RtcpPacket::parse(&malformed).unwrap().sdes().is_err());

        let multiple = [
            0x82, 202, 0, 4, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(
            RtcpPacket::parse(&multiple)
                .unwrap()
                .sdes()
                .unwrap()
                .unwrap()
                .chunks()
                .count(),
            2
        );

        let bye = [0x81, 203, 0, 2, 0, 0, 0, 9, 3, b'b', b'y', b'e'];
        let packet = RtcpPacket::parse(&bye).unwrap();
        assert_eq!(packet.bye().unwrap().unwrap().reason(), Some(&b"bye"[..]));
    }

    #[test]
    fn twcc_accepts_non_aligned_vector_and_iterates_deltas_once() {
        let mut bytes = vec![0xaf, 205, 0, 6];
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&[0x10, 0, 0x00, 3, 1, 2, 3, 7]);
        bytes.extend_from_slice(&[0xd2, 0, 0xfe, 0xff, 0xfe, 0, 0, 3]);
        let packet = RtcpCompound::parse(&bytes)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let twcc = packet.twcc().unwrap().unwrap();
        assert_eq!(twcc.reference_time().ticks(), 0x010203);
        assert_eq!(twcc.feedback_count(), 7);
        let statuses: Vec<_> = twcc.statuses().collect::<Result<_, _>>().unwrap();
        assert_eq!(statuses.len(), 3);
        assert_eq!(
            statuses[0],
            TwccPacketStatus::Received {
                sequence: 0x1000,
                delta: TwccRecvDelta(-2)
            }
        );
        assert_eq!(
            statuses[1],
            TwccPacketStatus::NotReceived { sequence: 0x1001 }
        );
        assert_eq!(
            statuses[2],
            TwccPacketStatus::Received {
                sequence: 0x1002,
                delta: TwccRecvDelta(-2)
            }
        );
        let mut unexplained = bytes;
        unexplained[0] = 0x8f;
        let packet = RtcpCompound::parse(&unexplained)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert!(packet.twcc().is_err());
    }

    #[test]
    fn twcc_one_bit_vector_handles_sequence_wrap_and_signed_deltas() {
        let mut bytes = vec![0xaf, 205, 0, 6];
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&[0xff, 0xfe, 0, 3, 0, 0, 1, 2, 0xb8, 0, 0x80, 0xff, 1]);
        bytes.extend_from_slice(&[0, 0, 3]);
        let packet = RtcpCompound::parse(&bytes)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(
            validate_twcc_chunks(&[0xff, 0xfe, 0, 3, 0, 0, 1, 2, 0xb8, 0, 0x80, 0xff, 1], 3),
            Ok((10, 3))
        );
        let statuses: Vec<_> = packet
            .twcc()
            .unwrap()
            .unwrap()
            .statuses()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            statuses[0],
            TwccPacketStatus::Received {
                sequence: 0xfffe,
                delta: TwccRecvDelta(-128)
            }
        );
        assert_eq!(
            statuses[1],
            TwccPacketStatus::Received {
                sequence: 0xffff,
                delta: TwccRecvDelta(-1)
            }
        );
        assert_eq!(
            statuses[2],
            TwccPacketStatus::Received {
                sequence: 0,
                delta: TwccRecvDelta(1)
            }
        );
    }
}
