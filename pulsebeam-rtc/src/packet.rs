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

#[derive(Clone, Debug)]
pub struct RtpPacketView<'a> {
    bytes: &'a [u8],
    provenance: PacketProvenance,
    header: Range<usize>,
    payload: Range<usize>,
    extension: Option<HeaderExtensionLocation>,
}

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
}

#[derive(Clone, Debug)]
pub struct RtcpPacketView<'a> {
    bytes: &'a [u8],
    range: Range<usize>,
    report_count: u8,
    packet_type: u8,
}

impl<'a> RtcpPacketView<'a> {
    pub fn bytes(&self) -> &'a [u8] {
        debug_assert!(self.range.end <= self.bytes.len());
        &self.bytes[self.range.clone()]
    }

    pub const fn report_count(self) -> u8 {
        self.report_count
    }

    pub const fn packet_type(self) -> u8 {
        self.packet_type
    }
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
        let packet_types: Vec<_> = rtcp.packets().map(RtcpPacketView::packet_type).collect();

        assert_eq!(packet_types, [200, 201]);
    }
}
