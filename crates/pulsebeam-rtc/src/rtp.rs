#![allow(
    clippy::disallowed_types,
    reason = "RTP values own immutable Bytes payloads and negotiated URI strings"
)]

use std::{sync::Arc, time::Instant};

use bytes::Bytes;

/// Negotiated RFC 8285 wire policy for one RTP leg.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtpExtensionForm {
    OneByte,
    TwoByte,
    Mixed,
}

/// An extension value addressed by its negotiated semantic URI, not a source wire ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpExtension {
    pub uri: Arc<str>,
    pub value: Bytes,
}

/// A direct primary RTP write. The caller owns sequence and timestamp translation.
///
/// Gaps, wraps and late holes are valid. An accepted original index must never be
/// reused under the same sender/key, including after pending packets are discarded.
/// Rejected writes do not consume an index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpWrite {
    pub sequence: u64,
    pub timestamp: u32,
    pub marker: bool,
    pub payload: Bytes,
    pub extensions: Vec<RtpExtension>,
}

/// Admission policy for an explicitly bound sender.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundedRtp {
    pub max_wire_bytes: usize,
    /// Optional protected primary ceiling, separate from the physical path
    /// budget that also applies to repair. Includes any transport framing.
    pub max_primary_wire_bytes: Option<usize>,
    pub clock: Option<RtpClockAnchor>,
    /// Opaque local authority for this write and any derived retransmission.
    /// Scoped packets always require adapter handoff, including with `poll`.
    pub scope: Option<MediaScope>,
    /// On successful admission, cancel older, unprepared writes for this
    /// sender. Does not cancel an already handed-off packet or reset SRTP.
    pub replace_queued: bool,
}

/// Adapter-owned incarnation identity, never serialized into RTP or RTCP.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaScope {
    owner: std::num::NonZeroU128,
    provenance: Option<std::num::NonZeroU64>,
}

impl MediaScope {
    pub const fn new(owner: std::num::NonZeroU128) -> Self {
        Self {
            owner,
            provenance: None,
        }
    }

    pub const fn owner(self) -> std::num::NonZeroU128 {
        self.owner
    }

    pub const fn with_provenance(mut self, provenance: Option<std::num::NonZeroU64>) -> Self {
        self.provenance = provenance;
        self
    }

    pub const fn provenance(self) -> Option<std::num::NonZeroU64> {
        self.provenance
    }

    pub(crate) fn includes(self, candidate: Self) -> bool {
        self.owner == candidate.owner
            && self
                .provenance
                .is_none_or(|tag| candidate.provenance == Some(tag))
    }
}

/// Output RTP clock correlation independent of an individual media timestamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpClockAnchor {
    /// Extended output timestamp at the start of the output epoch.
    pub timestamp: u64,
    /// Fixed offset from canonical RTP to this output epoch.
    pub canonical_offset: u32,
}

/// Caller projection of one monotonic instant into continuous global time
/// and its representable UTC label. The UTC label already includes the era
/// conversion; only its wire fields are needed here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SenderReportTime {
    pub opus_rtp: u32,
    pub video_rtp: u32,
    pub ntp_seconds: u32,
    pub ntp_fraction: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpHeader {
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload_type: u8,
    pub marker: bool,
}

pub(crate) type RtpObserver<'a> = dyn for<'packet> FnMut(AuthenticatedRtp<'packet>) + 'a;

/// Primary format selected by the receive binding, before RTX decapsulation.
/// `profile` is absent for codecs without a structured RTC profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpReceiveCodec {
    pub payload_type: u8,
    pub name: Box<str>,
    pub clock_rate: u32,
    pub channels: Option<u16>,
    pub profile: Option<crate::CodecProfile>,
}

impl RtpReceiveCodec {
    pub(crate) fn discovery_info(&self) -> crate::RtpPayloadInfo {
        crate::RtpPayloadInfo {
            payload_type: self.payload_type,
            name: self.name.clone(),
            clock_rate: self.clock_rate,
        }
    }
}

/// One negotiated receive-eligible endpoint extension. The URI is the wire
/// mapping's URI, including the RFC 6904 wrapper when the value is encrypted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpReceiveExtension {
    pub id: u8,
    pub uri: Box<str>,
    pub attributes: Option<Box<str>>,
}

impl RtpReceiveExtension {
    pub(crate) fn without_attributes(id: u8, uri: impl Into<Box<str>>) -> Self {
        Self {
            id,
            uri: uri.into(),
            attributes: None,
        }
    }

    /// RTC does not make encrypted header extensions available as clear values.
    pub fn is_encrypted(&self) -> bool {
        self.uri.as_ref() == "urn:ietf:params:rtp-hdrext:encrypt"
    }
}

/// Negotiated receive metadata for one installed primary encoding. This
/// describes RTC state, not permission to forward the publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpReceiveBinding<'a> {
    pub mid: &'a str,
    pub rid: Option<&'a str>,
    pub primary_ssrc: u32,
    pub primary_codec: &'a RtpReceiveCodec,
    pub repair_payload_type: Option<u8>,
    pub declared_repair_ssrc: Option<u32>,
    pub receive_extensions: &'a [RtpReceiveExtension],
    pub allow_mixed_extensions: bool,
}

/// Borrowed RTP wire facts after SRTP authentication and RTC media/RTX resolution.
/// This does not grant publication or forwarding authority. The wire header is
/// not normalized, and the payload excludes validated RTP padding but includes
/// any RTX OSN. An empty payload may represent a padding-only packet.
pub struct AuthenticatedRtp<'a> {
    pub arrival: Instant,
    pub mid: &'a str,
    pub rid: Option<&'a str>,
    pub primary_ssrc: u32,
    pub primary_codec: &'a RtpReceiveCodec,
    /// Negotiated repair payload type associated with this primary codec.
    pub repair_payload_type: Option<u8>,
    /// Signaled FID (or typed) repair SSRC. May be absent when the endpoint
    /// resolves repair dynamically by repaired RID.
    pub declared_repair_ssrc: Option<u32>,
    pub is_repair: bool,
    pub receive_extensions: &'a [RtpReceiveExtension],
    /// False means that the sender must keep a fixed extension form, not
    /// necessarily that it must use the one-byte form.
    pub allow_mixed_extensions: bool,
    pub(crate) wire: &'a crate::packet::RtpPacket<'a>,
    pub(crate) protection: Option<RtpProtection>,
}

#[derive(Clone, Copy)]
pub(crate) struct RtpProtection {
    pub bytes: usize,
    pub index: Option<u64>,
    pub direct_full_tag: bool,
}

impl<'a> AuthenticatedRtp<'a> {
    pub const fn binding(&self) -> RtpReceiveBinding<'a> {
        RtpReceiveBinding {
            mid: self.mid,
            rid: self.rid,
            primary_ssrc: self.primary_ssrc,
            primary_codec: self.primary_codec,
            repair_payload_type: self.repair_payload_type,
            declared_repair_ssrc: self.declared_repair_ssrc,
            receive_extensions: self.receive_extensions,
            allow_mixed_extensions: self.allow_mixed_extensions,
        }
    }

    pub const fn primary_payload_type(&self) -> u8 {
        self.primary_codec.payload_type
    }

    pub fn wire_header(&self) -> RtpHeader {
        RtpHeader {
            sequence: self.wire.sequence(),
            timestamp: self.wire.timestamp(),
            ssrc: self.wire.ssrc(),
            payload_type: self.wire.payload_type(),
            marker: self.wire.marker(),
        }
    }

    /// SRTP packet index of the primary, never the independent RTX index.
    pub fn authenticated_primary_index(&self) -> Option<u64> {
        if self.is_repair {
            return None;
        }
        self.protection.and_then(|protection| protection.index)
    }

    /// Protected size of this exact RTP datagram, before SRTP unprotection.
    pub fn protected_bytes(&self) -> Option<usize> {
        self.protection.map(|protection| protection.bytes)
    }

    /// Whether this packet arrived on the current direct UDP path with a
    /// 16-byte SRTP authentication tag, independent of RTP header extensions.
    pub fn has_direct_full_tag_protection(&self) -> bool {
        self.protection
            .is_some_and(|protection| protection.direct_full_tag)
    }

    pub fn wire_payload(&self) -> &'a [u8] {
        self.wire.payload()
    }

    pub fn first_csrc(&self) -> Option<u32> {
        self.wire.first_csrc()
    }

    pub fn padding_len(&self) -> usize {
        self.wire.padding_len()
    }

    pub fn extension_profile(&self) -> Option<u16> {
        self.wire.extension_profile()
    }

    pub fn wire_extensions(&self) -> impl Iterator<Item = (u8, &'a [u8])> {
        self.wire
            .extensions()
            .into_iter()
            .flatten()
            .map(|ext| (ext.id(), ext.value()))
    }
}

/// Authenticated primary RTP, including ordinary RTX decapsulation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpPacket {
    pub arrival: Instant,
    pub header: RtpHeader,
    pub payload: Bytes,
    pub extensions: Vec<RtpExtension>,
}

/// Wire Sender Report facts. Synchronization and media normalization are caller policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SenderReport {
    pub arrival: Instant,
    pub ssrc: u32,
    pub ntp_seconds: u32,
    pub ntp_fraction: u32,
    pub rtp_timestamp: u32,
}
