//! Canonical FabricPacket wire codec. See `crates/pulsebeam-core/docs/fabric-packet.md`.
//!
//! This codec knows binding compatibility, but not binding identity, transport,
//! routing or hop budget. Successful parsing is not source admission.

use super::{
    BorrowedPacket, Extension, ExtensionKind, FabricPacket, Form, Header, MAJOR, MAX_HOPS, Profile,
    REGISTRY_REVISION, WireError,
};

pub const BASE_BYTES: usize = 12;
pub const MAX_PACKET_BYTES: usize = 1280;

/// Each side declares its capabilities for the profile and the direction
/// being installed. A bit's position is the numeric extension ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer {
    pub major: u8,
    pub min_revision: u8,
    pub max_revision: u8,
    pub supported: u16,
    pub required: u16,
}

impl Offer {
    pub const fn current(profile: Profile, required: u16) -> Self {
        Self {
            major: MAJOR,
            min_revision: 0,
            max_revision: REGISTRY_REVISION,
            supported: registered(profile, REGISTRY_REVISION),
            required,
        }
    }
}

const fn registered(profile: Profile, revision: u8) -> u16 {
    let mut mask = 0;
    let mut id = 2_u8;
    while (id as u32) < u16::BITS {
        if let Ok(kind) = ExtensionKind::from_id(id)
            && kind.is_permitted_on(profile)
            && kind.minimum_revision() <= revision
        {
            mask |= kind.bit();
        }
        id = id.saturating_add(1);
    }
    mask
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoDdEmission {
    None,
    EveryPacket,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoDdReception {
    None,
    Accepted,
    Required,
}

/// Wire facts frozen for the lifetime of one authorized binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Agreement {
    profile: Profile,
    revision: u8,
    extensions: u16,
    requires_video_dd: bool,
}

impl Agreement {
    pub fn negotiate(profile: Profile, a: Offer, b: Offer) -> Result<Self, WireError> {
        let revision = a.max_revision.min(b.max_revision).min(REGISTRY_REVISION);
        if a.major != MAJOR || b.major != MAJOR || revision < a.min_revision.max(b.min_revision) {
            return Err(WireError::Version);
        }
        let extensions = a.supported & b.supported & registered(profile, revision);
        if (a.required | b.required) & !extensions != 0 {
            return Err(WireError::RequiredFeature);
        }
        Ok(Self {
            profile,
            revision,
            extensions,
            requires_video_dd: false,
        })
    }

    /// Install one video direction. Extension capability is necessary but does
    /// not promise that the publisher emits DD or that the reader requires it.
    pub fn negotiate_video(
        profile: Profile,
        publisher: Offer,
        emission: VideoDdEmission,
        receiver: Offer,
        reception: VideoDdReception,
    ) -> Result<Self, WireError> {
        if !profile.is_video() {
            return Err(WireError::ExtensionProfile);
        }
        let mut agreement = Self::negotiate(profile, publisher, receiver)?;
        let dd = ExtensionKind::DependencyDescriptor;
        match (emission, reception) {
            (VideoDdEmission::None, VideoDdReception::None | VideoDdReception::Accepted)
                if (publisher.required | receiver.required) & dd.bit() == 0 =>
            {
                agreement.extensions &= !dd.bit();
            }
            (
                VideoDdEmission::EveryPacket,
                VideoDdReception::Accepted | VideoDdReception::Required,
            ) if agreement.allows(dd) => {
                agreement.requires_video_dd = true;
            }
            _ => return Err(WireError::RequiredFeature),
        }
        Ok(agreement)
    }

    pub const fn profile(self) -> Profile {
        self.profile
    }

    pub const fn revision(self) -> u8 {
        self.revision
    }

    pub const fn extension_mask(self) -> u16 {
        self.extensions
    }

    pub fn decode(self, bytes: &[u8]) -> Result<BorrowedPacket<'_>, WireError> {
        if bytes.len() > MAX_PACKET_BYTES {
            return Err(WireError::Oversize);
        }
        let [pt, flags @ 0..=3, a, b, c, d, e, f, g, h, i, j, body @ ..] = bytes else {
            return Err(WireError::Header);
        };
        let profile = Profile::from_pt(*pt)?;
        if profile != self.profile {
            return Err(WireError::PayloadType);
        }
        let form = if flags & 2 != 0 { Form::Two } else { Form::One };
        let (optional, payload) = body
            .split_at_checked(usize::from(u16::from_be_bytes([*i, *j])))
            .ok_or(WireError::ExtensionLayout)?;
        profile.validate_payload(payload.len())?;
        let mut rest = optional;
        let mut last = 1;
        let mut needs_two = false;
        let mut has_video_dd = false;
        while !rest.is_empty() {
            let (id, value, next) = form.read_element(rest)?;
            if id <= last {
                return Err(WireError::ExtensionLayout);
            }
            let kind = ExtensionKind::from_id(id)?;
            if !self.allows(kind) {
                return Err(WireError::UnnegotiatedExtension);
            }
            kind.validate(profile, value)?;
            has_video_dd |= kind == ExtensionKind::DependencyDescriptor;
            needs_two |= value.len() > 16;
            last = id;
            rest = next;
        }
        if self.requires_video_dd && !has_video_dd {
            return Err(WireError::RequiredFeature);
        }
        if (form == Form::Two) != needs_two {
            return Err(WireError::ExtensionLayout);
        }
        Ok(FabricPacket {
            header: Header {
                profile,
                marker: flags & 1 != 0,
                sequence: u32::from_be_bytes([*a, *b, *c, *d]),
                timestamp: u32::from_be_bytes([*e, *f, *g, *h]),
            },
            hops: MAX_HOPS,
            form,
            optional,
            payload,
            source_protected_bytes: None,
            wan_received_bytes: None,
        })
    }

    /// Project the packet to this binding and serialize it. Unnegotiated
    /// advisory metadata can be omitted; dependency or timing facts cannot.
    pub fn write(
        self,
        packet: &FabricPacket<impl AsRef<[u8]>, impl AsRef<[u8]>>,
        output: &mut [u8],
    ) -> Result<usize, WireError> {
        self.write_extensions(
            packet.header(),
            packet.payload(),
            || packet.extensions(),
            output,
        )
    }

    /// Replace or omit the source DD, leaving all other extension negotiation intact.
    pub fn write_mapped_dd(
        self,
        packet: &FabricPacket<impl AsRef<[u8]>, impl AsRef<[u8]>>,
        descriptor: Option<&[u8]>,
        output: &mut [u8],
    ) -> Result<usize, WireError> {
        let kind = ExtensionKind::DependencyDescriptor;
        if let Some(value) = descriptor {
            kind.validate(packet.header().profile, value)?;
            if !self.allows(kind) || !packet.extensions().any(|ext| ext.kind == kind) {
                return Err(WireError::RequiredFeature);
            }
        }
        self.write_extensions(
            packet.header(),
            packet.payload(),
            || {
                packet.extensions().filter_map(|ext| {
                    if ext.kind == kind {
                        descriptor.map(|value| Extension { kind, value })
                    } else {
                        Some(ext)
                    }
                })
            },
            output,
        )
    }

    fn write_extensions<'a, I: Iterator<Item = Extension<'a>>>(
        self,
        header: Header,
        payload: &[u8],
        extensions: impl Fn() -> I,
        output: &mut [u8],
    ) -> Result<usize, WireError> {
        if header.profile != self.profile {
            return Err(WireError::PayloadType);
        }
        let mut needs_two = false;
        let mut count = 0_usize;
        let mut values = 0_usize;
        let mut has_video_dd = false;
        for ext in extensions() {
            if self.allows(ext.kind) {
                has_video_dd |= ext.kind == ExtensionKind::DependencyDescriptor;
                needs_two |= ext.value.len() > 16;
                count = count.saturating_add(1);
                values = values.saturating_add(ext.value.len());
            } else if !ext.kind.can_omit() {
                return Err(WireError::RequiredFeature);
            }
        }
        if self.requires_video_dd && !has_video_dd {
            return Err(WireError::RequiredFeature);
        }
        let form = if needs_two { Form::Two } else { Form::One };
        let extension_bytes = count
            .saturating_mul(form.header_bytes())
            .saturating_add(values);
        let len = BASE_BYTES
            .saturating_add(extension_bytes)
            .saturating_add(payload.len());
        if len > MAX_PACKET_BYTES {
            return Err(WireError::Oversize);
        }
        let [a, b, c, d] = header.sequence.to_be_bytes();
        let [e, f, g, h] = header.timestamp.to_be_bytes();
        let [i, j] = u16::try_from(extension_bytes)
            .map_err(|_| WireError::Oversize)?
            .to_be_bytes();
        let (base, body) = output
            .get_mut(..len)
            .and_then(|out| out.split_at_mut_checked(BASE_BYTES))
            .ok_or(WireError::OutputTooSmall)?;
        base.copy_from_slice(&[
            header.profile as u8,
            u8::from(header.marker) | (u8::from(needs_two) << 1),
            a,
            b,
            c,
            d,
            e,
            f,
            g,
            h,
            i,
            j,
        ]);
        let (mut metadata, payload_out) = body.split_at_mut(extension_bytes);
        for ext in extensions().filter(|ext| self.allows(ext.kind)) {
            let value_len = u8::try_from(ext.value.len()).map_err(|_| WireError::ExtensionValue)?;
            let (first, second) = form.element_header(ext.kind as u8, value_len);
            let (element, rest) = metadata.split_at_mut(form.element_bytes(ext.value.len()));
            match (second, element) {
                (None, [a, value @ ..]) => {
                    *a = first;
                    value.copy_from_slice(ext.value);
                }
                (Some(b), [a, b_out, value @ ..]) => {
                    [*a, *b_out] = [first, b];
                    value.copy_from_slice(ext.value);
                }
                _ => return Err(WireError::ExtensionLayout),
            }
            metadata = rest;
        }
        payload_out.copy_from_slice(payload);
        Ok(len)
    }

    const fn allows(self, kind: ExtensionKind) -> bool {
        self.extensions & kind.bit() != 0
    }
}

#[cfg(test)]
mod tests;
