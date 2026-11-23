//! PRODUCTION WIRE CONTRACT — EDIT WITH EXTREME CARE. Changes can break deployed clients, mixed-version nodes, and production traffic. Require compatibility review and wire-vector tests.
//!
//! Shared packet admission budgets the WAN RTP plaintext (1,200 bytes).
//! The historical image codec below has a separate, unchanged envelope.
//!
//! Historical RTP framing, major 1, registry revisions 0 (IDs 1–3) and 1
//! (IDs 1–9). See [RFC #275 §§2–3 and Appendix A][rfc] and the `fabric/tests`
//! golden vectors. No per-packet version or fallback; authenticated bindings
//! agree a profile and full emission set before using this codec.
//!
//! Network byte order: `90`, M/PT, seq16, timestamp or DATA label (32 bits),
//! SSRC (32 bits; zero locally), extension profile and word count (16 bits
//! each), extensions with minimum zero alignment, then the opaque payload.
//! Required ID 1 carries NTP32, the high half of seq32 and H in the high
//! three bits of its last octet. IDs are strictly ascending. Use `BEDE`
//! unless any value exceeds 16 bytes, then use `1000` for every element.
//! At most 1,152 bytes including framing. A network send changes only SSRC and
//! H; a network receive restores local SSRC zero without restoring H. H=7
//! only appears before the first network hop.
//!
//! This codec validates the envelope, not source/route authority, DD template
//! context, or DATA record contents. Endpoint adapters own those checks. PT99
//! carries one complete record verbatim, with no DD and no prefix or suffix.
//!
//! [rfc]: https://github.com/PulseBeamDev/pulsebeam/issues/275

use std::num::{NonZeroU16, NonZeroU32};

use arrayvec::ArrayVec;

pub const MAJOR: u8 = 1;
pub const REGISTRY_REVISION: u8 = 1;
pub const MAX_PACKET_BYTES: usize = 1_152;
// Pinned Native Opus input plus fixed RTP header and the retained audio-level block.
pub const MAX_WAN_RTP_BYTES: usize = 1_200 + 12 + 8;

fn legacy_extension_allowed(profile: Profile, kind: ExtensionKind) -> bool {
    (profile, kind) != (Profile::Data, ExtensionKind::DependencyDescriptor)
}
const RTP_BASE_BYTES: usize = 12;
const EXTENSION_PREFIX_BYTES: usize = 4;
const FIXED_HEADER: u8 = 0x90;
const PREFIX_BYTES: usize = 16;
const CORE_BYTES: usize = 7;
const MAX_HOPS: u8 = 7;
const DATA_RECORD_MIN: usize = 22;
const DATA_RECORD_MAX: usize = 1_058;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Profile {
    Opus = 96,
    H264 = 97,
    Av1 = 98,
    Data = 99,
}

impl Profile {
    pub const fn clock_rate(self) -> u32 {
        match self {
            Self::Opus => 48_000,
            Self::H264 | Self::Av1 | Self::Data => 90_000,
        }
    }

    pub fn from_pt(pt: u8) -> Result<Self, WireError> {
        match pt {
            96 => Ok(Self::Opus),
            97 => Ok(Self::H264),
            98 => Ok(Self::Av1),
            99 => Ok(Self::Data),
            _ => Err(WireError::PayloadType),
        }
    }

    fn validate_payload(self, len: usize) -> Result<(), WireError> {
        let valid = match self {
            Self::Data => (DATA_RECORD_MIN..=DATA_RECORD_MAX).contains(&len),
            _ => len != 0,
        };
        valid.then_some(()).ok_or(WireError::PayloadLength)
    }
}

/// Optional registered elements; the codec always supplies ID 1 itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ExtensionKind {
    DependencyDescriptor = 2,
    AbsoluteCaptureTime = 3,
    AudioLevel = 4,
    VideoOrientation = 5,
    VideoLayersAllocation = 6,
    VideoContentType = 7,
    ColorSpace = 8,
    PlayoutDelay = 9,
}

impl ExtensionKind {
    pub const fn minimum_revision(self) -> u8 {
        match self {
            Self::DependencyDescriptor | Self::AbsoluteCaptureTime => 0,
            _ => 1,
        }
    }

    pub fn from_id(id: u8) -> Result<Self, WireError> {
        match id {
            2 => Ok(Self::DependencyDescriptor),
            3 => Ok(Self::AbsoluteCaptureTime),
            4 => Ok(Self::AudioLevel),
            5 => Ok(Self::VideoOrientation),
            6 => Ok(Self::VideoLayersAllocation),
            7 => Ok(Self::VideoContentType),
            8 => Ok(Self::ColorSpace),
            9 => Ok(Self::PlayoutDelay),
            _ => Err(WireError::ExtensionId),
        }
    }

    pub fn is_permitted_on(self, profile: Profile) -> bool {
        match self {
            Self::DependencyDescriptor | Self::AbsoluteCaptureTime => true,
            Self::AudioLevel => profile == Profile::Opus,
            _ => matches!(profile, Profile::H264 | Profile::Av1),
        }
    }

    pub fn validate(self, profile: Profile, value: &[u8]) -> Result<(), WireError> {
        if !self.is_permitted_on(profile) {
            return Err(WireError::ExtensionProfile);
        }
        let valid = match self {
            Self::DependencyDescriptor if profile == Profile::Data => {
                (3..=96).contains(&value.len())
            }
            Self::DependencyDescriptor => (3..=255).contains(&value.len()),
            Self::AbsoluteCaptureTime => matches!(value.len(), 8 | 16),
            Self::AudioLevel => value.len() == 1,
            Self::VideoOrientation => matches!(value, [bits] if *bits <= 0x0f),
            Self::VideoLayersAllocation => crate::vla::validate(value).is_ok(),
            Self::VideoContentType => matches!(value, [0 | 1]),
            Self::ColorSpace => matches!(value.len(), 4 | 28),
            Self::PlayoutDelay => match *value {
                [a, b, c] => u16::from_be_bytes([a, b]) >> 4 <= u16::from_be_bytes([b & 15, c]),
                _ => false,
            },
        };
        valid.then_some(()).ok_or(WireError::ExtensionValue)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extension<'a> {
    pub kind: ExtensionKind,
    pub value: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub profile: Profile,
    pub marker: bool,
    pub sequence: u32,
    pub timestamp: u32,
    pub ingress_ntp32: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("Fabric packet exceeds its envelope")]
    Oversize,
    #[error("invalid canonical RTP header")]
    Header,
    #[error("unexpected local/network SSRC")]
    RouteKey,
    #[error("unassigned Fabric payload type")]
    PayloadType,
    #[error("invalid payload length for the Fabric profile")]
    PayloadLength,
    #[error("invalid extension length, order, packing or padding")]
    ExtensionLayout,
    #[error("missing or invalid required Fabric metadata")]
    CoreMetadata,
    #[error("unknown Fabric extension ID")]
    ExtensionId,
    #[error("extension is not permitted on the Fabric payload profile")]
    ExtensionProfile,
    #[error("invalid Fabric extension value")]
    ExtensionValue,
    #[error("a network image must have consumed at least one hop")]
    NetworkHop,
    #[error("packet position or envelope does not match the installed WAN window")]
    Window,
    #[error("no remaining network hops")]
    HopExhausted,
    #[error("output buffer too small for Fabric image")]
    OutputTooSmall,
}

/// One validated Fabric value. The route key is not part of the local value.
/// Payload storage may be borrowed or owned by the dataplane; the holder must
/// keep it immutable. The private metadata block contains just the optional
/// elements in canonical form, without alignment padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FabricPacket<P, E = Box<[u8]>> {
    header: Header,
    hops: u8,
    form: Form,
    optional: E,
    payload: P,
    source_protected_bytes: Option<NonZeroU16>,
    wan_received_bytes: Option<NonZeroU16>,
}

impl FabricPacket<&[u8]> {
    fn copy_payload(self) -> FabricPacket<Box<[u8]>> {
        FabricPacket {
            header: self.header,
            hops: self.hops,
            form: self.form,
            optional: self.optional,
            payload: self.payload.into(),
            source_protected_bytes: self.source_protected_bytes,
            wan_received_bytes: self.wan_received_bytes,
        }
    }
}

impl<P: AsRef<[u8]>> FabricPacket<P> {
    /// Validates and packs a fresh source packet, without copying its payload.
    pub fn new(
        header: Header,
        extensions: &[Extension<'_>],
        payload: P,
    ) -> Result<Self, WireError> {
        Self::pack(header, extensions, payload, None)
    }

    fn pack(
        header: Header,
        extensions: &[Extension<'_>],
        payload: P,
        width: Option<ExtensionWidth>,
    ) -> Result<Self, WireError> {
        let mut sorted = ArrayVec::<_, 8>::new();
        for ext in extensions {
            sorted
                .try_push(*ext)
                .map_err(|_| WireError::ExtensionLayout)?;
            ext.kind.validate(header.profile, ext.value)?;
        }
        sorted.sort_unstable_by_key(|ext| ext.kind);
        if sorted
            .windows(2)
            .any(|pair| matches!(pair, [a, b] if a.kind == b.kind))
        {
            return Err(WireError::ExtensionLayout);
        }
        let needs_two = sorted.iter().any(|ext| ext.value.len() > 16);
        let form = match (width, needs_two) {
            (Some(Form::One), true) => return Err(WireError::ExtensionLayout),
            (Some(Form::Two), _) | (None, true) => Form::Two,
            _ => Form::One,
        };
        let optional_len = sorted.iter().fold(0_usize, |len, ext| {
            len.saturating_add(form.element_bytes(ext.value.len()))
        });
        let payload_len = payload.as_ref().len();
        header.profile.validate_payload(payload_len)?;
        let mut optional = Vec::with_capacity(optional_len);
        for ext in sorted {
            let len = u8::try_from(ext.value.len()).map_err(|_| WireError::ExtensionValue)?;
            let (first, second) = form.element_header(ext.kind as u8, len);
            optional.push(first);
            optional.extend(second);
            optional.extend_from_slice(ext.value);
        }
        Ok(Self {
            header,
            hops: MAX_HOPS,
            form,
            optional: optional.into_boxed_slice(),
            payload,
            source_protected_bytes: None,
            wan_received_bytes: None,
        })
    }
}

pub type BorrowedPacket<'a> = FabricPacket<&'a [u8], &'a [u8]>;

impl<'a> BorrowedPacket<'a> {
    /// Move a validated borrowed image into owner-local immutable storage.
    pub fn into_owned(self) -> FabricPacket<Box<[u8]>> {
        FabricPacket {
            header: self.header,
            hops: self.hops,
            form: self.form,
            optional: self.optional.into(),
            payload: self.payload.into(),
            source_protected_bytes: self.source_protected_bytes,
            wan_received_bytes: self.wan_received_bytes,
        }
    }

    pub fn decode_local(image: &'a [u8]) -> Result<Self, WireError> {
        let (packet, key) = Self::decode(image)?;
        if key.is_some() {
            return Err(WireError::RouteKey);
        }
        Ok(packet)
    }

    /// Read the route label before resolving an authenticated receive binding.
    /// This is not packet validation or authority to deliver the image.
    pub fn network_route_key(image: &[u8]) -> Result<NonZeroU32, WireError> {
        if image.len() > MAX_PACKET_BYTES {
            return Err(WireError::Oversize);
        }
        if image.first() != Some(&FIXED_HEADER) || image.len() < PREFIX_BYTES {
            return Err(WireError::Header);
        }
        let bytes = image
            .get(8..12)
            .and_then(|key| key.try_into().ok())
            .ok_or(WireError::Header)?;
        NonZeroU32::new(u32::from_be_bytes(bytes)).ok_or(WireError::RouteKey)
    }

    /// Decodes an already authenticated carrier message. The caller must
    /// resolve and authorize the returned key on that carrier association.
    pub fn decode_network(image: &'a [u8]) -> Result<(Self, NonZeroU32), WireError> {
        let (packet, key) = Self::decode(image)?;
        let key = key.ok_or(WireError::RouteKey)?;
        if packet.hops == MAX_HOPS {
            return Err(WireError::NetworkHop);
        }
        Ok((packet, key))
    }

    fn decode(image: &'a [u8]) -> Result<(Self, Option<NonZeroU32>), WireError> {
        if image.len() > MAX_PACKET_BYTES {
            return Err(WireError::Oversize);
        }
        let Some((
            [
                FIXED_HEADER,
                pt,
                q1,
                q0,
                t3,
                t2,
                t1,
                t0,
                k3,
                k2,
                k1,
                k0,
                f1,
                f0,
                w1,
                w0,
            ],
            body,
        )) = image.split_first_chunk::<PREFIX_BYTES>()
        else {
            return Err(WireError::Header);
        };
        let profile = Profile::from_pt(pt & 0x7f)?;
        let timestamp = u32::from_be_bytes([*t3, *t2, *t1, *t0]);
        let key = NonZeroU32::new(u32::from_be_bytes([*k3, *k2, *k1, *k0]));
        let form = Form::from_profile([*f1, *f0])?;
        let block_len = usize::from(u16::from_be_bytes([*w1, *w0])).saturating_mul(4);
        let (block, payload) = body
            .split_at_checked(block_len)
            .ok_or(WireError::ExtensionLayout)?;
        profile.validate_payload(payload.len())?;
        let (1, [s3, s2, s1, s0, q3, q2, hop], optional_and_pad) = form.read_element(block)? else {
            return Err(WireError::CoreMetadata);
        };
        if hop & 31 != 0 {
            return Err(WireError::CoreMetadata);
        }
        let mut remaining = optional_and_pad;
        let mut last = 1;
        let mut needs_two = false;
        while remaining.first().is_some_and(|byte| *byte != 0) {
            let (id, value, rest) = form.read_element(remaining)?;
            if id <= last {
                return Err(WireError::ExtensionLayout);
            }
            let kind = ExtensionKind::from_id(id)?;
            if !legacy_extension_allowed(profile, kind) {
                return Err(WireError::ExtensionProfile);
            }
            kind.validate(profile, value)?;
            needs_two |= value.len() > 16;
            last = id;
            remaining = rest;
        }
        if remaining.len() > 3
            || remaining.iter().any(|byte| *byte != 0)
            || (form == Form::Two && !needs_two)
        {
            return Err(WireError::ExtensionLayout);
        }
        let optional = optional_and_pad
            .get(..optional_and_pad.len().saturating_sub(remaining.len()))
            .ok_or(WireError::ExtensionLayout)?;
        Ok((
            Self {
                header: Header {
                    profile,
                    marker: pt & 0x80 != 0,
                    sequence: u32::from_be_bytes([*q3, *q2, *q1, *q0]),
                    timestamp,
                    ingress_ntp32: u32::from_be_bytes([*s3, *s2, *s1, *s0]),
                },
                hops: hop >> 5,
                form,
                optional,
                payload,
                source_protected_bytes: None,
                wan_received_bytes: None,
            },
            key,
        ))
    }
}

impl<P: AsRef<[u8]>, E: AsRef<[u8]>> FabricPacket<P, E> {
    pub const fn header(&self) -> Header {
        self.header
    }

    pub const fn remaining_hops(&self) -> u8 {
        self.hops
    }

    /// Compare source facts, independent of diagnostic stamp, hop and packing.
    pub fn same_content(&self, other: &Self) -> bool {
        self.header.profile == other.header.profile
            && self.header.marker == other.header.marker
            && self.header.sequence == other.header.sequence
            && self.header.timestamp == other.header.timestamp
            && self.payload() == other.payload()
            && if self.form == other.form {
                self.optional.as_ref() == other.optional.as_ref()
            } else {
                self.extensions()
                    .map(|ext| (ext.kind, ext.value))
                    .eq(other.extensions().map(|ext| (ext.kind, ext.value)))
            }
    }

    /// Borrow the immutable packet body with a hop count retained from the
    /// actual admitted arrival, rather than from the stored identity copy.
    pub fn arrival_view(&self, hops: u8) -> Option<FabricPacket<&[u8], &[u8]>> {
        if hops > MAX_HOPS {
            return None;
        }
        Some(FabricPacket {
            header: self.header,
            hops,
            form: self.form,
            optional: self.optional.as_ref(),
            payload: self.payload(),
            source_protected_bytes: self.source_protected_bytes,
            wan_received_bytes: self.wan_received_bytes,
        })
    }

    /// Associate the authenticated publisher primary size with this in-process copy.
    /// Neither Fabric wire codec carries this evidence.
    pub fn with_source_protected_bytes(mut self, bytes: NonZeroU16) -> Self {
        self.source_protected_bytes = Some(bytes);
        self
    }

    pub const fn source_protected_bytes(&self) -> Option<NonZeroU16> {
        self.source_protected_bytes
    }

    pub const fn wan_received_bytes(&self) -> Option<NonZeroU16> {
        self.wan_received_bytes
    }

    pub fn payload(&self) -> &[u8] {
        self.payload.as_ref()
    }

    pub fn extensions(&self) -> impl Iterator<Item = Extension<'_>> {
        let mut rest = self.optional.as_ref();
        std::iter::from_fn(move || {
            let (id, value, next) = self.form.read_element(rest).ok()?;
            rest = next;
            Some(Extension {
                kind: ExtensionKind::from_id(id).ok()?,
                value,
            })
        })
    }

    pub fn wan_plaintext_len(&self) -> usize {
        wan_size(self.optional.as_ref().len(), self.payload().len())
    }

    pub fn legacy_wire_len(&self) -> Result<usize, WireError> {
        let (form, optional_len) = self.legacy_layout()?;
        check_size(form.block_bytes(optional_len), self.payload().len())
    }

    fn legacy_layout(&self) -> Result<(Form, usize), WireError> {
        if self
            .extensions()
            .any(|ext| !legacy_extension_allowed(self.header.profile, ext.kind))
        {
            return Err(WireError::ExtensionProfile);
        }
        let len = self.optional.as_ref().len();
        if self.form == Form::Two && !self.extensions().any(|ext| ext.value.len() > 16) {
            Ok((Form::One, len.saturating_sub(self.extensions().count())))
        } else {
            Ok((self.form, len))
        }
    }

    pub fn write_local(&self, output: &mut [u8]) -> Result<usize, WireError> {
        self.write_image(0, self.hops, output)
    }

    /// Writes one network attempt image. For same-hop retries, retain the
    /// image instead of consuming another hop from its decoded value.
    pub fn write_network(&self, key: NonZeroU32, output: &mut [u8]) -> Result<usize, WireError> {
        let hops = self.hops.checked_sub(1).ok_or(WireError::HopExhausted)?;
        self.write_image(key.get(), hops, output)
    }

    fn write_image(&self, key: u32, hops: u8, output: &mut [u8]) -> Result<usize, WireError> {
        let optional = self.optional.as_ref();
        let (form, optional_len) = self.legacy_layout()?;
        let block_len = form.block_bytes(optional_len);
        let payload = self.payload();
        let len = check_size(block_len, payload.len())?;
        let output = output.get_mut(..len).ok_or(WireError::OutputTooSmall)?;
        let (prefix, body) = output
            .split_at_mut_checked(PREFIX_BYTES)
            .ok_or(WireError::OutputTooSmall)?;
        let (block, payload_out) = body
            .split_at_mut_checked(block_len)
            .ok_or(WireError::OutputTooSmall)?;
        let [t3, t2, t1, t0] = self.header.timestamp.to_be_bytes();
        let [q3, q2, q1, q0] = self.header.sequence.to_be_bytes();
        let [k3, k2, k1, k0] = key.to_be_bytes();
        let [f1, f0] = form.profile();
        let [w1, w0] = u16::try_from(block_len / 4)
            .map_err(|_| WireError::Oversize)?
            .to_be_bytes();
        prefix.copy_from_slice(&[
            FIXED_HEADER,
            (u8::from(self.header.marker) << 7) | self.header.profile as u8,
            q1,
            q0,
            t3,
            t2,
            t1,
            t0,
            k3,
            k2,
            k1,
            k0,
            f1,
            f0,
            w1,
            w0,
        ]);
        let [s3, s2, s1, s0] = self.header.ingress_ntp32.to_be_bytes();
        let core = [s3, s2, s1, s0, q3, q2, hops << 5];
        let (first, second) = form.element_header(1, 7);
        let (core_header, tail) = block
            .split_at_mut_checked(form.header_bytes())
            .ok_or(WireError::OutputTooSmall)?;
        match (core_header, second) {
            ([x], None) => *x = first,
            ([x, y], Some(b)) => {
                *x = first;
                *y = b;
            }
            _ => return Err(WireError::ExtensionLayout),
        }
        let (core_out, tail) = tail
            .split_at_mut_checked(CORE_BYTES)
            .ok_or(WireError::OutputTooSmall)?;
        core_out.copy_from_slice(&core);
        let (optional_out, pad) = tail
            .split_at_mut_checked(optional_len)
            .ok_or(WireError::OutputTooSmall)?;
        if form == self.form {
            optional_out.copy_from_slice(optional);
        } else {
            let mut rest = optional_out;
            for ext in self.extensions() {
                let len = u8::try_from(ext.value.len()).map_err(|_| WireError::ExtensionValue)?;
                let (first, _) = Form::One.element_header(ext.kind as u8, len);
                let (head, tail) = rest.split_first_mut().ok_or(WireError::OutputTooSmall)?;
                let (value, next) = tail
                    .split_at_mut_checked(ext.value.len())
                    .ok_or(WireError::OutputTooSmall)?;
                *head = first;
                value.copy_from_slice(ext.value);
                rest = next;
            }
        }
        pad.fill(0);
        payload_out.copy_from_slice(payload);
        Ok(len)
    }
}

fn wan_size(optional_len: usize, payload_len: usize) -> usize {
    let extension_block = if optional_len == 0 {
        0
    } else {
        EXTENSION_PREFIX_BYTES.saturating_add(optional_len.div_ceil(4).saturating_mul(4))
    };
    RTP_BASE_BYTES
        .saturating_add(extension_block)
        .saturating_add(payload_len)
}

fn check_wan_size(optional_len: usize, payload_len: usize) -> Result<usize, WireError> {
    let len = wan_size(optional_len, payload_len);
    (len <= MAX_WAN_RTP_BYTES)
        .then_some(len)
        .ok_or(WireError::Oversize)
}

fn check_size(block_len: usize, payload_len: usize) -> Result<usize, WireError> {
    let len = PREFIX_BYTES
        .saturating_add(block_len)
        .saturating_add(payload_len);
    (len <= MAX_PACKET_BYTES)
        .then_some(len)
        .ok_or(WireError::Oversize)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionWidth {
    One,
    Two,
}

use ExtensionWidth as Form;

impl Form {
    const fn profile(self) -> [u8; 2] {
        match self {
            Self::One => [0xbe, 0xde],
            Self::Two => [0x10, 0x00],
        }
    }

    const fn from_profile(bytes: [u8; 2]) -> Result<Self, WireError> {
        match bytes {
            [0xbe, 0xde] => Ok(Self::One),
            [0x10, 0x00] => Ok(Self::Two),
            _ => Err(WireError::ExtensionLayout),
        }
    }

    const fn header_bytes(self) -> usize {
        match self {
            Self::One => 1,
            Self::Two => 2,
        }
    }

    const fn element_bytes(self, len: usize) -> usize {
        self.header_bytes().saturating_add(len)
    }

    const fn block_bytes(self, optional_len: usize) -> usize {
        self.element_bytes(CORE_BYTES)
            .saturating_add(optional_len)
            .div_ceil(4)
            .saturating_mul(4)
    }

    const fn element_header(self, id: u8, len: u8) -> (u8, Option<u8>) {
        match self {
            Self::One => ((id << 4) | len.saturating_sub(1), None),
            Self::Two => (id, Some(len)),
        }
    }

    fn read_element(self, bytes: &[u8]) -> Result<(u8, &[u8], &[u8]), WireError> {
        let (id, len, tail) = match (self, bytes) {
            (Self::One, [first, tail @ ..]) => (first >> 4, (first & 15).saturating_add(1), tail),
            (Self::Two, [id, len, tail @ ..]) => (*id, *len, tail),
            _ => return Err(WireError::ExtensionLayout),
        };
        let (value, rest) = tail
            .split_at_checked(usize::from(len))
            .ok_or(WireError::ExtensionLayout)?;
        Ok((id, value, rest))
    }
}

pub mod media_source;
pub mod wan;

#[cfg(test)]
mod tests;
