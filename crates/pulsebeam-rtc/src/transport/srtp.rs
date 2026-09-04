use str0m::crypto::CryptoProvider;
use str0m::crypto::dtls::{KeyingMaterial, SrtpProfile};
use str0m::rtp::RtpHeader;
use str0m::rtp_::{SrtpContext, extend_u16};

use crate::packet::{PacketError, RtpPacket};

const MAX_SSRC_STATES: usize = 512;
const MAX_RTP_EXTENSION_BYTES: usize = 16 * 1024;
const SRTCP_INDEX_MASK: u64 = 0x7fff_ffff;
const SRTCP_INDEX_MODULUS: u64 = 1 << 31;
const SRTCP_INDEX_HALF_RANGE: u64 = 1 << 30;

#[derive(Debug, PartialEq, Eq)]
pub enum SrtpError {
    UnsupportedProfile,
    InvalidPacket,
    Replay,
    Crypto,
    OutputFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpMetadata {
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload_type: u8,
    pub marker: bool,
    pub header_len: usize,
}

struct RtpHeaderFacts {
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    payload_type: u8,
    marker: bool,
    has_padding: bool,
    has_extension: bool,
    csrc_count: usize,
    csrc: [u32; 15],
    header_len: usize,
}

impl RtpHeaderFacts {
    fn parse(packet: &[u8]) -> Result<Self, SrtpError> {
        if packet.len() < 12 {
            return Err(SrtpError::InvalidPacket);
        }
        let first = *packet.first().ok_or(SrtpError::InvalidPacket)?;
        if first >> 6 != 2 {
            return Err(SrtpError::InvalidPacket);
        }
        let csrc_count = usize::from(first & 0x0f);
        let csrc_bytes = csrc_count.checked_mul(4).ok_or(SrtpError::InvalidPacket)?;
        let mut header_len = 12usize
            .checked_add(csrc_bytes)
            .ok_or(SrtpError::InvalidPacket)?;
        if packet.get(..header_len).is_none() {
            return Err(SrtpError::InvalidPacket);
        }
        let mut csrc = [0_u32; 15];
        for (index, slot) in csrc.iter_mut().enumerate().take(csrc_count) {
            let offset = 12usize
                .checked_add(index.checked_mul(4).ok_or(SrtpError::InvalidPacket)?)
                .ok_or(SrtpError::InvalidPacket)?;
            let end = offset.checked_add(4).ok_or(SrtpError::InvalidPacket)?;
            *slot = u32::from_be_bytes(
                packet
                    .get(offset..end)
                    .ok_or(SrtpError::InvalidPacket)?
                    .try_into()
                    .map_err(|_| SrtpError::InvalidPacket)?,
            );
        }
        let has_extension = first & 0x10 != 0;
        if has_extension {
            let extension_end = header_len.checked_add(4).ok_or(SrtpError::InvalidPacket)?;
            let extension_header = packet
                .get(header_len..extension_end)
                .ok_or(SrtpError::InvalidPacket)?;
            let words = usize::from(u16::from_be_bytes([
                *extension_header.get(2).ok_or(SrtpError::InvalidPacket)?,
                *extension_header.get(3).ok_or(SrtpError::InvalidPacket)?,
            ]));
            let extension_bytes = words.checked_mul(4).ok_or(SrtpError::InvalidPacket)?;
            if extension_bytes > MAX_RTP_EXTENSION_BYTES {
                return Err(SrtpError::InvalidPacket);
            }
            header_len = extension_end
                .checked_add(extension_bytes)
                .ok_or(SrtpError::InvalidPacket)?;
            if packet.get(..header_len).is_none() {
                return Err(SrtpError::InvalidPacket);
            }
        }
        Ok(Self {
            sequence: u16::from_be_bytes([
                *packet.get(2).ok_or(SrtpError::InvalidPacket)?,
                *packet.get(3).ok_or(SrtpError::InvalidPacket)?,
            ]),
            timestamp: u32::from_be_bytes([
                *packet.get(4).ok_or(SrtpError::InvalidPacket)?,
                *packet.get(5).ok_or(SrtpError::InvalidPacket)?,
                *packet.get(6).ok_or(SrtpError::InvalidPacket)?,
                *packet.get(7).ok_or(SrtpError::InvalidPacket)?,
            ]),
            ssrc: u32::from_be_bytes([
                *packet.get(8).ok_or(SrtpError::InvalidPacket)?,
                *packet.get(9).ok_or(SrtpError::InvalidPacket)?,
                *packet.get(10).ok_or(SrtpError::InvalidPacket)?,
                *packet.get(11).ok_or(SrtpError::InvalidPacket)?,
            ]),
            payload_type: *packet.get(1).ok_or(SrtpError::InvalidPacket)? & 0x7f,
            marker: *packet.get(1).ok_or(SrtpError::InvalidPacket)? & 0x80 != 0,
            has_padding: first & 0x20 != 0,
            has_extension,
            csrc_count,
            csrc,
            header_len,
        })
    }
}

#[derive(Debug)]
struct ReplayWindow {
    ssrc: u32,
    highest: u64,
    bitmap: u64,
}

impl ReplayWindow {
    fn accept(&mut self, index: u64) -> bool {
        if index > self.highest {
            let Some(shift) = index.checked_sub(self.highest) else {
                debug_assert!(false, "replay index ordering changed");
                return false;
            };
            self.bitmap = if shift >= 64 {
                1
            } else {
                (self.bitmap << shift) | 1
            };
            self.highest = index;
            return true;
        }
        let Some(delta) = self.highest.checked_sub(index) else {
            debug_assert!(false, "replay index ordering changed");
            return false;
        };
        if delta >= 64 || self.bitmap & (1 << delta) != 0 {
            return false;
        }
        self.bitmap |= 1 << delta;
        true
    }
}

#[derive(Debug)]
pub(crate) struct SrtpLayer {
    tx: SrtpContext,
    rx: SrtpContext,
    tx_last: Vec<ReplayWindow>,
    rx_last: Vec<ReplayWindow>,
    rtcp_rx_last: Vec<ReplayWindow>,
    profile: SrtpProfile,
}

impl SrtpLayer {
    pub(crate) fn new(
        material: KeyingMaterial,
        profile: SrtpProfile,
        active: bool,
        provider: &CryptoProvider,
    ) -> Result<Self, SrtpError> {
        if !matches!(
            profile,
            SrtpProfile::AES128_CM_SHA1_80
                | SrtpProfile::AEAD_AES_128_GCM
                | SrtpProfile::AEAD_AES_256_GCM
        ) {
            return Err(SrtpError::UnsupportedProfile);
        }
        Ok(Self {
            tx: SrtpContext::new(provider, profile, &material, active),
            rx: SrtpContext::new(provider, profile, &material, !active),
            tx_last: Vec::new(),
            rx_last: Vec::new(),
            rtcp_rx_last: Vec::new(),
            profile,
        })
    }

    pub(crate) fn protect_rtp(&mut self, packet: &[u8]) -> Result<Vec<u8>, SrtpError> {
        let parsed = RtpPacket::parse(packet).map_err(|_| SrtpError::InvalidPacket)?;
        let index = self.next_tx_index(parsed.ssrc(), parsed.sequence())?;
        let header = to_str0m_header(&parsed)?;
        Ok(self.tx.protect_rtp(packet, &header, index))
    }

    fn next_tx_index(&mut self, ssrc: u32, sequence: u16) -> Result<u64, SrtpError> {
        if let Some(window) = self.tx_last.iter_mut().find(|window| window.ssrc == ssrc) {
            let index = extend_sequence(Some(window.highest), sequence);
            window.highest = index;
            return Ok(index);
        }
        if self.tx_last.len() >= MAX_SSRC_STATES {
            return Err(SrtpError::OutputFull);
        }
        let index = extend_sequence(None, sequence);
        self.tx_last.push(ReplayWindow {
            ssrc,
            highest: index,
            bitmap: 0,
        });
        Ok(index)
    }

    pub(crate) fn protect_rtcp(&mut self, packet: &[u8]) -> Result<Vec<u8>, SrtpError> {
        if packet.len() < 8 {
            return Err(SrtpError::InvalidPacket);
        }
        Ok(self.tx.protect_rtcp(packet))
    }

    pub(crate) fn unprotect_rtp(
        &mut self,
        packet: &[u8],
    ) -> Result<(Vec<u8>, RtpMetadata), SrtpError> {
        let parsed = RtpHeaderFacts::parse(packet)?;
        let index = self
            .rx_last
            .iter()
            .find(|window| window.ssrc == parsed.ssrc)
            .map_or_else(
                || extend_sequence(None, parsed.sequence),
                |window| extend_sequence(Some(window.highest), parsed.sequence),
            );
        let Some(window_index) = self
            .rx_last
            .iter()
            .position(|window| window.ssrc == parsed.ssrc)
        else {
            if self.rx_last.len() >= MAX_SSRC_STATES {
                return Err(SrtpError::OutputFull);
            }
            let result = decrypt_rtp(&mut self.rx, packet, &parsed, index);
            if result.is_ok() {
                self.rx_last.push(ReplayWindow {
                    ssrc: parsed.ssrc,
                    highest: index,
                    bitmap: 1,
                });
            }
            return result;
        };
        let window = self
            .rx_last
            .get_mut(window_index)
            .ok_or(SrtpError::OutputFull)?;
        let previous = (window.highest, window.bitmap);
        if !window.accept(index) {
            return Err(SrtpError::Replay);
        }
        let result = decrypt_rtp(&mut self.rx, packet, &parsed, index);
        if result.is_err() {
            window.highest = previous.0;
            window.bitmap = previous.1;
        }
        result
    }

    pub(crate) fn unprotect_rtcp(&mut self, packet: &[u8]) -> Result<Vec<u8>, SrtpError> {
        if packet.len() < 12 {
            return Err(SrtpError::InvalidPacket);
        }
        let index_start = match self.profile {
            SrtpProfile::AES128_CM_SHA1_80 => packet.len().checked_sub(14),
            SrtpProfile::AEAD_AES_128_GCM | SrtpProfile::AEAD_AES_256_GCM => {
                packet.len().checked_sub(4)
            }
            _ => return Err(SrtpError::UnsupportedProfile),
        }
        .ok_or(SrtpError::InvalidPacket)?;
        let raw_index = u32::from_be_bytes(
            packet
                .get(index_start..index_start.checked_add(4).ok_or(SrtpError::InvalidPacket)?)
                .ok_or(SrtpError::InvalidPacket)?
                .try_into()
                .map_err(|_| SrtpError::InvalidPacket)?,
        );
        let index = raw_srtcp_index(
            u64::from(raw_index) & SRTCP_INDEX_MASK,
            packet,
            &self.rtcp_rx_last,
        )?;
        let ssrc = u32::from_be_bytes(
            packet
                .get(4..8)
                .ok_or(SrtpError::InvalidPacket)?
                .try_into()
                .map_err(|_| SrtpError::InvalidPacket)?,
        );
        let Some(window_index) = self
            .rtcp_rx_last
            .iter()
            .position(|window| window.ssrc == ssrc)
        else {
            if self.rtcp_rx_last.len() >= MAX_SSRC_STATES {
                return Err(SrtpError::OutputFull);
            }
            let result = self.rx.unprotect_rtcp(packet).ok_or(SrtpError::Crypto);
            if result.is_ok() {
                self.rtcp_rx_last.push(ReplayWindow {
                    ssrc,
                    highest: index,
                    bitmap: 1,
                });
            }
            return result;
        };
        let window = self
            .rtcp_rx_last
            .get_mut(window_index)
            .ok_or(SrtpError::OutputFull)?;
        let previous = (window.highest, window.bitmap);
        if !window.accept(index) {
            return Err(SrtpError::Replay);
        }
        let result = self.rx.unprotect_rtcp(packet).ok_or(SrtpError::Crypto);
        if result.is_err() {
            window.highest = previous.0;
            window.bitmap = previous.1;
        }
        result
    }
}

fn raw_srtcp_index(raw: u64, packet: &[u8], windows: &[ReplayWindow]) -> Result<u64, SrtpError> {
    let ssrc = u32::from_be_bytes(
        packet
            .get(4..8)
            .ok_or(SrtpError::InvalidPacket)?
            .try_into()
            .map_err(|_| SrtpError::InvalidPacket)?,
    );
    let Some(previous) = windows.iter().find(|window| window.ssrc == ssrc) else {
        return Ok(raw);
    };
    let previous_low = previous.highest & SRTCP_INDEX_MASK;
    let roc = previous.highest / SRTCP_INDEX_MODULUS;
    let index = if raw < previous_low
        && previous_low
            .checked_sub(raw)
            .is_some_and(|difference| difference > SRTCP_INDEX_HALF_RANGE)
    {
        roc.checked_add(1)
            .and_then(|next_roc| next_roc.checked_mul(SRTCP_INDEX_MODULUS))
            .and_then(|base| base.checked_add(raw))
            .ok_or(SrtpError::OutputFull)?
    } else if raw > previous_low
        && raw
            .checked_sub(previous_low)
            .is_some_and(|difference| difference > SRTCP_INDEX_HALF_RANGE)
        && roc > 0
    {
        roc.checked_sub(1)
            .and_then(|previous_roc| previous_roc.checked_mul(SRTCP_INDEX_MODULUS))
            .and_then(|base| base.checked_add(raw))
            .ok_or(SrtpError::OutputFull)?
    } else {
        roc.checked_mul(SRTCP_INDEX_MODULUS)
            .and_then(|base| base.checked_add(raw))
            .ok_or(SrtpError::OutputFull)?
    };
    Ok(index)
}

fn decrypt_rtp(
    context: &mut SrtpContext,
    packet: &[u8],
    parsed: &RtpHeaderFacts,
    index: u64,
) -> Result<(Vec<u8>, RtpMetadata), SrtpError> {
    let header = to_str0m_header_facts(parsed);
    let plaintext = context
        .unprotect_rtp(packet, &header, index)
        .ok_or(SrtpError::Crypto)?;
    let metadata = RtpMetadata {
        sequence: parsed.sequence,
        timestamp: parsed.timestamp,
        ssrc: parsed.ssrc,
        payload_type: parsed.payload_type,
        marker: parsed.marker,
        header_len: parsed.header_len,
    };
    let capacity = header
        .header_len
        .checked_add(plaintext.len())
        .ok_or(SrtpError::OutputFull)?;
    let mut output = Vec::with_capacity(capacity);
    output.extend_from_slice(
        packet
            .get(..header.header_len)
            .ok_or(SrtpError::InvalidPacket)?,
    );
    output.extend_from_slice(plaintext);
    Ok((output, metadata))
}

fn to_str0m_header_facts(packet: &RtpHeaderFacts) -> RtpHeader {
    RtpHeader {
        version: 2,
        has_padding: packet.has_padding,
        has_extension: packet.has_extension,
        csrc_count: packet.csrc_count,
        marker: packet.marker,
        payload_type: packet.payload_type.into(),
        sequence_number: packet.sequence,
        timestamp: packet.timestamp,
        ssrc: packet.ssrc.into(),
        csrc: packet.csrc,
        ext_vals: Default::default(),
        header_len: packet.header_len,
    }
}

fn to_str0m_header(packet: &RtpPacket<'_>) -> Result<RtpHeader, SrtpError> {
    let mut csrc = [0_u32; 15];
    for (slot, value) in csrc.iter_mut().zip(packet.csrcs()) {
        *slot = value;
    }
    Ok(RtpHeader {
        version: 2,
        has_padding: packet.padding() != 0,
        has_extension: packet.extension_profile().is_some(),
        csrc_count: usize::from(packet.csrc_count()),
        marker: packet.marker(),
        payload_type: packet.payload_type().into(),
        sequence_number: packet.sequence(),
        timestamp: packet.timestamp(),
        ssrc: packet.ssrc().into(),
        csrc,
        ext_vals: Default::default(),
        header_len: packet.payload_range().start,
    })
}

fn extend_sequence(previous: Option<u64>, sequence: u16) -> u64 {
    extend_u16(previous, sequence)
}

impl From<PacketError> for SrtpError {
    fn from(_: PacketError) -> Self {
        Self::InvalidPacket
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtcp_header(ssrc: u32) -> [u8; 8] {
        let bytes = ssrc.to_be_bytes();
        [0x80, 201, 0, 1, bytes[0], bytes[1], bytes[2], bytes[3]]
    }

    #[test]
    fn srtcp_index_is_per_ssrc_and_wraps_at_31_bits() {
        let packet_a = rtcp_header(1);
        let packet_b = rtcp_header(2);
        let windows = [ReplayWindow {
            ssrc: 1,
            highest: SRTCP_INDEX_MODULUS - 1,
            bitmap: 1,
        }];
        assert_eq!(
            raw_srtcp_index(0, &packet_a, &windows),
            Ok(SRTCP_INDEX_MODULUS)
        );
        assert_eq!(raw_srtcp_index(0, &packet_b, &windows), Ok(0));
    }
}
