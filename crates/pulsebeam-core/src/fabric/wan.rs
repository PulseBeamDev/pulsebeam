//! PRODUCTION WIRE CONTRACT — EDIT WITH EXTREME CARE. Changes can break deployed clients, mixed-version nodes, and production traffic. Require compatibility review and wire-vector tests.
//!
//! Fabric WAN RTP revision 0, [RFC #275 §9.1][rfc]. The authenticated route
//! selects the profile, SSRC, aligned 32,768-position window and allowed RTP
//! extensions. The header has V=2, P=0, CC=0, optional X, M, PT=96+H, low
//! sequence bits, timestamp and SSRC. Fields are big-endian. An RFC 8285 block
//! uses ascending IDs 2–9 and minimum zero padding. The full RTP plaintext is
//! limited to 1,200 bytes before the 16-byte SRTP tag. No format sniffing.
//!
//! Association authorization, SRTP, DATA record projection and DD context are
//! caller-owned. The literal vectors in `wan/tests.rs` pin this layout.
//!
//! [rfc]: https://github.com/PulseBeamDev/pulsebeam/issues/275

use std::num::{NonZeroU16, NonZeroU32};

use super::{
    BorrowedPacket, ExtensionKind, FabricPacket, Form, Header, MAX_HOPS, MAX_WAN_RTP_BYTES,
    Profile, WireError, check_wan_size, wan_size,
};

pub const WINDOW_TUPLE_BYTES: usize = 17;
pub const WINDOW_SPAN: u64 = 0x8000;
const WINDOW_MASK: u64 = WINDOW_SPAN - 1;
const HOP_PT: u8 = 96;
pub const FEEDBACK_SSRC: u32 = 0x0a0b_0c0d;
const KNOWN_EXTENSIONS: u16 = 0x03fc;
const SOURCE_BUDGET_ID: u8 = 10;
const GCM_TAG_BYTES: usize = 16;

/// Immutable wire facts for one installed forward or repair window. The owner
/// also binds its association, direction, purpose and key lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Window {
    start: u64,
    ssrc: NonZeroU32,
    profile: Profile,
    forms: u8,
    emission: u16,
    source_budget: bool,
}

impl Window {
    pub fn from_tuple(tuple: [u8; WINDOW_TUPLE_BYTES], emission: u16) -> Result<Self, WireError> {
        let [
            3,
            1,
            revision @ (0 | 1),
            profile,
            a,
            b,
            c,
            d,
            e,
            f,
            g,
            h,
            forms,
            i,
            j,
            k,
            l,
        ] = tuple
        else {
            return Err(WireError::Window);
        };
        let profile = match profile {
            1 => Profile::Opus,
            2 => Profile::H264,
            3 => Profile::Av1,
            4 => Profile::Data,
            _ => return Err(WireError::PayloadType),
        };
        let start = u64::from_be_bytes([a, b, c, d, e, f, g, h]);
        let ssrc = u32::from_be_bytes([i, j, k, l]);
        let window = Self::new(
            profile,
            start,
            forms,
            NonZeroU32::new(ssrc).ok_or(WireError::RouteKey)?,
            emission,
        )?;
        match revision {
            0 => Ok(window),
            _ => window.with_opus_source_budget(),
        }
    }

    pub fn new(
        profile: Profile,
        start: u64,
        forms: u8,
        ssrc: NonZeroU32,
        emission: u16,
    ) -> Result<Self, WireError> {
        if ssrc.get() == FEEDBACK_SSRC {
            return Err(WireError::RouteKey);
        }
        if start & WINDOW_MASK != 0
            || forms & !3 != 0
            || emission & !KNOWN_EXTENSIONS != 0
            || (forms == 0 && emission != 0)
        {
            return Err(WireError::Window);
        }
        for id in 2..=9 {
            let kind = ExtensionKind::from_id(id)?;
            if emission & extension_bit(id) != 0 && !kind.is_permitted_on(profile) {
                return Err(WireError::ExtensionProfile);
            }
        }
        Ok(Self {
            start,
            ssrc,
            profile,
            forms,
            emission,
            source_budget: false,
        })
    }

    /// Only use on an Opus window with explicit peer support for this revision.
    pub fn with_opus_source_budget(mut self) -> Result<Self, WireError> {
        if self.profile != Profile::Opus {
            return Err(WireError::Window);
        }
        self.source_budget = true;
        Ok(self)
    }

    pub const fn to_tuple(self) -> [u8; WINDOW_TUPLE_BYTES] {
        let profile = match self.profile {
            Profile::Opus => 1,
            Profile::H264 => 2,
            Profile::Av1 => 3,
            Profile::Data => 4,
        };
        let [a, b, c, d, e, f, g, h] = self.start.to_be_bytes();
        let [i, j, k, l] = self.ssrc.get().to_be_bytes();
        [
            3,
            1,
            self.source_budget as u8,
            profile,
            a,
            b,
            c,
            d,
            e,
            f,
            g,
            h,
            self.forms,
            i,
            j,
            k,
            l,
        ]
    }

    /// Reuse the exact profile and extension policy for a distinct repair SSRC.
    pub fn repair_window(self, position: u64, ssrc: NonZeroU32) -> Result<Self, WireError> {
        if ssrc == self.ssrc {
            return Err(WireError::RouteKey);
        }
        let mut repair = Self::new(
            self.profile,
            Self::start_for(position),
            self.forms,
            ssrc,
            self.emission,
        )?;
        repair.source_budget = self.source_budget;
        Ok(repair)
    }

    pub const fn ssrc(self) -> NonZeroU32 {
        self.ssrc
    }

    pub const fn start_for(position: u64) -> u64 {
        position & !WINDOW_MASK
    }

    pub const fn forms(self) -> u8 {
        self.forms
    }

    pub const fn start(self) -> u64 {
        self.start
    }

    pub const fn profile(self) -> Profile {
        self.profile
    }

    pub const fn carries_opus_source_budget(self) -> bool {
        self.source_budget
    }

    pub const fn emission(self) -> u16 {
        self.emission
    }

    /// Resolve a sequence against the installed window, without ROC inference.
    pub fn position(self, sequence: u16) -> Result<u64, WireError> {
        let position = (self.start & !0xffff) | u64::from(sequence);
        self.contains(position)
            .then_some(position)
            .ok_or(WireError::Window)
    }

    pub const fn contains(self, position: u64) -> bool {
        position & !WINDOW_MASK == self.start
    }

    /// Decode one authenticated RTP plaintext under the exact window binding.
    pub fn decode(self, bytes: &[u8]) -> Result<(u64, BorrowedPacket<'_>), WireError> {
        if bytes.len() > MAX_WAN_RTP_BYTES {
            return Err(WireError::Oversize);
        }
        let [
            first @ (0x80 | 0x90),
            pt,
            a,
            b,
            c,
            d,
            e,
            f,
            i,
            j,
            k,
            l,
            body @ ..,
        ] = bytes
        else {
            return Err(WireError::Header);
        };
        if u32::from_be_bytes([*i, *j, *k, *l]) != self.ssrc.get() {
            return Err(WireError::RouteKey);
        }
        let position = self.position(u16::from_be_bytes([*a, *b]))?;
        let hops = (pt & 0x7f)
            .checked_sub(HOP_PT)
            .filter(|hops| *hops <= MAX_HOPS)
            .ok_or(WireError::PayloadType)?;
        if hops == MAX_HOPS {
            return Err(WireError::NetworkHop);
        }
        let (form, optional, payload, source_delta) = if *first == 0x90 {
            let [a, b, c, d, rest @ ..] = body else {
                return Err(WireError::ExtensionLayout);
            };
            let form = Form::from_profile([*a, *b])?;
            let block_len = usize::from(u16::from_be_bytes([*c, *d])).saturating_mul(4);
            let (block, payload) = rest
                .split_at_checked(block_len)
                .ok_or(WireError::ExtensionLayout)?;
            let (optional, source_delta) = self.optional_bytes(form, block)?;
            (form, optional, payload, source_delta)
        } else {
            (Form::One, &[][..], body, None)
        };
        self.profile.validate_payload(payload.len())?;
        let protected = bytes.len().saturating_add(GCM_TAG_BYTES);
        let wan_received_bytes = u16::try_from(protected).ok().and_then(NonZeroU16::new);
        let source_protected_bytes = if let Some(delta) = source_delta {
            if wan_size(optional.len(), payload.len()) != bytes.len() {
                return Err(WireError::ExtensionLayout);
            }
            let source = protected.saturating_add(usize::from(delta));
            Some(
                NonZeroU16::new(u16::try_from(source).map_err(|_| WireError::Oversize)?)
                    .ok_or(WireError::Oversize)?,
            )
        } else {
            None
        };
        Ok((
            position,
            FabricPacket {
                header: Header {
                    profile: self.profile,
                    marker: pt & 0x80 != 0,
                    sequence: low32(position),
                    timestamp: u32::from_be_bytes([*c, *d, *e, *f]),
                    ingress_ntp32: 0,
                },
                hops,
                form,
                optional,
                payload,
                source_protected_bytes,
                wan_received_bytes,
            },
        ))
    }

    /// Write a new hop. Retries on that hop use the same image and do not
    /// decrement again. SRTP index ownership is separate from this codec.
    pub fn write<P: AsRef<[u8]>, E: AsRef<[u8]>>(
        self,
        position: u64,
        packet: &FabricPacket<P, E>,
        output: &mut [u8],
    ) -> Result<usize, WireError> {
        let header = packet.header();
        if !self.contains(position)
            || header.sequence != low32(position)
            || header.profile != self.profile
        {
            return Err(WireError::Window);
        }
        let hops = packet.hops.checked_sub(1).ok_or(WireError::HopExhausted)?;
        let optional = packet.optional.as_ref();
        for ext in packet.extensions() {
            self.check_extension(ext.kind as u8, ext.value)?;
        }
        if !optional.is_empty() && !self.allows_form(packet.form) {
            return Err(WireError::ExtensionLayout);
        }
        let payload = packet.payload();
        let len = check_wan_size(optional.len(), payload.len())?;
        // The WAN profile seals this RTP image with a full GCM tag and no MKI.
        let protected = len.saturating_add(GCM_TAG_BYTES);
        if packet
            .source_protected_bytes()
            .or_else(|| packet.wan_received_bytes())
            .is_some_and(|source| protected > usize::from(source.get()))
        {
            return Err(WireError::Oversize);
        }
        let header_len = wan_size(optional.len(), 0);
        let (rtp, payload_out) = output
            .get_mut(..len)
            .and_then(|out| out.split_at_mut_checked(header_len))
            .ok_or(WireError::OutputTooSmall)?;
        let [_, _, a, b] = header.sequence.to_be_bytes();
        let [c, d, e, f] = header.timestamp.to_be_bytes();
        let [i, j, k, l] = self.ssrc.get().to_be_bytes();
        let pt = HOP_PT.saturating_add(hops) | (u8::from(header.marker) << 7);
        let (base, block) = rtp
            .split_at_mut_checked(12)
            .ok_or(WireError::OutputTooSmall)?;
        base.copy_from_slice(&[
            if optional.is_empty() { 0x80 } else { 0x90 },
            pt,
            a,
            b,
            c,
            d,
            e,
            f,
            i,
            j,
            k,
            l,
        ]);
        if let [a, b, c, d, body @ ..] = block {
            [*a, *b] = packet.form.profile();
            [*c, *d] = u16::try_from(body.len() / 4)
                .map_err(|_| WireError::Oversize)?
                .to_be_bytes();
            let (dst, pad) = body
                .split_at_mut_checked(optional.len())
                .ok_or(WireError::OutputTooSmall)?;
            dst.copy_from_slice(optional);
            pad.fill(0);
            if self.source_budget
                && let Some(delta) = packet
                    .source_protected_bytes()
                    .and_then(|source| usize::from(source.get()).checked_sub(protected))
                    .and_then(|delta| u8::try_from(delta).ok())
                    .filter(|&delta| delta != 0)
            {
                match (packet.form, pad) {
                    (Form::One, [first, value, ..]) => {
                        *first = SOURCE_BUDGET_ID << 4;
                        *value = delta;
                    }
                    (Form::Two, [id, len, value, ..]) => {
                        *id = SOURCE_BUDGET_ID;
                        *len = 1;
                        *value = delta;
                    }
                    _ => {}
                }
            }
        }
        payload_out.copy_from_slice(payload);
        Ok(len)
    }

    const fn allows_form(self, form: Form) -> bool {
        self.forms
            & (match form {
                Form::One => 1,
                Form::Two => 2,
            })
            != 0
    }

    fn optional_bytes(self, form: Form, block: &[u8]) -> Result<(&[u8], Option<u8>), WireError> {
        if !self.allows_form(form) {
            return Err(WireError::ExtensionLayout);
        }
        let mut rest = block;
        let mut last = 1;
        let mut needs_two = false;
        while rest.first().is_some_and(|byte| *byte != 0) {
            let (id, value, next) = form.read_element(rest)?;
            if id <= last {
                return Err(WireError::ExtensionLayout);
            }
            if self.source_budget && id == SOURCE_BUDGET_ID {
                let [delta @ 1..=u8::MAX] = value else {
                    return Err(WireError::ExtensionValue);
                };
                if last == 1 || next.iter().any(|byte| *byte != 0) {
                    return Err(WireError::ExtensionLayout);
                }
                let optional = block
                    .get(..block.len().saturating_sub(rest.len()))
                    .ok_or(WireError::ExtensionLayout)?;
                return Ok((optional, Some(*delta)));
            }
            self.check_extension(id, value)?;
            needs_two |= value.len() > 16;
            last = id;
            rest = next;
        }
        if last == 1
            || rest.len() > 3
            || rest.iter().any(|byte| *byte != 0)
            || (self.profile == Profile::Data && form == Form::Two && !needs_two)
        {
            return Err(WireError::ExtensionLayout);
        }
        block
            .get(..block.len().saturating_sub(rest.len()))
            .map(|optional| (optional, None))
            .ok_or(WireError::ExtensionLayout)
    }

    fn check_extension(self, id: u8, value: &[u8]) -> Result<(), WireError> {
        let kind = ExtensionKind::from_id(id)?;
        if self.emission & extension_bit(id) == 0 {
            return Err(WireError::ExtensionProfile);
        }
        kind.validate(self.profile, value)
    }
}

fn extension_bit(id: u8) -> u16 {
    1_u16.checked_shl(u32::from(id)).unwrap_or(0)
}

fn low32(position: u64) -> u32 {
    let [_, _, _, _, a, b, c, d] = position.to_be_bytes();
    u32::from_be_bytes([a, b, c, d])
}

#[cfg(test)]
mod tests;
