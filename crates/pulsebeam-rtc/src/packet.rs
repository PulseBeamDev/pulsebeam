use std::{fmt, ops::Range};

const MAX_RTP_CSRC: usize = 15;
const MAX_RTP_EXTENSION_BYTES: usize = 16 * 1024;
const MAX_RTCP_PACKET_BYTES: usize = 64 * 1024;
const MAX_RTCP_REPORT_BLOCKS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PacketError {
    TooShort,
    InvalidVersion,
    InvalidPadding,
    InvalidExtension,
    InvalidLength,
    TooManyItems,
    InvalidValue,
    MalformedH264,
    MalformedOpus,
    InvalidDependencyDescriptor,
    InvalidVideoLayerAllocation,
    InvalidCaptureTime,
    InvalidAudioLevel,
    InvalidPlayoutDelay,
}

fn read_u8(bytes: &[u8], offset: usize) -> Result<u8, PacketError> {
    bytes.get(offset).copied().ok_or(PacketError::TooShort)
}
fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, PacketError> {
    let end = offset.checked_add(2).ok_or(PacketError::InvalidLength)?;
    bytes
        .get(offset..end)
        .and_then(|value| value.try_into().ok())
        .map(u16::from_be_bytes)
        .ok_or(PacketError::TooShort)
}
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PacketError> {
    let end = offset.checked_add(4).ok_or(PacketError::InvalidLength)?;
    bytes
        .get(offset..end)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or(PacketError::TooShort)
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooShort => "packet is too short",
            Self::InvalidVersion => "packet version is not two",
            Self::InvalidPadding => "invalid packet padding",
            Self::InvalidExtension => "invalid RTP extension",
            Self::InvalidLength => "invalid packet length",
            Self::TooManyItems => "packet contains too many items",
            Self::InvalidValue => "packet contains an invalid value",
            Self::MalformedH264 => "malformed H264 payload",
            Self::MalformedOpus => "malformed Opus payload",
            Self::InvalidDependencyDescriptor => "invalid dependency descriptor",
            Self::InvalidVideoLayerAllocation => "invalid video layer allocation",
            Self::InvalidCaptureTime => "invalid capture time",
            Self::InvalidAudioLevel => "invalid audio level",
            Self::InvalidPlayoutDelay => "invalid playout delay",
        })
    }
}

impl std::error::Error for PacketError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtpPacket<'a> {
    bytes: &'a [u8],
    payload: Range<usize>,
    extensions: Option<Range<usize>>,
    extension_profile: Option<u16>,
    csrc_count: u8,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    payload_type: u8,
    marker: bool,
    padding: u8,
}

impl<'a> RtpPacket<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.len() < 12 {
            return Err(PacketError::TooShort);
        }
        debug_assert!(bytes.len() >= 12);
        let first = read_u8(bytes, 0)?;
        if first >> 6 != 2 {
            return Err(PacketError::InvalidVersion);
        }
        let has_padding = first & 0x20 != 0;
        let has_extension = first & 0x10 != 0;
        let csrc_count = usize::from(first & 0x0f);
        debug_assert!(csrc_count <= MAX_RTP_CSRC);
        let header_len = 12usize
            .checked_add(
                csrc_count
                    .checked_mul(4)
                    .ok_or(PacketError::InvalidLength)?,
            )
            .ok_or(PacketError::InvalidLength)?;
        if csrc_count > MAX_RTP_CSRC || header_len > bytes.len() {
            return Err(PacketError::InvalidLength);
        }
        let (extensions, extension_profile, header_len) = if has_extension {
            let extension_header_end = header_len
                .checked_add(4)
                .ok_or(PacketError::InvalidExtension)?;
            if extension_header_end > bytes.len() {
                return Err(PacketError::InvalidExtension);
            }
            let profile = read_u16(bytes, header_len)?;
            let words = usize::from(read_u16(
                bytes,
                header_len
                    .checked_add(2)
                    .ok_or(PacketError::InvalidExtension)?,
            )?);
            let length = words.checked_mul(4).ok_or(PacketError::InvalidExtension)?;
            if length > MAX_RTP_EXTENSION_BYTES {
                return Err(PacketError::InvalidExtension);
            }
            let start = extension_header_end;
            let end = start
                .checked_add(length)
                .ok_or(PacketError::InvalidExtension)?;
            if end > bytes.len() {
                return Err(PacketError::InvalidExtension);
            }
            validate_extension_elements(
                bytes.get(start..end).ok_or(PacketError::InvalidExtension)?,
                profile,
            )?;
            (Some(start..end), Some(profile), end)
        } else {
            (None, None, header_len)
        };
        let padding = if has_padding {
            let value = *bytes.last().ok_or(PacketError::InvalidPadding)?;
            if value == 0 || usize::from(value) > bytes.len().saturating_sub(header_len) {
                return Err(PacketError::InvalidPadding);
            }
            value
        } else {
            0
        };
        let payload_end = bytes
            .len()
            .checked_sub(usize::from(padding))
            .ok_or(PacketError::InvalidPadding)?;
        if payload_end < header_len {
            return Err(PacketError::InvalidLength);
        }
        let result = Self {
            bytes,
            payload: header_len..payload_end,
            extensions,
            extension_profile,
            csrc_count: u8::try_from(csrc_count).map_err(|_| PacketError::TooManyItems)?,
            sequence: read_u16(bytes, 2)?,
            timestamp: read_u32(bytes, 4)?,
            ssrc: read_u32(bytes, 8)?,
            payload_type: read_u8(bytes, 1)? & 0x7f,
            marker: read_u8(bytes, 1)? & 0x80 != 0,
            padding,
        };
        debug_assert!(result.payload.end <= bytes.len());
        Ok(result)
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
    pub fn payload(&self) -> &'a [u8] {
        self.bytes.get(self.payload.clone()).unwrap_or_default()
    }
    pub fn payload_range(&self) -> Range<usize> {
        self.payload.clone()
    }
    pub const fn sequence(&self) -> u16 {
        self.sequence
    }
    pub const fn timestamp(&self) -> u32 {
        self.timestamp
    }
    pub const fn ssrc(&self) -> u32 {
        self.ssrc
    }
    pub const fn payload_type(&self) -> u8 {
        self.payload_type
    }
    pub const fn marker(&self) -> bool {
        self.marker
    }
    pub const fn padding(&self) -> u8 {
        self.padding
    }
    pub const fn csrc_count(&self) -> u8 {
        self.csrc_count
    }

    pub fn csrcs(&self) -> CsrcIter<'a> {
        let start = 12usize;
        let end = start.saturating_add(usize::from(self.csrc_count).saturating_mul(4));
        debug_assert!(end <= self.bytes.len());
        CsrcIter {
            bytes: self.bytes,
            offset: start,
            end,
        }
    }

    pub const fn extension_profile(&self) -> Option<u16> {
        self.extension_profile
    }

    pub fn extension_data(&self) -> Option<&'a [u8]> {
        self.extensions
            .as_ref()
            .and_then(|range| self.bytes.get(range.clone()))
    }

    pub fn extension_range(&self) -> Option<Range<usize>> {
        self.extensions.clone()
    }

    pub fn extensions(&self) -> Result<ExtensionIter<'a>, PacketError> {
        let Some(range) = &self.extensions else {
            return Ok(ExtensionIter {
                bytes: &[],
                offset: 0,
                end: 0,
                profile: None,
            });
        };
        debug_assert!(range.end <= self.bytes.len());
        Ok(ExtensionIter {
            bytes: self
                .bytes
                .get(range.clone())
                .ok_or(PacketError::InvalidExtension)?,
            offset: 0,
            end: range.len(),
            profile: self.extension_profile,
        })
    }
}

pub struct CsrcIter<'a> {
    bytes: &'a [u8],
    offset: usize,
    end: usize,
}
impl<'a> Iterator for CsrcIter<'a> {
    type Item = u32;
    fn next(&mut self) -> Option<Self::Item> {
        if self.offset == self.end {
            return None;
        }
        if self.offset.checked_add(4)? > self.end {
            return None;
        }
        let value = read_u32(self.bytes, self.offset).ok()?;
        self.offset = self.offset.checked_add(4)?;
        Some(value)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.end.saturating_sub(self.offset) / 4;
        (n, Some(n))
    }
}
impl ExactSizeIterator for CsrcIter<'_> {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpExtension<'a> {
    id: u8,
    value: &'a [u8],
}
impl<'a> RtpExtension<'a> {
    pub const fn id(&self) -> u8 {
        self.id
    }
    pub const fn value(&self) -> &'a [u8] {
        self.value
    }
}

pub struct ExtensionIter<'a> {
    bytes: &'a [u8],
    offset: usize,
    end: usize,
    profile: Option<u16>,
}
impl<'a> Iterator for ExtensionIter<'a> {
    type Item = RtpExtension<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        while self.offset < self.end {
            let byte = *self.bytes.get(self.offset)?;
            if self.profile == Some(0xBEDE) {
                self.offset = self.offset.checked_add(1)?;
                if byte == 0 {
                    continue;
                }
                let id = byte >> 4;
                let length = usize::from(byte & 0x0f).checked_add(1)?;
                if id == 15 || self.offset.checked_add(length)? > self.end {
                    self.offset = self.end;
                    return None;
                }
                let end = self.offset.checked_add(length)?;
                let value = self.bytes.get(self.offset..end)?;
                self.offset = end;
                return Some(RtpExtension { id, value });
            }
            if self
                .profile
                .is_some_and(|profile| profile & 0xfff0 == 0x1000)
            {
                self.offset = self.offset.checked_add(1)?;
                if byte == 0 {
                    continue;
                }
                let id = byte;
                let length = usize::from(*self.bytes.get(self.offset)?);
                self.offset = self.offset.checked_add(1)?;
                if id == 0 || self.offset.checked_add(length)? > self.end {
                    self.offset = self.end;
                    return None;
                }
                let end = self.offset.checked_add(length)?;
                let value = self.bytes.get(self.offset..end)?;
                self.offset = end;
                return Some(RtpExtension { id, value });
            }
            self.offset = self.end;
        }
        None
    }
}

fn validate_extension_elements(bytes: &[u8], profile: u16) -> Result<(), PacketError> {
    if profile == 0xBEDE {
        let mut offset = 0;
        while offset < bytes.len() {
            let byte = *bytes.get(offset).ok_or(PacketError::InvalidExtension)?;
            offset = offset.checked_add(1).ok_or(PacketError::InvalidExtension)?;
            if byte == 0 {
                continue;
            }
            let id = byte >> 4;
            let length = usize::from(byte & 0x0f)
                .checked_add(1)
                .ok_or(PacketError::InvalidExtension)?;
            if id == 15 {
                return Ok(());
            }
            if offset
                .checked_add(length)
                .is_none_or(|end| end > bytes.len())
            {
                return Err(PacketError::InvalidExtension);
            }
            offset = offset
                .checked_add(length)
                .ok_or(PacketError::InvalidExtension)?;
        }
    } else if profile & 0xfff0 == 0x1000 {
        let mut offset = 0;
        while offset < bytes.len() {
            let id = *bytes.get(offset).ok_or(PacketError::InvalidExtension)?;
            offset = offset.checked_add(1).ok_or(PacketError::InvalidExtension)?;
            if id == 0 {
                continue;
            }
            let length = usize::from(*bytes.get(offset).ok_or(PacketError::InvalidExtension)?);
            offset = offset.checked_add(1).ok_or(PacketError::InvalidExtension)?;
            if offset
                .checked_add(length)
                .is_none_or(|end| end > bytes.len())
            {
                return Err(PacketError::InvalidExtension);
            }
            offset = offset
                .checked_add(length)
                .ok_or(PacketError::InvalidExtension)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcpPacket<'a> {
    bytes: &'a [u8],
    packet_type: u8,
    count: u8,
    padding: u8,
}

impl<'a> RtcpPacket<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.len() < 4 {
            return Err(PacketError::TooShort);
        }
        if read_u8(bytes, 0)? >> 6 != 2 {
            return Err(PacketError::InvalidVersion);
        }
        let first = read_u8(bytes, 0)?;
        let has_padding = first & 0x20 != 0;
        let length_words = usize::from(read_u16(bytes, 2)?);
        let length = length_words
            .checked_add(1)
            .and_then(|v| v.checked_mul(4))
            .ok_or(PacketError::InvalidLength)?;
        if length != bytes.len() || length > MAX_RTCP_PACKET_BYTES {
            return Err(PacketError::InvalidLength);
        }
        let padding = if has_padding {
            let value = *bytes.last().ok_or(PacketError::InvalidPadding)?;
            let body_length = length.checked_sub(4).ok_or(PacketError::InvalidPadding)?;
            if value == 0 || usize::from(value) > body_length {
                return Err(PacketError::InvalidPadding);
            }
            value
        } else {
            0
        };
        Ok(Self {
            bytes,
            packet_type: read_u8(bytes, 1)?,
            count: first & 0x1f,
            padding,
        })
    }
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
    pub fn body(&self) -> &'a [u8] {
        let end = self.bytes.len().saturating_sub(usize::from(self.padding));
        self.bytes.get(4..end).unwrap_or_default()
    }
    pub const fn packet_type(&self) -> u8 {
        self.packet_type
    }
    pub const fn count(&self) -> u8 {
        self.count
    }
    pub const fn padding(&self) -> u8 {
        self.padding
    }
}

pub struct RtcpCompound<'a> {
    bytes: &'a [u8],
    offset: usize,
    reports: usize,
}
impl<'a> RtcpCompound<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.is_empty() {
            return Err(PacketError::TooShort);
        }
        if bytes.len() > MAX_RTCP_PACKET_BYTES * MAX_RTCP_REPORT_BLOCKS {
            return Err(PacketError::TooManyItems);
        }
        Ok(Self {
            bytes,
            offset: 0,
            reports: 0,
        })
    }
}
impl<'a> Iterator for RtcpCompound<'a> {
    type Item = Result<RtcpPacket<'a>, PacketError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.offset == self.bytes.len() {
            return None;
        }
        if self.reports == MAX_RTCP_REPORT_BLOCKS {
            self.offset = self.bytes.len();
            return Some(Err(PacketError::TooManyItems));
        }
        let _end = match self.offset.checked_add(4) {
            Some(value) if value <= self.bytes.len() => value,
            _ => {
                self.offset = self.bytes.len();
                return Some(Err(PacketError::TooShort));
            }
        };
        let words = match read_u16(self.bytes, self.offset.saturating_add(2)) {
            Ok(value) => usize::from(value),
            Err(error) => {
                self.offset = self.bytes.len();
                return Some(Err(error));
            }
        };
        let Some(length) = words.checked_add(1).and_then(|v| v.checked_mul(4)) else {
            self.offset = self.bytes.len();
            return Some(Err(PacketError::InvalidLength));
        };
        let packet_end = match self.offset.checked_add(length) {
            Some(value) if value <= self.bytes.len() => value,
            _ => {
                self.offset = self.bytes.len();
                return Some(Err(PacketError::InvalidLength));
            }
        };
        let result = RtcpPacket::parse(self.bytes.get(self.offset..packet_end).unwrap_or_default())
            .and_then(|packet| {
                if packet.padding() != 0 && packet_end != self.bytes.len() {
                    Err(PacketError::InvalidPadding)
                } else {
                    Ok(packet)
                }
            });
        self.offset = packet_end;
        self.reports = self.reports.saturating_add(1);
        Some(result)
    }
}

#[cfg(test)]
mod structural {
    use super::*;

    fn rtp(flags: u8, extension: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x80 | flags, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
        bytes.extend_from_slice(extension);
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn parses_rtp_structure_and_one_byte_extensions() {
        let packet = rtp(0x10, &[0xbe, 0xde, 0, 1, 0x10, 0xaa, 0, 0], &[1, 2]);
        let parsed = RtpPacket::parse(&packet).unwrap();
        assert_eq!(parsed.payload(), &[1, 2]);
        let extensions: Vec<_> = parsed.extensions().unwrap().collect();
        assert_eq!(extensions[0].id(), 1);
        assert_eq!(extensions[0].value(), &[0xaa]);
    }

    #[test]
    fn extension_terminator_keeps_previous_and_two_byte_255_is_valid() {
        let packet = rtp(0x10, &[0xbe, 0xde, 0, 1, 0x10, 0xaa, 0xf0, 0xff], &[]);
        let parsed = RtpPacket::parse(&packet).unwrap();
        let extensions: Vec<_> = parsed.extensions().unwrap().collect();
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0].value(), &[0xaa]);

        let packet = rtp(0x10, &[0x10, 0x00, 0, 1, 0xff, 1, 0xab, 0], &[]);
        let parsed = RtpPacket::parse(&packet).unwrap();
        let extensions: Vec<_> = parsed.extensions().unwrap().collect();
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0].id(), 255);
        assert_eq!(extensions[0].value(), &[0xab]);

        let packet = rtp(0x10, &[0x10, 0x00, 0, 1, 0xff, 0, 0, 0], &[]);
        let parsed = RtpPacket::parse(&packet).unwrap();
        let extension = parsed.extensions().unwrap().next().unwrap();
        assert_eq!(extension.id(), 255);
        assert!(extension.value().is_empty());
    }

    #[test]
    fn validates_padding_and_csrc_boundaries() {
        let mut packet = rtp(0x20, &[], &[1, 2, 2]);
        assert_eq!(RtpPacket::parse(&packet).unwrap().payload(), &[1]);
        *packet.last_mut().unwrap() = 0;
        assert_eq!(RtpPacket::parse(&packet), Err(PacketError::InvalidPadding));
        let mut invalid = rtp(0x0f, &[], &[]);
        invalid.truncate(20);
        assert_eq!(RtpPacket::parse(&invalid), Err(PacketError::InvalidLength));
    }

    #[test]
    fn csrc_values_ranges_and_all_truncations_are_bounded() {
        let mut packet = vec![0x82, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
        packet.extend_from_slice(&[0x11, 0x22, 0x33, 0x44, 0xaa, 0xbb, 0xcc, 0xdd, 9]);
        let parsed = RtpPacket::parse(&packet).unwrap();
        assert_eq!(
            parsed.csrcs().collect::<Vec<_>>(),
            vec![0x11223344, 0xaabbccdd]
        );
        assert!(parsed.payload_range().end <= packet.len());
        assert!(parsed.extension_range().is_none());
        for length in 0..packet.len() {
            let _ = RtpPacket::parse(&packet[..length]);
        }
        for count in 0..=15u8 {
            let mut candidate = vec![0x80 | count, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
            candidate.resize(12 + usize::from(count) * 4, 0);
            assert!(RtpPacket::parse(&candidate).is_ok());
        }
    }

    #[test]
    fn compound_rtcp_is_bounded() {
        let bytes = [0x80, 200, 0, 1, 0, 0, 0, 0, 0x80, 201, 0, 1, 0, 0, 0, 0];
        assert_eq!(
            RtcpCompound::parse(&bytes)
                .unwrap()
                .map(Result::unwrap)
                .count(),
            2
        );
        let malformed = [0x80, 200, 0, 3, 0, 0, 0, 0];
        assert!(matches!(
            RtcpCompound::parse(&malformed).unwrap().next(),
            Some(Err(PacketError::InvalidLength))
        ));
        let mut many = Vec::new();
        for _ in 0..257 {
            many.extend_from_slice(&[0x80, 204, 0, 1, 0, 0, 0, 0]);
        }
        let mut compound = RtcpCompound::parse(&many).unwrap();
        for _ in 0..256 {
            assert!(compound.next().unwrap().is_ok());
        }
        assert_eq!(compound.next(), Some(Err(PacketError::TooManyItems)));
        let padded_first = [
            0xa0, 204, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0x80, 204, 0, 1, 0, 0, 0, 0,
        ];
        assert_eq!(
            RtcpCompound::parse(&padded_first).unwrap().next(),
            Some(Err(PacketError::InvalidPadding))
        );
    }

    #[test]
    fn arbitrary_bytes_do_not_panic() {
        proptest::proptest!(|(bytes: Vec<u8>)| {
            if let Ok(packet) = RtpPacket::parse(&bytes) {
                let _ = packet.extensions().unwrap().collect::<Vec<_>>();
                let _ = packet.csrcs().collect::<Vec<_>>();
            }
            if let Ok(compound) = RtcpCompound::parse(&bytes) {
                for packet in compound.flatten() {
                        let _ = packet.sender_report();
                        let _ = packet.receiver_report();
                        if let Ok(Some(sdes)) = packet.sdes() {
                            for chunk in sdes.chunks() {
                                let _ = chunk.items().collect::<Vec<_>>();
                            }
                        }
                        let _ = packet.bye();
                        let _ = packet.nack().map(|value| value.map(|nack| nack.pairs().count()));
                        let _ = packet.pli();
                        let _ = packet.fir().map(|value| value.map(|fir| fir.entries().count()));
                        let _ = packet.twcc().map(|value| {
                            value.map(|twcc| twcc.statuses().collect::<Vec<_>>())
                        });
                }
            }
        });
    }
}
