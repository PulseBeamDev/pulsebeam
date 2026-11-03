use std::collections::{BTreeMap, BTreeSet};

use str0m::crypto::CryptoProvider;
use str0m::crypto::dtls::{KeyingMaterial, SrtpProfile};
use str0m::rtp::RtpHeader;
use str0m::rtp_::{SrtpContext, extend_u16};

use crate::packet::RtpPacket;

const MAX_RTP_EXTENSION_BYTES: usize = 16 * 1024;
const SRTCP_INDEX_MASK: u64 = 0x7fff_ffff;
const MAX_SRTP_INDEX: u64 = 1 << 48;
const MAX_SRTCP_PACKETS: u32 = 1 << 31;

#[derive(Debug, PartialEq, Eq)]
pub enum SrtpError {
    UnsupportedProfile,
    InvalidPacket,
    Replay,
    Crypto,
    OutputFull,
    OutputTooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ProtectionLayout {
    pub(super) wire_bytes: usize,
    pub(super) initialized_bytes: usize,
}

impl From<str0m::rtp_::srtp::SrtpProtectionLayout> for ProtectionLayout {
    fn from(layout: str0m::rtp_::srtp::SrtpProtectionLayout) -> Self {
        Self {
            wire_bytes: layout.wire_bytes,
            initialized_bytes: layout.initialized_bytes,
        }
    }
}

impl ProtectionLayout {
    pub(super) fn check_limit(self, limit: Option<usize>) -> Result<Self, SrtpError> {
        if limit.is_some_and(|limit| self.wire_bytes > limit) {
            return Err(SrtpError::OutputTooLarge);
        }
        Ok(self)
    }
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

pub(crate) fn outbound_rtp_metadata(packet: &[u8]) -> Result<RtpMetadata, SrtpError> {
    let parsed = RtpPacket::parse(packet).map_err(|_| SrtpError::InvalidPacket)?;
    Ok(RtpMetadata {
        sequence: parsed.sequence(),
        timestamp: parsed.timestamp(),
        ssrc: parsed.ssrc(),
        payload_type: parsed.payload_type(),
        marker: parsed.marker(),
        header_len: parsed.payload_range().start,
    })
}

pub(crate) fn outbound_twcc_sequence(
    packet: &[u8],
    twcc_extension_id: Option<u8>,
) -> Result<Option<u16>, SrtpError> {
    let Some(id) = twcc_extension_id else {
        return Ok(None);
    };
    let parsed = RtpPacket::parse(packet).map_err(|_| SrtpError::InvalidPacket)?;
    Ok(parsed
        .extensions()
        .map_err(|_| SrtpError::InvalidPacket)?
        .find(|extension| extension.id() == id && extension.value().len() == 2)
        .and_then(|extension| extension.value().try_into().ok())
        .map(u16::from_be_bytes))
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
struct RtpReplayState {
    window: ReplayWindow,
    reserved: bool,
}

pub(crate) const RTP_REPLAY_STATE_BYTES: usize = std::mem::size_of::<(u32, RtpReplayState)>();
pub(crate) const RTCP_REPLAY_STATE_BYTES: usize = std::mem::size_of::<(u32, ReplayWindow)>();

#[derive(Clone, Copy, Debug)]
pub(crate) struct ReplayLimits {
    pub(crate) rtp_bytes: usize,
    pub(crate) rtcp_bytes: usize,
}

impl Default for ReplayLimits {
    fn default() -> Self {
        Self {
            rtp_bytes: crate::ConnectionLimits::DEFAULT_MAX_RTP_REPLAY_STATE_BYTES,
            rtcp_bytes: crate::ConnectionLimits::DEFAULT_MAX_RTCP_REPLAY_STATE_BYTES,
        }
    }
}

impl ReplayLimits {
    pub(crate) fn fits_rtp(self, states: usize) -> bool {
        states
            .checked_mul(RTP_REPLAY_STATE_BYTES)
            .is_some_and(|bytes| bytes <= self.rtp_bytes)
    }

    fn fits_rtcp(self, states: usize) -> bool {
        states
            .checked_mul(RTCP_REPLAY_STATE_BYTES)
            .is_some_and(|bytes| bytes <= self.rtcp_bytes)
    }
}

#[derive(Debug)]
pub(crate) struct SrtpLayer {
    tx: SrtpContext,
    rx: SrtpContext,
    rx_last: BTreeMap<u32, RtpReplayState>,
    unreserved_rx: usize,
    reserved_rx: usize,
    limits: ReplayLimits,
    rtcp_rx_last: BTreeMap<u32, ReplayWindow>,
    profile: SrtpProfile,
    tx_rtcp_packets: u32,
}

impl SrtpLayer {
    pub(crate) fn new(
        material: KeyingMaterial,
        profile: SrtpProfile,
        active: bool,
        provider: &CryptoProvider,
        limits: ReplayLimits,
        reserved_rx: usize,
    ) -> Result<Self, SrtpError> {
        if !matches!(
            profile,
            SrtpProfile::AES128_CM_SHA1_80
                | SrtpProfile::AEAD_AES_128_GCM
                | SrtpProfile::AEAD_AES_256_GCM
        ) {
            return Err(SrtpError::UnsupportedProfile);
        }
        if !limits.fits_rtp(reserved_rx) {
            return Err(SrtpError::OutputFull);
        }
        Ok(Self {
            tx: SrtpContext::new(provider, profile, &material, active),
            rx: SrtpContext::new(provider, profile, &material, !active),
            rx_last: BTreeMap::new(),
            unreserved_rx: 0,
            reserved_rx,
            limits,
            rtcp_rx_last: BTreeMap::new(),
            profile,
            tx_rtcp_packets: 0,
        })
    }

    pub(crate) fn can_admit_ssrcs(
        &self,
        reserved: usize,
        new: &BTreeSet<u32>,
        retiring: &BTreeSet<u32>,
    ) -> bool {
        let overlap = new
            .union(retiring)
            .filter(|ssrc| self.rx_last.get(ssrc).is_some_and(|state| !state.reserved))
            .count();
        self.limits
            .fits_rtp(reserved.saturating_add(self.unreserved_rx.saturating_sub(overlap)))
    }

    pub(crate) fn reserve_ssrcs(&mut self, reserved: usize, identities: &BTreeSet<u32>) {
        self.reserved_rx = reserved;
        for ssrc in identities {
            if let Some(state) = self.rx_last.get_mut(ssrc).filter(|state| !state.reserved) {
                state.reserved = true;
                self.unreserved_rx = self.unreserved_rx.saturating_sub(1);
            }
        }
    }

    // The transport must install its permanent SSRC fence before releasing a window.
    pub(crate) fn retire_ssrc(&mut self, ssrc: u32) {
        if self
            .rx_last
            .remove(&ssrc)
            .is_some_and(|state| !state.reserved)
        {
            self.unreserved_rx = self.unreserved_rx.saturating_sub(1);
        }
    }

    pub(super) fn rtp_layout(&self, plaintext: usize) -> Result<ProtectionLayout, SrtpError> {
        self.tx
            .rtp_protection_layout(plaintext)
            .map(ProtectionLayout::from)
            .ok_or(SrtpError::OutputTooLarge)
    }

    pub(super) fn rtcp_layout(&self, plaintext: usize) -> Result<ProtectionLayout, SrtpError> {
        self.tx
            .rtcp_protection_layout(plaintext)
            .map(ProtectionLayout::from)
            .ok_or(SrtpError::OutputTooLarge)
    }

    pub(crate) fn protect_rtp_into(
        &mut self,
        packet: &[u8],
        index: u64,
        wire_limit: Option<usize>,
        output: &mut [u8],
    ) -> Result<usize, SrtpError> {
        let parsed = RtpHeaderFacts::parse(packet)?;
        if index >= MAX_SRTP_INDEX || u16::try_from(index & 0xffff).ok() != Some(parsed.sequence) {
            return Err(SrtpError::InvalidPacket);
        }
        let layout = self.rtp_layout(packet.len())?.check_limit(wire_limit)?;
        let header = to_str0m_header_facts(&parsed);
        let written = self
            .tx
            .protect_rtp_into(packet, &header, index, output)
            .ok_or(SrtpError::OutputFull)?;
        debug_assert_eq!(written, layout.wire_bytes);
        Ok(written)
    }

    #[cfg(test)]
    pub(super) const fn test_rtcp_packets(&self) -> u32 {
        self.tx_rtcp_packets
    }

    pub(crate) fn protect_rtcp_into(
        &mut self,
        packet: &[u8],
        wire_limit: Option<usize>,
        output: &mut [u8],
    ) -> Result<usize, SrtpError> {
        if packet.len() < 8 {
            return Err(SrtpError::InvalidPacket);
        }
        let layout = self.rtcp_layout(packet.len())?.check_limit(wire_limit)?;
        // str0m's context-scoped SRTCP counter wraps internally. Do not let
        // it reuse an index/IV under this key epoch.
        if self.tx_rtcp_packets >= MAX_SRTCP_PACKETS {
            return Err(SrtpError::OutputFull);
        }
        let next = self
            .tx_rtcp_packets
            .checked_add(1)
            .ok_or(SrtpError::OutputFull)?;
        let written = self
            .tx
            .protect_rtcp_into(packet, output)
            .ok_or(SrtpError::OutputFull)?;
        debug_assert_eq!(written, layout.wire_bytes);
        self.tx_rtcp_packets = next;
        Ok(written)
    }

    #[cfg(test)]
    pub(super) fn protect_rtp(
        &mut self,
        packet: &[u8],
        index: u64,
        wire_limit: Option<usize>,
    ) -> Result<Vec<u8>, SrtpError> {
        let layout = self.rtp_layout(packet.len())?.check_limit(wire_limit)?;
        let mut output = vec![0; layout.initialized_bytes];
        let written = self.protect_rtp_into(packet, index, wire_limit, &mut output)?;
        output.truncate(written);
        Ok(output)
    }

    #[cfg(test)]
    pub(super) fn protect_rtcp(
        &mut self,
        packet: &[u8],
        wire_limit: Option<usize>,
    ) -> Result<Vec<u8>, SrtpError> {
        let layout = self.rtcp_layout(packet.len())?.check_limit(wire_limit)?;
        let mut output = vec![0; layout.initialized_bytes];
        let written = self.protect_rtcp_into(packet, wire_limit, &mut output)?;
        output.truncate(written);
        Ok(output)
    }

    pub(crate) fn unprotect_rtp(
        &mut self,
        packet: &[u8],
        reserved: bool,
    ) -> Result<(Vec<u8>, RtpMetadata), SrtpError> {
        let parsed = RtpHeaderFacts::parse(packet)?;
        let index = extend_sequence(
            self.rx_last
                .get(&parsed.ssrc)
                .map(|state| state.window.highest),
            parsed.sequence,
        );
        if index >= MAX_SRTP_INDEX {
            return Err(SrtpError::InvalidPacket);
        }
        let Some(state) = self.rx_last.get_mut(&parsed.ssrc) else {
            let states = self
                .reserved_rx
                .saturating_add(self.unreserved_rx)
                .saturating_add(usize::from(!reserved))
                .max(self.rx_last.len().saturating_add(1));
            if !self.limits.fits_rtp(states) {
                return Err(SrtpError::OutputFull);
            }
            let result = decrypt_rtp(&mut self.rx, packet, &parsed, index);
            if result.is_ok() {
                self.rx_last.insert(
                    parsed.ssrc,
                    RtpReplayState {
                        window: ReplayWindow {
                            highest: index,
                            bitmap: 1,
                        },
                        reserved,
                    },
                );
                if !reserved {
                    self.unreserved_rx = self.unreserved_rx.saturating_add(1);
                }
            }
            return result;
        };
        let previous = (state.window.highest, state.window.bitmap);
        if !state.window.accept(index) {
            return Err(SrtpError::Replay);
        }
        let result = decrypt_rtp(&mut self.rx, packet, &parsed, index);
        if result.is_err() {
            state.window.highest = previous.0;
            state.window.bitmap = previous.1;
        } else if reserved && !state.reserved {
            state.reserved = true;
            self.unreserved_rx = self.unreserved_rx.saturating_sub(1);
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
        // A 31-bit index cannot be extended across a wrap under the same
        // key: that would authenticate old ciphertext as a new packet.
        let index = u64::from(raw_index) & SRTCP_INDEX_MASK;
        let ssrc = u32::from_be_bytes(
            packet
                .get(4..8)
                .ok_or(SrtpError::InvalidPacket)?
                .try_into()
                .map_err(|_| SrtpError::InvalidPacket)?,
        );
        let Some(window) = self.rtcp_rx_last.get_mut(&ssrc) else {
            if !self
                .limits
                .fits_rtcp(self.rtcp_rx_last.len().saturating_add(1))
            {
                return Err(SrtpError::OutputFull);
            }
            let result = self.rx.unprotect_rtcp(packet).ok_or(SrtpError::Crypto);
            if result.is_ok() {
                self.rtcp_rx_last.insert(
                    ssrc,
                    ReplayWindow {
                        highest: index,
                        bitmap: 1,
                    },
                );
            }
            return result;
        };
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

fn extend_sequence(previous: Option<u64>, sequence: u16) -> u64 {
    extend_u16(previous, sequence)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_profile_protects_media(profile: SrtpProfile) {
        let provider = str0m::crypto::from_feature_flags();
        let material = vec![7; profile.keying_material_len()];
        let mut sender = SrtpLayer::new(
            KeyingMaterial::new(&material),
            profile,
            true,
            &provider,
            ReplayLimits::default(),
            0,
        )
        .expect("sender profile");
        let mut receiver = SrtpLayer::new(
            KeyingMaterial::new(&material),
            profile,
            false,
            &provider,
            ReplayLimits {
                rtp_bytes: RTP_REPLAY_STATE_BYTES.saturating_mul(4),
                rtcp_bytes: RTCP_REPLAY_STATE_BYTES.saturating_mul(2),
            },
            0,
        )
        .expect("receiver profile");
        let limit = usize::from(u16::MAX);
        let rtp_plaintext = limit
            .checked_sub(sender.rtp_layout(0).unwrap().wire_bytes)
            .unwrap();
        let rtcp_plaintext = limit
            .checked_sub(sender.rtcp_layout(0).unwrap().wire_bytes)
            .unwrap();
        assert_eq!(
            sender
                .rtp_layout(rtp_plaintext)
                .unwrap()
                .check_limit(Some(limit))
                .unwrap()
                .wire_bytes,
            limit
        );
        assert_eq!(
            sender
                .rtcp_layout(rtcp_plaintext)
                .unwrap()
                .check_limit(Some(limit))
                .unwrap()
                .wire_bytes,
            limit
        );
        assert_eq!(
            sender
                .rtp_layout(rtp_plaintext.checked_add(1).unwrap())
                .unwrap()
                .check_limit(Some(limit)),
            Err(SrtpError::OutputTooLarge)
        );
        assert_eq!(
            sender
                .rtcp_layout(rtcp_plaintext.checked_add(1).unwrap())
                .unwrap()
                .check_limit(Some(limit)),
            Err(SrtpError::OutputTooLarge)
        );
        assert_eq!(
            sender.rtp_layout(usize::MAX),
            Err(SrtpError::OutputTooLarge)
        );
        assert_eq!(
            sender.rtcp_layout(usize::MAX),
            Err(SrtpError::OutputTooLarge)
        );
        for ssrc in [0u32, 7] {
            let mut rtp = [0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 1, 2, 3];
            rtp[8..12].copy_from_slice(&ssrc.to_be_bytes());
            let layout = sender.rtp_layout(rtp.len()).unwrap();
            assert_eq!(
                sender.protect_rtp(&rtp, 1, Some(layout.wire_bytes.checked_sub(1).unwrap())),
                Err(SrtpError::OutputTooLarge)
            );
            let mut short = vec![0xa5; layout.initialized_bytes.checked_sub(1).unwrap()];
            assert_eq!(
                sender.protect_rtp_into(&rtp, 1, Some(layout.wire_bytes), &mut short),
                Err(SrtpError::OutputFull)
            );
            assert!(short.iter().all(|byte| *byte == 0xa5));
            let prefix = u16::try_from(layout.wire_bytes).unwrap().to_be_bytes();
            let mut protected = super::super::secure_output_buffer(layout, Some(prefix)).unwrap();
            let pointer = protected.as_ptr();
            let capacity = protected.capacity();
            let written = sender
                .protect_rtp_into(
                    &rtp,
                    1,
                    Some(layout.wire_bytes),
                    &mut protected[prefix.len()..],
                )
                .unwrap();
            assert_eq!(written, layout.wire_bytes);
            assert_eq!(&protected[..prefix.len()], &prefix);
            assert_eq!(protected.as_ptr(), pointer);
            assert_eq!(protected.capacity(), capacity);
            let end = prefix.len().checked_add(written).unwrap();
            protected.copy_within(prefix.len()..end, 0);
            protected.truncate(written);
            assert_eq!(protected.len(), layout.wire_bytes);
            assert!(protected.capacity() >= layout.initialized_bytes);
            if profile == SrtpProfile::AES128_CM_SHA1_80 {
                assert_eq!(
                    layout.initialized_bytes.checked_sub(layout.wire_bytes),
                    Some(5)
                );
            }
            assert_ne!(protected, rtp);
            let before = (receiver.rx_last.len(), receiver.unreserved_rx);
            let mut corrupt = protected.clone();
            *corrupt.last_mut().unwrap() ^= 1;
            assert_eq!(
                receiver.unprotect_rtp(&corrupt, false),
                Err(SrtpError::Crypto)
            );
            assert_eq!((receiver.rx_last.len(), receiver.unreserved_rx), before);
            assert_eq!(
                receiver
                    .unprotect_rtp(&protected, false)
                    .expect("authenticated RTP")
                    .0,
                rtp
            );
            assert_eq!(
                receiver.unprotect_rtp(&protected, false),
                Err(SrtpError::Replay)
            );
            rtp[2..4].copy_from_slice(&2u16.to_be_bytes());
            let second = sender.protect_rtp(&rtp, 2, None).unwrap();
            let mut corrupt = second.clone();
            *corrupt.last_mut().unwrap() ^= 1;
            assert_eq!(
                receiver.unprotect_rtp(&corrupt, true),
                Err(SrtpError::Crypto)
            );
            let state = &receiver.rx_last[&ssrc];
            assert_eq!(
                (state.window.highest, state.window.bitmap, state.reserved),
                (1, 1, false)
            );
            assert_eq!(Some(receiver.unreserved_rx), before.1.checked_add(1));
            assert_eq!(receiver.unprotect_rtp(&second, false).unwrap().0, rtp);
            let window = &mut receiver.rx_last.get_mut(&ssrc).unwrap().window;
            window.highest = MAX_SRTP_INDEX - 1;
            window.bitmap = 1;
            rtp[2..4].copy_from_slice(&0u16.to_be_bytes());
            let ancient = sender.protect_rtp(&rtp, 0, None).unwrap();
            assert_eq!(
                receiver.unprotect_rtp(&ancient, false),
                Err(SrtpError::InvalidPacket)
            );
            assert_eq!(
                sender.protect_rtp(&rtp, MAX_SRTP_INDEX, None),
                Err(SrtpError::InvalidPacket)
            );
        }
        assert!(ReplayLimits::default().fits_rtp(8192));
        assert!(!ReplayLimits::default().fits_rtp(usize::MAX));
        let capacity = receiver.limits.rtp_bytes / RTP_REPLAY_STATE_BYTES;
        assert_eq!(receiver.unreserved_rx, 2);
        let both = BTreeSet::from([0, 7]);
        let zero = BTreeSet::from([0]);
        let empty = BTreeSet::new();
        assert!(receiver.can_admit_ssrcs(capacity, &both, &empty));
        assert!(!receiver.can_admit_ssrcs(capacity, &zero, &empty));
        assert!(receiver.can_admit_ssrcs(capacity.saturating_sub(1), &zero, &empty));
        assert_eq!(receiver.unreserved_rx, 2);
        receiver.reserve_ssrcs(1, &BTreeSet::from([7]));
        assert_eq!(receiver.unreserved_rx, 1);
        assert!(!receiver.can_admit_ssrcs(capacity, &BTreeSet::from([7]), &empty));
        assert!(receiver.can_admit_ssrcs(capacity, &zero, &empty));
        receiver.reserve_ssrcs(2, &both);
        receiver.reserve_ssrcs(2, &both);
        receiver.reserve_ssrcs(3, &BTreeSet::from([8]));
        assert_eq!(receiver.unreserved_rx, 0);
        assert!(receiver.can_admit_ssrcs(capacity, &empty, &empty));
        for (ssrc, reserved) in [(8u32, true), (9, false)] {
            let mut rtp = [0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 3];
            rtp[8..12].copy_from_slice(&ssrc.to_be_bytes());
            let protected = sender.protect_rtp(&rtp, 1, None).unwrap();
            assert_eq!(receiver.unprotect_rtp(&protected, reserved).unwrap().0, rtp);
            assert_eq!(receiver.rx_last[&ssrc].reserved, reserved);
        }
        assert_eq!(receiver.unreserved_rx, 1);
        assert!(!receiver.can_admit_ssrcs(capacity, &both, &empty));
        assert!(receiver.can_admit_ssrcs(capacity.saturating_sub(1), &empty, &empty));
        assert!(receiver.can_admit_ssrcs(capacity, &BTreeSet::from([9]), &empty));
        assert!(receiver.can_admit_ssrcs(capacity, &empty, &BTreeSet::from([9])));
        let mut extra = [0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 10, 3];
        let protected = sender.protect_rtp(&extra, 1, None).unwrap();
        assert_eq!(
            receiver.unprotect_rtp(&protected, false),
            Err(SrtpError::OutputFull)
        );
        assert_eq!((receiver.rx_last.len(), receiver.unreserved_rx), (4, 1));
        receiver.retire_ssrc(9);
        assert!(!receiver.rx_last.contains_key(&9));
        assert_eq!(receiver.unreserved_rx, 0);
        receiver.reserve_ssrcs(4, &BTreeSet::from([10]));
        assert_eq!(receiver.unprotect_rtp(&protected, true).unwrap().0, extra);
        receiver.retire_ssrc(8);
        assert!(!receiver.rx_last.contains_key(&8));
        receiver.reserve_ssrcs(3, &empty);
        extra[2..4].copy_from_slice(&2u16.to_be_bytes());
        let second = sender.protect_rtp(&extra, 2, None).unwrap();
        assert_eq!(receiver.unprotect_rtp(&second, true).unwrap().0, extra);
        let rtcp = rtcp_header(0);
        let layout = sender.rtcp_layout(rtcp.len()).unwrap();
        let counter = sender.tx_rtcp_packets;
        assert_eq!(
            sender.protect_rtcp(&rtcp, Some(layout.wire_bytes.checked_sub(1).unwrap())),
            Err(SrtpError::OutputTooLarge)
        );
        assert_eq!(sender.tx_rtcp_packets, counter);
        let mut short = vec![0xa5; layout.initialized_bytes.checked_sub(1).unwrap()];
        assert_eq!(
            sender.protect_rtcp_into(&rtcp, Some(layout.wire_bytes), &mut short),
            Err(SrtpError::OutputFull)
        );
        assert!(short.iter().all(|byte| *byte == 0xa5));
        assert_eq!(sender.tx_rtcp_packets, counter);
        let first = sender.protect_rtcp(&rtcp, Some(layout.wire_bytes)).unwrap();
        assert_eq!(sender.tx_rtcp_packets, counter.checked_add(1).unwrap());
        assert_eq!(first.len(), layout.wire_bytes);
        assert!(first.capacity() >= layout.initialized_bytes);
        if profile == SrtpProfile::AES128_CM_SHA1_80 {
            assert_eq!(
                layout.initialized_bytes.checked_sub(layout.wire_bytes),
                Some(1)
            );
        }
        assert_eq!(receiver.unprotect_rtcp(&first), Ok(rtcp.to_vec()));
        assert_eq!(receiver.rtcp_rx_last[&0].highest, 0);
        let before = (
            receiver.rtcp_rx_last[&0].highest,
            receiver.rtcp_rx_last[&0].bitmap,
        );
        let second = sender.protect_rtcp(&rtcp, None).unwrap();
        let mut corrupt = second.clone();
        corrupt[8] ^= 1;
        assert_eq!(receiver.unprotect_rtcp(&corrupt), Err(SrtpError::Crypto));
        assert_eq!(
            (
                receiver.rtcp_rx_last[&0].highest,
                receiver.rtcp_rx_last[&0].bitmap
            ),
            before
        );
        assert_eq!(receiver.unprotect_rtcp(&second), Ok(rtcp.to_vec()));
        let window = receiver.rtcp_rx_last.get_mut(&0).unwrap();
        window.highest = SRTCP_INDEX_MASK;
        window.bitmap = 1;
        assert_eq!(receiver.unprotect_rtcp(&first), Err(SrtpError::Replay));
        let other = rtcp_header(9);
        let protected = sender.protect_rtcp(&other, None).unwrap();
        let mut corrupt = protected.clone();
        corrupt[8] ^= 1;
        assert_eq!(receiver.unprotect_rtcp(&corrupt), Err(SrtpError::Crypto));
        assert_eq!(receiver.rtcp_rx_last.len(), 1);
        assert_eq!(receiver.unprotect_rtcp(&protected), Ok(other.to_vec()));
        let extra = sender.protect_rtcp(&rtcp_header(10), None).unwrap();
        assert_eq!(receiver.unprotect_rtcp(&extra), Err(SrtpError::OutputFull));
        assert_eq!(receiver.rtcp_rx_last.len(), 2);
        let continuing = sender.protect_rtcp(&other, None).unwrap();
        assert_eq!(receiver.unprotect_rtcp(&continuing), Ok(other.to_vec()));
        sender.tx_rtcp_packets = MAX_SRTCP_PACKETS;
        assert_eq!(sender.protect_rtcp(&rtcp, None), Err(SrtpError::OutputFull));
        assert_eq!(sender.tx_rtcp_packets, MAX_SRTCP_PACKETS);
    }

    fn rtcp_header(ssrc: u32) -> [u8; 8] {
        let bytes = ssrc.to_be_bytes();
        [0x80, 201, 0, 1, bytes[0], bytes[1], bytes[2], bytes[3]]
    }

    #[test]
    fn modern_and_legacy_profiles_authenticate_media_without_index_reuse() {
        assert_profile_protects_media(SrtpProfile::AEAD_AES_128_GCM);
        assert_profile_protects_media(SrtpProfile::AES128_CM_SHA1_80);
        assert_profile_protects_media(SrtpProfile::AEAD_AES_256_GCM);
    }

    #[test]
    fn aead_128_outranks_legacy_and_legacy_remains_a_fallback() {
        let select = |offered: &[SrtpProfile]| {
            SrtpProfile::ALL
                .iter()
                .copied()
                .find(|profile| offered.contains(profile))
        };
        assert_eq!(
            select(&[
                SrtpProfile::AES128_CM_SHA1_80,
                SrtpProfile::AEAD_AES_128_GCM,
            ]),
            Some(SrtpProfile::AEAD_AES_128_GCM)
        );
        assert_eq!(
            select(&[SrtpProfile::AES128_CM_SHA1_80]),
            Some(SrtpProfile::AES128_CM_SHA1_80)
        );
        assert_eq!(select(&[]), None);
    }
}
