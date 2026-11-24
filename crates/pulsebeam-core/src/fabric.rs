//! Typed Fabric packets shared by shard mailboxes, canonical endpoint bindings,
//! and the authenticated WAN projection.

use std::num::NonZeroU16;

use arrayvec::ArrayVec;

pub const MAJOR: u8 = 1;
pub const REGISTRY_REVISION: u8 = 1;
pub const MAX_UDP_PAYLOAD_BYTES: u16 = 1_280 - 40 - 8;
// Provisioned WAN SRTP uses the full 16-byte GCM tag.
pub const MAX_WAN_RTP_BYTES: usize = MAX_UDP_PAYLOAD_BYTES as usize - 16;

const RTP_BASE_BYTES: usize = 12;
const EXTENSION_PREFIX_BYTES: usize = 4;
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

/// Registered metadata. IDs 0 and 1 are not legal packet extensions.
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

    pub const fn from_id(id: u8) -> Result<Self, WireError> {
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

    pub const fn is_permitted_on(self, profile: Profile) -> bool {
        match self {
            Self::DependencyDescriptor | Self::AbsoluteCaptureTime => true,
            Self::AudioLevel => matches!(profile, Profile::Opus),
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

    pub const fn bit(self) -> u16 {
        1_u16.wrapping_shl(self as u32)
    }

    /// Dependency and timing facts must not be erased by a downgrade.
    const fn can_omit(self) -> bool {
        !matches!(self, Self::DependencyDescriptor | Self::AbsoluteCaptureTime)
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("Fabric packet exceeds its envelope")]
    Oversize,
    #[error("invalid Fabric packet header")]
    Header,
    #[error("no supported Fabric packet version for this binding")]
    Version,
    #[error("binding cannot preserve a required Fabric feature")]
    RequiredFeature,
    #[error("extension was not negotiated for this Fabric binding")]
    UnnegotiatedExtension,
    #[error("unexpected WAN SSRC")]
    RouteKey,
    #[error("unassigned Fabric payload type")]
    PayloadType,
    #[error("invalid payload length for the Fabric profile")]
    PayloadLength,
    #[error("invalid extension length, order, packing or padding")]
    ExtensionLayout,
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
}

impl<P: AsRef<[u8]>, E: AsRef<[u8]>> FabricPacket<P, E> {
    pub const fn header(&self) -> Header {
        self.header
    }

    pub const fn remaining_hops(&self) -> u8 {
        self.hops
    }

    /// Compare source facts independently of hop budget and extension packing.
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

    /// Set the canonical timestamp on a newly admitted endpoint media image,
    /// before it becomes visible to any branch or repair store.
    #[must_use]
    pub fn with_admitted_media_timestamp(mut self, timestamp: u32) -> Self {
        debug_assert!(self.header.profile != Profile::Data);
        self.header.timestamp = timestamp;
        self
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
pub mod packet;
pub mod wan;

#[cfg(test)]
mod tests;
