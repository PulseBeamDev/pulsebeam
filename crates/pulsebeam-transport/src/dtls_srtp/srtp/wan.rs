//! Fabric WAN protection for one authenticated DTLS-SRTP key epoch.
//!
//! This does not select a carrier, authorize a route, qualify a path, or submit
//! to a socket. The association owner must retire all windows with the epoch.

use std::{collections::BTreeMap, num::NonZeroU32};

pub mod feedback;
pub mod handoff;
pub mod path;
pub mod session;

use super::{
    CryptoProvider, KeyingMaterial, ReplayLimits, RtpHeaderFacts, SrtpError, SrtpLayer,
    SrtpProfile, decrypt_rtp,
};

pub const MAX_DATAGRAM_BYTES: usize = pulsebeam_dtls::FABRIC_WAN_DATAGRAM_BYTES;
const RTP_TAG_BYTES: usize = 16;
const RTCP_TRAILER_BYTES: usize = 20;
const SEQUENCE_HALF: u16 = 0x8000;
const POSITION_MASK: u64 = 0x7fff;
const BITMAP_WORDS: usize = 512;
const FEEDBACK_SSRC: u32 = 0x0a0b_0c0d;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Direction {
    Send,
    Receive,
}

/// Independent per-direction quotas. Retired SSRCs still count against the
/// epoch quota even though their bitmaps are released.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub active_windows: usize,
    pub epoch_windows: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    Binding,
    Exhausted,
    Protection(SrtpError),
}

impl From<SrtpError> for Error {
    fn from(value: SrtpError) -> Self {
        Self::Protection(value)
    }
}

pub struct Protector {
    crypto: SrtpLayer,
    send: Windows,
    receive: Windows,
    limits: Limits,
}

impl Protector {
    /// Consume the authenticated exporter output from the configured Fabric WAN
    /// association. Do not recreate the protector on a path change.
    pub fn from_dtls_exporter(
        exporter: pulsebeam_dtls::KeyingMaterial,
        profile: pulsebeam_dtls::SrtpProfile,
        role: pulsebeam_dtls::Role,
        limits: Limits,
    ) -> Result<Self, Error> {
        if profile != pulsebeam_dtls::SrtpProfile::AEAD_AES_256_GCM {
            return Err(SrtpError::UnsupportedProfile.into());
        }
        Self::new(
            &exporter,
            SrtpProfile::AEAD_AES_256_GCM,
            role == pulsebeam_dtls::Role::Client,
            &str0m::crypto::from_feature_flags(),
            limits,
        )
    }

    fn new(
        exporter: &[u8],
        profile: SrtpProfile,
        active: bool,
        provider: &CryptoProvider,
        limits: Limits,
    ) -> Result<Self, Error> {
        if profile != SrtpProfile::AEAD_AES_256_GCM {
            return Err(SrtpError::UnsupportedProfile.into());
        }
        if exporter.len() != profile.keying_material_len() {
            return Err(SrtpError::InvalidPacket.into());
        }
        if limits.active_windows == 0 || limits.active_windows > limits.epoch_windows {
            return Err(Error::Binding);
        }
        Ok(Self {
            crypto: SrtpLayer::new(
                KeyingMaterial::new(exporter),
                profile,
                active,
                provider,
                ReplayLimits {
                    rtp_bytes: 0,
                    rtcp_bytes: super::RTCP_REPLAY_STATE_BYTES,
                },
                0,
            )?,
            send: Windows::default(),
            receive: Windows::default(),
            limits,
        })
    }

    pub fn install(
        &mut self,
        direction: Direction,
        ssrc: NonZeroU32,
        start: u64,
    ) -> Result<(), Error> {
        if start & POSITION_MASK != 0 || ssrc.get() == FEEDBACK_SSRC {
            return Err(Error::Binding);
        }
        let limits = self.limits;
        let windows = self.windows(direction);
        if windows.entries.contains_key(&ssrc.get()) {
            return Err(Error::Binding);
        }
        if windows.active >= limits.active_windows || windows.entries.len() >= limits.epoch_windows
        {
            return Err(Error::Exhausted);
        }
        windows.entries.insert(
            ssrc.get(),
            Some(Window {
                upper: start & u64::from(SEQUENCE_HALF) != 0,
                used: Box::new([0; BITMAP_WORDS]),
            }),
        );
        windows.active = windows.active.saturating_add(1);
        Ok(())
    }

    pub fn retire(&mut self, direction: Direction, ssrc: NonZeroU32) {
        let windows = self.windows(direction);
        if windows
            .entries
            .get_mut(&ssrc.get())
            .and_then(Option::take)
            .is_some()
        {
            windows.active = windows.active.saturating_sub(1);
        }
    }

    fn windows(&mut self, direction: Direction) -> &mut Windows {
        match direction {
            Direction::Send => &mut self.send,
            Direction::Receive => &mut self.receive,
        }
    }

    #[cfg(test)]
    fn seal_rtp(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        self.seal_rtp_with_route(plaintext).map(|(bytes, _)| bytes)
    }

    fn seal_rtp_with_route(
        &mut self,
        plaintext: &[u8],
    ) -> Result<(Vec<u8>, RtpHeaderFacts), Error> {
        let layout = self
            .crypto
            .rtp_layout(plaintext.len())?
            .check_limit(Some(MAX_DATAGRAM_BYTES))?;
        let header = rtp_header(plaintext, 0)?;
        let position = self.send.unused_position(&header)?;
        let mut output = vec![0; layout.initialized_bytes];
        // The nonce is spent when attempting AEAD, regardless of later handoff.
        position.commit();
        let written = self.crypto.protect_rtp_into(
            plaintext,
            u64::from(header.sequence),
            Some(MAX_DATAGRAM_BYTES),
            &mut output,
        )?;
        output.truncate(written);
        Ok((output, header))
    }

    pub fn open_rtp(&mut self, sealed: &[u8]) -> Result<Vec<u8>, Error> {
        check_wire_len(sealed.len())?;
        let header = rtp_header(sealed, RTP_TAG_BYTES)?;
        let position = self.receive.unused_position(&header)?;
        let (plaintext, _) = decrypt_rtp(
            &mut self.crypto.rx,
            sealed,
            &header,
            u64::from(header.sequence),
        )?;
        position.commit();
        Ok(plaintext)
    }

    fn seal_feedback(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        feedback::Report::parse(plaintext)?;
        let layout = self
            .crypto
            .rtcp_layout(plaintext.len())?
            .check_limit(Some(MAX_DATAGRAM_BYTES))?;
        let mut output = vec![0; layout.initialized_bytes];
        let written =
            self.crypto
                .protect_rtcp_into(plaintext, Some(MAX_DATAGRAM_BYTES), &mut output)?;
        output.truncate(written);
        Ok(output)
    }

    pub fn open_feedback(&mut self, sealed: &[u8]) -> Result<Vec<u8>, Error> {
        let (plaintext, _) = self.unseal_feedback(sealed)?;
        feedback::Report::parse(&plaintext)?;
        Ok(plaintext)
    }

    fn unseal_feedback(&mut self, sealed: &[u8]) -> Result<(Vec<u8>, u32), Error> {
        check_wire_len(sealed.len())?;
        feedback::header(sealed, sealed.len().saturating_sub(RTCP_TRAILER_BYTES))?;
        let (_, index) = sealed.split_last_chunk().ok_or(SrtpError::InvalidPacket)?;
        let index = u32::from_be_bytes(*index);
        if index & 0x8000_0000 == 0 {
            return Err(SrtpError::InvalidPacket.into());
        }
        let plaintext = self.crypto.unprotect_rtcp(sealed)?;
        Ok((plaintext, index & 0x7fff_ffff))
    }
}

#[derive(Default)]
struct Windows {
    entries: BTreeMap<u32, Option<Window>>,
    active: usize,
}

struct Window {
    upper: bool,
    used: Box<[u64; BITMAP_WORDS]>,
}

struct Position<'a> {
    word: &'a mut u64,
    mask: u64,
}

impl Position<'_> {
    fn commit(self) {
        *self.word |= self.mask;
    }
}

impl Windows {
    fn unused_position(&mut self, header: &RtpHeaderFacts) -> Result<Position<'_>, Error> {
        let window = self
            .entries
            .get_mut(&header.ssrc)
            .and_then(Option::as_mut)
            .ok_or(Error::Binding)?;
        if window.upper != (header.sequence & SEQUENCE_HALF != 0) {
            return Err(Error::Binding);
        }
        let offset = usize::from(header.sequence & !SEQUENCE_HALF);
        let word = window.used.get_mut(offset / 64).ok_or(Error::Binding)?;
        let mask = 1 << (offset % 64);
        if *word & mask != 0 {
            return Err(SrtpError::Replay.into());
        }
        Ok(Position { word, mask })
    }
}

fn rtp_header(packet: &[u8], trailer: usize) -> Result<RtpHeaderFacts, SrtpError> {
    let body_len = packet
        .len()
        .checked_sub(trailer)
        .ok_or(SrtpError::InvalidPacket)?;
    let body = packet.get(..body_len).ok_or(SrtpError::InvalidPacket)?;
    if !matches!(body.first(), Some(0x80 | 0x90)) {
        return Err(SrtpError::InvalidPacket);
    }
    let header = RtpHeaderFacts::parse(body)?;
    if !(96..=102).contains(&header.payload_type) {
        return Err(SrtpError::InvalidPacket);
    }
    Ok(header)
}

fn check_wire_len(bytes: usize) -> Result<(), SrtpError> {
    (bytes <= MAX_DATAGRAM_BYTES)
        .then_some(())
        .ok_or(SrtpError::OutputTooLarge)
}

#[cfg(test)]
mod tests;
