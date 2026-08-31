use std::{net::SocketAddr, ops::Range, time::Instant};

use crate::{NegotiatedMediaSection, PacketId, StreamId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportProtocol {
    Udp,
    Tcp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportMetadata {
    protocol: TransportProtocol,
    source: SocketAddr,
    destination: SocketAddr,
}

impl TransportMetadata {
    pub const fn new(
        protocol: TransportProtocol,
        source: SocketAddr,
        destination: SocketAddr,
    ) -> Self {
        Self {
            protocol,
            source,
            destination,
        }
    }

    pub const fn protocol(self) -> TransportProtocol {
        self.protocol
    }

    pub const fn source(self) -> SocketAddr {
        self.source
    }

    pub const fn destination(self) -> SocketAddr {
        self.destination
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketProvenance {
    received_at: Instant,
    transport: TransportMetadata,
    packet_id: PacketId,
    stream_id: Option<StreamId>,
}

impl PacketProvenance {
    pub const fn new(
        received_at: Instant,
        transport: TransportMetadata,
        packet_id: PacketId,
    ) -> Self {
        Self {
            received_at,
            transport,
            packet_id,
            stream_id: None,
        }
    }

    pub const fn received_at(self) -> Instant {
        self.received_at
    }

    pub const fn transport(self) -> TransportMetadata {
        self.transport
    }

    pub const fn packet_id(self) -> PacketId {
        self.packet_id
    }

    pub const fn stream_id(self) -> Option<StreamId> {
        self.stream_id
    }

    pub const fn with_stream(self, stream_id: StreamId) -> Self {
        Self {
            received_at: self.received_at,
            transport: self.transport,
            packet_id: self.packet_id,
            stream_id: Some(stream_id),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct IngressPacket<'a> {
    bytes: &'a [u8],
    provenance: PacketProvenance,
}

impl<'a> IngressPacket<'a> {
    pub const fn new(bytes: &'a [u8], provenance: PacketProvenance) -> Self {
        Self { bytes, provenance }
    }

    pub const fn bytes(self) -> &'a [u8] {
        self.bytes
    }

    pub const fn provenance(self) -> PacketProvenance {
        self.provenance
    }

    pub fn parse(self) -> Result<PacketView<'a>, PacketError> {
        let first = *self.bytes.first().ok_or(PacketError::Empty)?;
        if first >> 6 != 2 {
            return Err(PacketError::UnsupportedVersion(first >> 6));
        }

        let second = *self.bytes.get(1).ok_or(PacketError::Truncated)?;
        if matches!(second, 192..=223) {
            CompoundRtcpView::parse(self.bytes, self.provenance).map(PacketView::Rtcp)
        } else {
            RtpPacketView::parse(self.bytes, self.provenance).map(PacketView::Rtp)
        }
    }
}

#[derive(Clone, Debug)]
pub enum PacketView<'a> {
    Rtp(RtpPacketView<'a>),
    Rtcp(CompoundRtcpView<'a>),
}

impl<'a> PacketView<'a> {
    pub const fn provenance(&self) -> PacketProvenance {
        match self {
            Self::Rtp(packet) => packet.provenance(),
            Self::Rtcp(packet) => packet.provenance(),
        }
    }

    pub const fn with_stream(self, stream_id: StreamId) -> Self {
        match self {
            Self::Rtp(packet) => Self::Rtp(packet.with_stream(stream_id)),
            Self::Rtcp(packet) => Self::Rtcp(packet.with_stream(stream_id)),
        }
    }
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum PacketError {
    #[error("packet is empty")]
    Empty,
    #[error("packet is truncated")]
    Truncated,
    #[error("unsupported RTP/RTCP version {0}")]
    UnsupportedVersion(u8),
    #[error("invalid RTP padding")]
    InvalidRtpPadding,
    #[error("invalid RTP header extension")]
    InvalidRtpExtension,
    #[error("invalid RTCP packet length")]
    InvalidRtcpLength,
    #[error("invalid RTCP packet type {0}")]
    InvalidRtcpType(u8),
}

#[derive(Clone, Debug)]
struct HeaderExtensionLocation {
    profile: u16,
    data: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RtpExtensionEntry {
    id: u8,
    value: Range<usize>,
}

impl RtpExtensionEntry {
    pub(crate) const fn id(&self) -> u8 {
        self.id
    }

    pub(crate) fn value(&self) -> Range<usize> {
        self.value.clone()
    }
}

#[derive(Clone, Debug)]
pub struct RtpPacketView<'a> {
    bytes: &'a [u8],
    provenance: PacketProvenance,
    header: Range<usize>,
    payload: Range<usize>,
    extension: Option<HeaderExtensionLocation>,
}

#[allow(
    clippy::indexing_slicing,
    reason = "RTP structure is bounds-checked once before its fixed fields and ranges are accessed"
)]
impl<'a> RtpPacketView<'a> {
    fn parse(bytes: &'a [u8], provenance: PacketProvenance) -> Result<Self, PacketError> {
        let fixed = bytes.get(..12).ok_or(PacketError::Truncated)?;
        debug_assert_eq!(fixed.len(), 12);

        let first = fixed[0];
        let csrc_count = usize::from(first & 0x0f);
        let has_extension = first & 0x10 != 0;
        let has_padding = first & 0x20 != 0;
        let csrc_bytes = csrc_count.checked_mul(4).ok_or(PacketError::Truncated)?;
        let mut header_end = 12usize
            .checked_add(csrc_bytes)
            .ok_or(PacketError::Truncated)?;
        if bytes.get(..header_end).is_none() {
            return Err(PacketError::Truncated);
        }

        let extension = if has_extension {
            let extension_header = bytes
                .get(header_end..header_end.checked_add(4).ok_or(PacketError::Truncated)?)
                .ok_or(PacketError::Truncated)?;
            let profile = u16::from_be_bytes([extension_header[0], extension_header[1]]);
            let words = usize::from(u16::from_be_bytes([
                extension_header[2],
                extension_header[3],
            ]));
            let extension_len = words
                .checked_mul(4)
                .ok_or(PacketError::InvalidRtpExtension)?;
            let data_start = header_end
                .checked_add(4)
                .ok_or(PacketError::InvalidRtpExtension)?;
            let data_end = data_start
                .checked_add(extension_len)
                .ok_or(PacketError::InvalidRtpExtension)?;
            if bytes.get(data_start..data_end).is_none() {
                return Err(PacketError::Truncated);
            }
            header_end = data_end;
            Some(HeaderExtensionLocation {
                profile,
                data: data_start..data_end,
            })
        } else {
            None
        };

        let mut payload_end = bytes.len();
        if has_padding {
            let padding = usize::from(*bytes.last().ok_or(PacketError::Truncated)?);
            if padding == 0 || padding > payload_end.saturating_sub(header_end) {
                return Err(PacketError::InvalidRtpPadding);
            }
            payload_end = payload_end
                .checked_sub(padding)
                .ok_or(PacketError::InvalidRtpPadding)?;
        }
        if payload_end < header_end {
            return Err(PacketError::Truncated);
        }
        debug_assert!(header_end <= payload_end);
        debug_assert!(payload_end <= bytes.len());

        Ok(Self {
            bytes,
            provenance,
            header: 0..header_end,
            payload: header_end..payload_end,
            extension,
        })
    }

    pub const fn provenance(&self) -> PacketProvenance {
        self.provenance
    }

    pub const fn with_stream(mut self, stream_id: StreamId) -> Self {
        self.provenance = self.provenance.with_stream(stream_id);
        self
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn header(&self) -> &'a [u8] {
        debug_assert!(self.header.end <= self.bytes.len());
        &self.bytes[self.header.clone()]
    }

    pub fn payload(&self) -> &'a [u8] {
        debug_assert!(self.payload.end <= self.bytes.len());
        &self.bytes[self.payload.clone()]
    }

    pub(crate) fn payload_range(&self) -> Range<usize> {
        self.payload.clone()
    }

    pub(crate) fn extension_entries(&self) -> Result<Box<[RtpExtensionEntry]>, PacketError> {
        let Some(location) = self.extension.as_ref() else {
            return Ok(Box::new([]));
        };
        let data = self
            .bytes
            .get(location.data.clone())
            .ok_or(PacketError::InvalidRtpExtension)?;
        match location.profile {
            0xbede => one_byte_extension_entries(data, location.data.start),
            0x1000..=0x10ff => two_byte_extension_entries(data, location.data.start),
            _ => Ok(Box::new([])),
        }
    }

    pub fn marker(&self) -> bool {
        self.bytes[1] & 0x80 != 0
    }

    pub fn payload_type(&self) -> u8 {
        self.bytes[1] & 0x7f
    }

    pub fn sequence_number(&self) -> u16 {
        u16::from_be_bytes([self.bytes[2], self.bytes[3]])
    }

    pub fn timestamp(&self) -> u32 {
        u32::from_be_bytes([self.bytes[4], self.bytes[5], self.bytes[6], self.bytes[7]])
    }

    pub fn ssrc(&self) -> u32 {
        u32::from_be_bytes([self.bytes[8], self.bytes[9], self.bytes[10], self.bytes[11]])
    }

    pub fn header_extension(
        &self,
        section: &NegotiatedMediaSection,
        extension_id: u8,
    ) -> Result<Option<HeaderExtensionValue<'a>>, PacketError> {
        if !section
            .header_extensions()
            .iter()
            .any(|extension| extension.id() == extension_id)
        {
            return Ok(None);
        }

        self.header_extension_by_id(extension_id)
    }

    pub fn header_extension_by_id(
        &self,
        extension_id: u8,
    ) -> Result<Option<HeaderExtensionValue<'a>>, PacketError> {
        debug_assert_ne!(
            extension_id, 0,
            "RTP header extension identifiers start at one"
        );

        let Some(location) = self.extension.as_ref() else {
            return Ok(None);
        };
        let data = self
            .bytes
            .get(location.data.clone())
            .ok_or(PacketError::InvalidRtpExtension)?;

        match location.profile {
            0xbede => one_byte_extension(data, extension_id),
            0x1000..=0x10ff => two_byte_extension(data, extension_id),
            _ => Ok(None),
        }
    }

    pub fn rewrite_header_extension(
        &self,
        bytes: &mut [u8],
        section: &NegotiatedMediaSection,
        source_id: u8,
        destination_id: u8,
        value: Option<&[u8]>,
    ) -> Result<(), PacketError> {
        if bytes.len() != self.bytes.len()
            || !section
                .header_extensions()
                .iter()
                .any(|extension| extension.id() == source_id)
        {
            return Err(PacketError::InvalidRtpExtension);
        }
        let Some(location) = self.extension.as_ref() else {
            return Ok(());
        };
        match location.profile {
            0xbede => rewrite_one_byte_extension(
                bytes,
                location.data.clone(),
                source_id,
                destination_id,
                value,
            ),
            0x1000..=0x10ff => rewrite_two_byte_extension(
                bytes,
                location.data.clone(),
                source_id,
                destination_id,
                value,
            ),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderExtensionValue<'a> {
    id: u8,
    value: &'a [u8],
}

impl<'a> HeaderExtensionValue<'a> {
    pub const fn id(self) -> u8 {
        self.id
    }

    pub const fn value(self) -> &'a [u8] {
        self.value
    }
}

fn one_byte_extension(
    data: &[u8],
    extension_id: u8,
) -> Result<Option<HeaderExtensionValue<'_>>, PacketError> {
    let mut offset = 0usize;
    while offset < data.len() {
        let entry = *data.get(offset).ok_or(PacketError::InvalidRtpExtension)?;
        offset = offset
            .checked_add(1)
            .ok_or(PacketError::InvalidRtpExtension)?;
        if entry == 0 {
            continue;
        }
        let id = entry >> 4;
        if id == 15 {
            break;
        }
        let length = usize::from((entry & 0x0f).saturating_add(1));
        let end = offset
            .checked_add(length)
            .ok_or(PacketError::InvalidRtpExtension)?;
        let value = data
            .get(offset..end)
            .ok_or(PacketError::InvalidRtpExtension)?;
        offset = end;
        if id == extension_id {
            return Ok(Some(HeaderExtensionValue { id, value }));
        }
    }
    Ok(None)
}

fn two_byte_extension(
    data: &[u8],
    extension_id: u8,
) -> Result<Option<HeaderExtensionValue<'_>>, PacketError> {
    let mut offset = 0usize;
    while offset < data.len() {
        let id = *data.get(offset).ok_or(PacketError::InvalidRtpExtension)?;
        offset = offset
            .checked_add(1)
            .ok_or(PacketError::InvalidRtpExtension)?;
        if id == 0 {
            continue;
        }
        let length = usize::from(*data.get(offset).ok_or(PacketError::InvalidRtpExtension)?);
        offset = offset
            .checked_add(1)
            .ok_or(PacketError::InvalidRtpExtension)?;
        let end = offset
            .checked_add(length)
            .ok_or(PacketError::InvalidRtpExtension)?;
        let value = data
            .get(offset..end)
            .ok_or(PacketError::InvalidRtpExtension)?;
        offset = end;
        if id == extension_id {
            return Ok(Some(HeaderExtensionValue { id, value }));
        }
    }
    Ok(None)
}

fn one_byte_extension_entries(
    data: &[u8],
    base: usize,
) -> Result<Box<[RtpExtensionEntry]>, PacketError> {
    let mut entries = Vec::new();
    let mut offset = 0usize;
    while offset < data.len() {
        let entry = *data.get(offset).ok_or(PacketError::InvalidRtpExtension)?;
        offset = offset
            .checked_add(1)
            .ok_or(PacketError::InvalidRtpExtension)?;
        if entry == 0 {
            continue;
        }
        let id = entry >> 4;
        if id == 15 {
            break;
        }
        let length = usize::from((entry & 0x0f).saturating_add(1));
        let end = offset
            .checked_add(length)
            .ok_or(PacketError::InvalidRtpExtension)?;
        if data.get(offset..end).is_none() {
            return Err(PacketError::InvalidRtpExtension);
        }
        entries.push(RtpExtensionEntry {
            id,
            value: base.saturating_add(offset)..base.saturating_add(end),
        });
        offset = end;
    }
    Ok(entries.into_boxed_slice())
}

fn two_byte_extension_entries(
    data: &[u8],
    base: usize,
) -> Result<Box<[RtpExtensionEntry]>, PacketError> {
    let mut entries = Vec::new();
    let mut offset = 0usize;
    while offset < data.len() {
        let id = *data.get(offset).ok_or(PacketError::InvalidRtpExtension)?;
        offset = offset
            .checked_add(1)
            .ok_or(PacketError::InvalidRtpExtension)?;
        if id == 0 {
            continue;
        }
        let length = usize::from(*data.get(offset).ok_or(PacketError::InvalidRtpExtension)?);
        offset = offset
            .checked_add(1)
            .ok_or(PacketError::InvalidRtpExtension)?;
        let end = offset
            .checked_add(length)
            .ok_or(PacketError::InvalidRtpExtension)?;
        if data.get(offset..end).is_none() {
            return Err(PacketError::InvalidRtpExtension);
        }
        entries.push(RtpExtensionEntry {
            id,
            value: base.saturating_add(offset)..base.saturating_add(end),
        });
        offset = end;
    }
    Ok(entries.into_boxed_slice())
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "extension ranges are structurally bounds-checked before every rewrite"
)]
fn rewrite_one_byte_extension(
    bytes: &mut [u8],
    data: Range<usize>,
    source_id: u8,
    destination_id: u8,
    value: Option<&[u8]>,
) -> Result<(), PacketError> {
    if !(1..15).contains(&destination_id) {
        return Err(PacketError::InvalidRtpExtension);
    }
    let mut offset = data.start;
    while offset < data.end {
        let entry = *bytes.get(offset).ok_or(PacketError::InvalidRtpExtension)?;
        offset = offset
            .checked_add(1)
            .ok_or(PacketError::InvalidRtpExtension)?;
        if entry == 0 {
            continue;
        }
        let id = entry >> 4;
        if id == 15 {
            break;
        }
        let length = usize::from((entry & 0x0f).saturating_add(1));
        let end = offset
            .checked_add(length)
            .ok_or(PacketError::InvalidRtpExtension)?;
        let entry_value = bytes
            .get_mut(offset..end)
            .ok_or(PacketError::InvalidRtpExtension)?;
        if id == source_id {
            if let Some(value) = value {
                if value.len() != entry_value.len() {
                    return Err(PacketError::InvalidRtpExtension);
                }
                entry_value.copy_from_slice(value);
            }
            bytes[offset - 1] = (destination_id << 4) | (entry & 0x0f);
            return Ok(());
        }
        offset = end;
    }
    Ok(())
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "extension ranges are structurally bounds-checked before every rewrite"
)]
fn rewrite_two_byte_extension(
    bytes: &mut [u8],
    data: Range<usize>,
    source_id: u8,
    destination_id: u8,
    value: Option<&[u8]>,
) -> Result<(), PacketError> {
    if destination_id == 0 {
        return Err(PacketError::InvalidRtpExtension);
    }
    let mut offset = data.start;
    while offset < data.end {
        let id = *bytes.get(offset).ok_or(PacketError::InvalidRtpExtension)?;
        offset = offset
            .checked_add(1)
            .ok_or(PacketError::InvalidRtpExtension)?;
        if id == 0 {
            continue;
        }
        let length = usize::from(*bytes.get(offset).ok_or(PacketError::InvalidRtpExtension)?);
        offset = offset
            .checked_add(1)
            .ok_or(PacketError::InvalidRtpExtension)?;
        let end = offset
            .checked_add(length)
            .ok_or(PacketError::InvalidRtpExtension)?;
        let entry_value = bytes
            .get_mut(offset..end)
            .ok_or(PacketError::InvalidRtpExtension)?;
        if id == source_id {
            if let Some(value) = value {
                if value.len() != entry_value.len() {
                    return Err(PacketError::InvalidRtpExtension);
                }
                entry_value.copy_from_slice(value);
            }
            bytes[offset - 2] = destination_id;
            return Ok(());
        }
        offset = end;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct CompoundRtcpView<'a> {
    bytes: &'a [u8],
    provenance: PacketProvenance,
}

impl<'a> CompoundRtcpView<'a> {
    fn parse(bytes: &'a [u8], provenance: PacketProvenance) -> Result<Self, PacketError> {
        let mut offset = 0usize;
        while offset < bytes.len() {
            let packet = parse_rtcp_packet(bytes, offset)?;
            offset = packet.range.end;
        }
        debug_assert_eq!(offset, bytes.len());
        Ok(Self { bytes, provenance })
    }

    pub const fn provenance(&self) -> PacketProvenance {
        self.provenance
    }

    pub const fn with_stream(mut self, stream_id: StreamId) -> Self {
        self.provenance = self.provenance.with_stream(stream_id);
        self
    }

    pub fn packets(&self) -> RtcpPacketIter<'a> {
        RtcpPacketIter {
            bytes: self.bytes,
            offset: 0,
        }
    }

    pub fn nacks(&self) -> Result<Vec<RtcpNack>, PacketError> {
        self.packets().try_fold(Vec::new(), |mut nacks, packet| {
            if let Some(nack) = packet.nack()? {
                nacks.push(nack);
            }
            Ok(nacks)
        })
    }
}

#[derive(Clone, Debug)]
pub struct RtcpPacketView<'a> {
    bytes: &'a [u8],
    range: Range<usize>,
    report_count: u8,
    packet_type: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SenderReport {
    ssrc: u32,
    ntp_timestamp: u64,
    rtp_timestamp: u32,
    packet_count: u32,
    octet_count: u32,
}

impl SenderReport {
    pub const fn ssrc(self) -> u32 {
        self.ssrc
    }
    pub const fn ntp_timestamp(self) -> u64 {
        self.ntp_timestamp
    }
    pub const fn rtp_timestamp(self) -> u32 {
        self.rtp_timestamp
    }
    pub const fn packet_count(self) -> u32 {
        self.packet_count
    }
    pub const fn octet_count(self) -> u32 {
        self.octet_count
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcpFeedback {
    sender_ssrc: u32,
    media_ssrc: u32,
    packet_type: u8,
    format: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcpNack {
    media_ssrc: u32,
    sequences: Box<[u16]>,
}

impl RtcpNack {
    pub const fn media_ssrc(&self) -> u32 {
        self.media_ssrc
    }

    pub fn sequences(&self) -> &[u16] {
        &self.sequences
    }
}

impl RtcpFeedback {
    pub const fn sender_ssrc(self) -> u32 {
        self.sender_ssrc
    }
    pub const fn media_ssrc(self) -> u32 {
        self.media_ssrc
    }
    pub const fn packet_type(self) -> u8 {
        self.packet_type
    }
    pub const fn format(self) -> u8 {
        self.format
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "RTCP packet ranges are structurally validated before access"
)]
impl<'a> RtcpPacketView<'a> {
    pub fn bytes(&self) -> &'a [u8] {
        debug_assert!(self.range.end <= self.bytes.len());
        &self.bytes[self.range.clone()]
    }

    pub const fn report_count(&self) -> u8 {
        self.report_count
    }

    pub const fn packet_type(&self) -> u8 {
        self.packet_type
    }

    pub fn sender_ssrc(&self) -> Result<u32, PacketError> {
        read_u32(self.bytes(), 4)
    }

    pub fn sender_report(&self) -> Result<Option<SenderReport>, PacketError> {
        if self.packet_type != 200 {
            return Ok(None);
        }
        let bytes = self.bytes();
        if bytes.len() < 28 {
            return Err(PacketError::Truncated);
        }
        Ok(Some(SenderReport {
            ssrc: read_u32(bytes, 4)?,
            ntp_timestamp: (u64::from(read_u32(bytes, 8)?) << 32) | u64::from(read_u32(bytes, 12)?),
            rtp_timestamp: read_u32(bytes, 16)?,
            packet_count: read_u32(bytes, 20)?,
            octet_count: read_u32(bytes, 24)?,
        }))
    }

    pub fn feedback(&self) -> Result<Option<RtcpFeedback>, PacketError> {
        if !matches!(self.packet_type, 205 | 206) {
            return Ok(None);
        }
        let bytes = self.bytes();
        if bytes.len() < 12 {
            return Err(PacketError::Truncated);
        }
        Ok(Some(RtcpFeedback {
            sender_ssrc: read_u32(bytes, 4)?,
            media_ssrc: read_u32(bytes, 8)?,
            packet_type: self.packet_type,
            format: self.report_count,
        }))
    }

    pub fn nack(&self) -> Result<Option<RtcpNack>, PacketError> {
        if self.packet_type != 205 || self.report_count != 1 {
            return Ok(None);
        }
        let bytes = self.bytes();
        let media_ssrc = read_u32(bytes, 8)?;
        let fci = bytes.get(12..).ok_or(PacketError::Truncated)?;
        if !fci.len().is_multiple_of(4) {
            return Err(PacketError::InvalidRtcpLength);
        }
        let mut sequences = Vec::with_capacity(fci.len().saturating_mul(17).saturating_div(4));
        for chunk in fci.chunks_exact(4) {
            let pid = u16::from_be_bytes([chunk[0], chunk[1]]);
            let blp = u16::from_be_bytes([chunk[2], chunk[3]]);
            sequences.push(pid);
            for bit in 0..16u16 {
                if blp & (1u16 << bit) != 0 {
                    sequences.push(pid.wrapping_add(bit.wrapping_add(1)));
                }
            }
        }
        Ok(Some(RtcpNack {
            media_ssrc,
            sequences: sequences.into_boxed_slice(),
        }))
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "the four-byte field range is validated before conversion"
)]
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PacketError> {
    let end = offset.checked_add(4).ok_or(PacketError::Truncated)?;
    let value = bytes.get(offset..end).ok_or(PacketError::Truncated)?;
    debug_assert_eq!(value.len(), 4);
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

pub struct RtcpPacketIter<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for RtcpPacketIter<'a> {
    type Item = RtcpPacketView<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset == self.bytes.len() {
            return None;
        }
        let packet = parse_rtcp_packet(self.bytes, self.offset).ok()?;
        self.offset = packet.range.end;
        Some(packet)
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "the RTCP fixed header range is validated before fields are read"
)]
fn parse_rtcp_packet(bytes: &[u8], offset: usize) -> Result<RtcpPacketView<'_>, PacketError> {
    let header_end = offset
        .checked_add(4)
        .ok_or(PacketError::InvalidRtcpLength)?;
    let header = bytes
        .get(offset..header_end)
        .ok_or(PacketError::Truncated)?;
    if header[0] >> 6 != 2 {
        return Err(PacketError::UnsupportedVersion(header[0] >> 6));
    }
    let packet_type = header[1];
    if !matches!(packet_type, 192..=223) {
        return Err(PacketError::InvalidRtcpType(packet_type));
    }
    let words = usize::from(u16::from_be_bytes([header[2], header[3]]));
    let size = words
        .checked_add(1)
        .and_then(|words| words.checked_mul(4))
        .ok_or(PacketError::InvalidRtcpLength)?;
    let end = offset
        .checked_add(size)
        .ok_or(PacketError::InvalidRtcpLength)?;
    if bytes.get(offset..end).is_none() {
        return Err(PacketError::Truncated);
    }
    debug_assert!(end > offset);
    Ok(RtcpPacketView {
        bytes,
        range: offset..end,
        report_count: header[0] & 0x1f,
        packet_type,
    })
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    reason = "test packet timestamps intentionally use checked small durations"
)]
mod tests {
    use std::{net::Ipv4Addr, time::Duration};

    use super::*;
    use crate::{DtlsFingerprint, IceCredentials, ServerTransport, negotiate};

    fn provenance() -> PacketProvenance {
        let source = SocketAddr::from((Ipv4Addr::LOCALHOST, 5000));
        let destination = SocketAddr::from((Ipv4Addr::LOCALHOST, 6000));
        PacketProvenance::new(
            Instant::now() + Duration::from_millis(1),
            TransportMetadata::new(TransportProtocol::Udp, source, destination),
            PacketId::new(9),
        )
    }

    fn negotiated_section() -> crate::NegotiatedMediaSection {
        let ice = IceCredentials::new("localufrag".to_owned(), "localpassword".to_owned())
            .expect("valid ICE credentials");
        let fingerprint = DtlsFingerprint::new("sha-256".to_owned(), Box::new([9; 32]))
            .expect("valid fingerprint");
        let server = ServerTransport::new(7, ice, fingerprint, Box::new([]));
        let offer = "v=0\r\n\
                     o=- 1 2 IN IP4 127.0.0.1\r\n\
                     s=-\r\n\
                     t=0 0\r\n\
                     a=group:BUNDLE 0\r\n\
                     a=ice-ufrag:remoteufrag\r\n\
                     a=ice-pwd:remotepassword\r\n\
                     a=fingerprint:sha-256 01:02:03:04\r\n\
                     a=setup:actpass\r\n\
                     m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                     c=IN IP4 0.0.0.0\r\n\
                     a=mid:0\r\n\
                     a=sendonly\r\n\
                     a=rtcp-mux\r\n\
                     a=rtpmap:111 opus/48000/2\r\n\
                     a=extmap:3 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n";

        negotiate(offer, &server)
            .expect("accepted offer")
            .session()
            .media_sections()[0]
            .clone()
    }

    #[test]
    fn packet_view_preserves_provenance_through_stream_resolution() {
        let bytes = [0x80, 111, 0, 7, 0, 0, 0, 9, 0, 0, 0, 10];
        let packet = IngressPacket::new(&bytes, provenance())
            .parse()
            .expect("RTP packet");
        let packet = packet.with_stream(StreamId::new(12));

        assert_eq!(packet.provenance().packet_id(), PacketId::new(9));
        assert_eq!(packet.provenance().stream_id(), Some(StreamId::new(12)));
    }

    #[test]
    fn packet_view_rejects_truncated_header_extension() {
        let bytes = [0x90, 111, 0, 7, 0, 0, 0, 9, 0, 0, 0, 10, 0xbe, 0xde, 0, 1];
        let error = IngressPacket::new(&bytes, provenance())
            .parse()
            .expect_err("truncated extension");

        assert_eq!(error, PacketError::Truncated);
    }

    #[test]
    fn packet_view_reads_negotiated_extension_lazily() {
        let bytes = [
            0x90, 111, 0, 7, 0, 0, 0, 9, 0, 0, 0, 10, 0xbe, 0xde, 0, 1, 0x31, 0xaa, 0xbb, 0,
        ];
        let packet = IngressPacket::new(&bytes, provenance())
            .parse()
            .expect("RTP packet");
        let PacketView::Rtp(rtp) = packet else {
            panic!("RTP packet");
        };
        let value = rtp
            .header_extension(&negotiated_section(), 3)
            .expect("valid extension")
            .expect("negotiated extension");

        assert_eq!(value.id(), 3);
        assert_eq!(value.value(), [0xaa, 0xbb]);
    }

    #[test]
    fn compound_rtcp_iterates_without_reparsing_the_datagram() {
        let bytes = [0x80, 200, 0, 1, 0, 0, 0, 0, 0x80, 201, 0, 1, 0, 0, 0, 0];
        let packet = IngressPacket::new(&bytes, provenance())
            .parse()
            .expect("compound RTCP");
        let PacketView::Rtcp(rtcp) = packet else {
            panic!("RTCP packet");
        };
        let packet_types: Vec<_> = rtcp.packets().map(|packet| packet.packet_type()).collect();

        assert_eq!(packet_types, [200, 201]);
    }

    #[test]
    fn compound_rtcp_exposes_nack_sequences_structurally() {
        let bytes = [
            0x81,
            205,
            0,
            3,
            0,
            0,
            0,
            1,
            0,
            0,
            0,
            9,
            0,
            10,
            0,
            0b0000_0101,
        ];
        let packet = IngressPacket::new(&bytes, provenance())
            .parse()
            .expect("RTCP packet");
        let PacketView::Rtcp(rtcp) = packet else {
            panic!("RTCP packet");
        };

        let nacks = rtcp.nacks().expect("NACK feedback");

        assert_eq!(nacks.len(), 1);
        assert_eq!(nacks[0].media_ssrc(), 9);
        assert_eq!(nacks[0].sequences(), [10, 11, 13]);
    }
}
