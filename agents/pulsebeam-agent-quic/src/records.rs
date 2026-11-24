//! Bounded application records independent of the QUIC endpoint.

use std::num::{NonZeroU16, NonZeroU32, NonZeroU64};

use pulsebeam_proto::state::{MAX_SUCCESSOR_RECEIPT_BYTES, validation::is_application_name};

use crate::{
    data_bindings::FabricOffer,
    frame::{Error, MAX_TOKEN},
};

const MAX_NAME: usize = 36;
const MAX_PROOF: usize = 256;
const MAX_CAPABILITIES: usize = 4096;
pub(crate) const DATAGRAM_HEADER: usize = 3;

// Do not derive Debug for types that borrow credentials.
pub struct Admit<'a> {
    pub attempt: NonZeroU64,
    pub fabric: FabricOffer,
    pub room: Option<&'a str>,
    pub owner: &'a str,
    pub token: &'a str,
    pub successor: Option<&'a [u8]>,
}

impl<'a> Admit<'a> {
    pub fn decode(mut body: &'a [u8]) -> Result<Self, Error> {
        let attempt = revision(&mut body)?;
        let fabric = FabricOffer::decode(take(&mut body)?);
        let room = field(&mut body, MAX_NAME)?;
        let room = (!room.is_empty()).then(|| name(room)).transpose()?;
        let owner = visible(field(&mut body, MAX_PROOF)?)?;
        let token = visible(field(&mut body, MAX_TOKEN)?)?;
        let successor = field(&mut body, MAX_SUCCESSOR_RECEIPT_BYTES)?;
        let successor = (!successor.is_empty()).then_some(successor);
        if !body.is_empty() || successor.is_some() && room.is_none() {
            return Err(Error::Body);
        }
        Ok(Self {
            attempt,
            fabric,
            room,
            owner,
            token,
            successor,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        if let Some(room) = self.room {
            name(room.as_bytes())?;
        }
        visible(self.owner.as_bytes())?;
        visible(self.token.as_bytes())?;
        if self
            .successor
            .is_some_and(|receipt| receipt.is_empty() || self.room.is_none())
        {
            return Err(Error::Body);
        }
        let mut out = self.attempt.get().to_be_bytes().to_vec();
        out.extend_from_slice(&self.fabric.encode());
        append(&mut out, self.room.unwrap_or_default().as_bytes(), MAX_NAME)?;
        append(&mut out, self.owner.as_bytes(), MAX_PROOF)?;
        append(&mut out, self.token.as_bytes(), MAX_TOKEN)?;
        append(
            &mut out,
            self.successor.unwrap_or_default(),
            MAX_SUCCESSOR_RECEIPT_BYTES,
        )?;
        Ok(out)
    }
}

pub struct Renew<'a> {
    pub revision: NonZeroU64,
    pub token: &'a str,
}

impl<'a> Renew<'a> {
    pub fn decode(mut body: &'a [u8]) -> Result<Self, Error> {
        let revision = revision(&mut body)?;
        let token = visible(field(&mut body, MAX_TOKEN)?)?;
        if !body.is_empty() {
            return Err(Error::Body);
        }
        Ok(Self { revision, token })
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        visible(self.token.as_bytes())?;
        let mut out = self.revision.get().to_be_bytes().to_vec();
        append(&mut out, self.token.as_bytes(), MAX_TOKEN)?;
        Ok(out)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Denial {
    Denied = 1,
    Placement = 2,
    AttemptUnavailable = 3,
    Capacity = 4,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Accepted<'a> {
    pub connection: [u8; 16],
    pub fabric: FabricOffer,
    pub participant: &'a str,
    pub room: Option<&'a str>,
    pub capabilities: &'a [u8],
}

pub fn decode_admit_result(mut body: &[u8]) -> Result<Result<Accepted<'_>, Denial>, Error> {
    match body {
        [0, rest @ ..] => body = rest,
        [1] => return Ok(Err(Denial::Denied)),
        [2] => return Ok(Err(Denial::Placement)),
        [3] => return Ok(Err(Denial::AttemptUnavailable)),
        [4] => return Ok(Err(Denial::Capacity)),
        _ => return Err(Error::Body),
    }
    let connection = take(&mut body)?;
    let fabric = FabricOffer::decode(take(&mut body)?);
    let participant = name(field(&mut body, MAX_NAME)?)?;
    let room = field(&mut body, MAX_NAME)?;
    let room = (!room.is_empty()).then(|| name(room)).transpose()?;
    let capabilities = field(&mut body, MAX_CAPABILITIES)?;
    if !body.is_empty() {
        return Err(Error::Body);
    }
    Ok(Ok(Accepted {
        connection,
        fabric,
        participant,
        room,
        capabilities,
    }))
}

pub fn encode_admit_result(result: Result<Accepted<'_>, Denial>) -> Result<Vec<u8>, Error> {
    let result = match result {
        Ok(result) => result,
        Err(denial) => return Ok(vec![denial as u8]),
    };
    if let Some(room) = result.room {
        name(room.as_bytes())?;
    }
    name(result.participant.as_bytes())?;
    let mut out = vec![0];
    out.extend_from_slice(&result.connection);
    out.extend_from_slice(&result.fabric.encode());
    append(&mut out, result.participant.as_bytes(), MAX_NAME)?;
    append(
        &mut out,
        result.room.unwrap_or_default().as_bytes(),
        MAX_NAME,
    )?;
    append(&mut out, result.capabilities, MAX_CAPABILITIES)?;
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
pub struct RenewResult {
    pub request: NonZeroU64,
    pub accepted: Option<Lease>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Lease {
    pub revision: NonZeroU64,
    pub expires_unix_seconds: NonZeroU64,
}

impl RenewResult {
    pub fn decode(mut body: &[u8]) -> Result<Self, Error> {
        let request = revision(&mut body)?;
        let [status] = take(&mut body)?;
        let rev = NonZeroU64::new(u64::from_be_bytes(take(&mut body)?));
        let expires = NonZeroU64::new(u64::from_be_bytes(take(&mut body)?));
        if !body.is_empty() {
            return Err(Error::Body);
        }
        let accepted = match (status, rev, expires) {
            (0, Some(revision), Some(expires_unix_seconds)) => Some(Lease {
                revision,
                expires_unix_seconds,
            }),
            (1, None, None) => None,
            _ => return Err(Error::Body),
        };
        Ok(Self { request, accepted })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.request.get().to_be_bytes().to_vec();
        let (status, revision, expiry) = self.accepted.as_ref().map_or((1, 0, 0), |lease| {
            (0, lease.revision.get(), lease.expires_unix_seconds.get())
        });
        out.push(status);
        out.extend_from_slice(&revision.to_be_bytes());
        out.extend_from_slice(&expiry.to_be_bytes());
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DatagramKind {
    Rtcp = 3,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Datagram<'a> {
    pub kind: DatagramKind,
    pub binding: NonZeroU16,
    pub payload: &'a [u8],
}

impl<'a> Datagram<'a> {
    /// Parse only the envelope. The installed binding owner validates and
    /// parses the payload after the association and lifetime checks.
    pub fn decode(mut bytes: &'a [u8], negotiated_limit: usize) -> Result<Self, Error> {
        if bytes.len() > negotiated_limit {
            return Err(Error::Length);
        }
        let [kind, high, low] = take(&mut bytes)?;
        let kind = match kind {
            3 => DatagramKind::Rtcp,
            _ => return Err(Error::Kind),
        };
        let binding = NonZeroU16::new(u16::from_be_bytes([high, low])).ok_or(Error::Body)?;
        if bytes.is_empty() {
            return Err(Error::Length);
        }
        Ok(Self {
            kind,
            binding,
            payload: bytes,
        })
    }

    pub fn encode(&self, negotiated_limit: usize) -> Result<Vec<u8>, Error> {
        let len = self.payload.len().saturating_add(DATAGRAM_HEADER);
        if self.payload.is_empty() || len > negotiated_limit {
            return Err(Error::Length);
        }
        let mut out = Vec::with_capacity(len);
        out.push(self.kind as u8);
        out.extend_from_slice(&self.binding.get().to_be_bytes());
        out.extend_from_slice(self.payload);
        Ok(out)
    }
}

/// Negotiated independently from the FabricPacket registry at binding install.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketFraming(());

impl PacketFraming {
    pub const VERSION: u8 = 1;

    pub fn negotiate(peer_version: u8) -> Result<Self, Error> {
        (peer_version == Self::VERSION)
            .then_some(Self(()))
            .ok_or(Error::Body)
    }
}

/// An opaque range of a logical packet. Whole carries no transfer state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketFrame<'a> {
    Whole(&'a [u8]),
    Slice {
        transfer: NonZeroU32,
        total: u16,
        offset: u16,
        bytes: &'a [u8],
    },
}

impl<'a> PacketFrame<'a> {
    pub const WHOLE_HEADER_BYTES: usize = DATAGRAM_HEADER;
    pub const SLICE_HEADER_BYTES: usize = DATAGRAM_HEADER + 8;
    const WHOLE: u8 = 4;
    const SLICE: u8 = 5;

    /// Read only the routing prefix. The owner must resolve this binding,
    /// including generation and direction, before invoking the reassembler.
    pub fn binding(bytes: &[u8]) -> Option<NonZeroU16> {
        match bytes {
            [Self::WHOLE | Self::SLICE, a, b, ..] => NonZeroU16::new(u16::from_be_bytes([*a, *b])),
            _ => None,
        }
    }

    pub fn decode(_: PacketFraming, bytes: &'a [u8]) -> Result<Self, Error> {
        Self::binding(bytes).ok_or(Error::Body)?;
        let frame = match bytes {
            [Self::WHOLE, _, _, bytes @ ..] => Self::Whole(bytes),
            [Self::SLICE, _, _, a, b, c, d, e, f, g, h, bytes @ ..] => Self::Slice {
                transfer: NonZeroU32::new(u32::from_be_bytes([*a, *b, *c, *d]))
                    .ok_or(Error::Body)?,
                total: u16::from_be_bytes([*e, *f]),
                offset: u16::from_be_bytes([*g, *h]),
                bytes,
            },
            _ => return Err(Error::Length),
        };
        frame.validate()?;
        Ok(frame)
    }

    pub fn write(
        self,
        _: PacketFraming,
        binding: NonZeroU16,
        output: &mut [u8],
    ) -> Result<usize, Error> {
        self.validate()?;
        let [a, b] = binding.get().to_be_bytes();
        let (header, header_len, bytes) = match self {
            Self::Whole(bytes) => (
                [Self::WHOLE, a, b, 0, 0, 0, 0, 0, 0, 0, 0],
                Self::WHOLE_HEADER_BYTES,
                bytes,
            ),
            Self::Slice {
                transfer,
                total,
                offset,
                bytes,
            } => {
                let [c, d, e, f] = transfer.get().to_be_bytes();
                let [g, h] = total.to_be_bytes();
                let [i, j] = offset.to_be_bytes();
                (
                    [Self::SLICE, a, b, c, d, e, f, g, h, i, j],
                    Self::SLICE_HEADER_BYTES,
                    bytes,
                )
            }
        };
        let len = header_len.saturating_add(bytes.len());
        let (prefix, data) = output
            .get_mut(..len)
            .and_then(|out| out.split_at_mut_checked(header_len))
            .ok_or(Error::Length)?;
        prefix.copy_from_slice(header.get(..header_len).ok_or(Error::Length)?);
        data.copy_from_slice(bytes);
        Ok(len)
    }

    fn validate(self) -> Result<(), Error> {
        use pulsebeam_core::fabric::packet::MAX_PACKET_BYTES;
        match self {
            Self::Whole(bytes) if !bytes.is_empty() && bytes.len() <= MAX_PACKET_BYTES => Ok(()),
            Self::Slice {
                total,
                offset,
                bytes,
                ..
            } if usize::from(total) <= MAX_PACKET_BYTES
                && !bytes.is_empty()
                && bytes.len() < usize::from(total)
                && usize::from(offset).saturating_add(bytes.len()) <= usize::from(total) =>
            {
                Ok(())
            }
            _ => Err(Error::Length),
        }
    }
}

/// Standalone RTCP Picture Loss Indication (RFC 4585). The QUIC envelope
/// selects the media binding; the media SSRC must also match that binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PictureLossIndication {
    pub sender_ssrc: u32,
    pub media_ssrc: NonZeroU32,
}

impl PictureLossIndication {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let [0x81, 206, 0, 2, a, b, c, d, e, f, g, h] = bytes else {
            return Err(Error::Body);
        };
        Ok(Self {
            sender_ssrc: u32::from_be_bytes([*a, *b, *c, *d]),
            media_ssrc: NonZeroU32::new(u32::from_be_bytes([*e, *f, *g, *h])).ok_or(Error::Body)?,
        })
    }

    pub fn encode(self) -> [u8; 12] {
        let [a, b, c, d] = self.sender_ssrc.to_be_bytes();
        let [e, f, g, h] = self.media_ssrc.get().to_be_bytes();
        [0x81, 206, 0, 2, a, b, c, d, e, f, g, h]
    }
}

fn take<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N], Error> {
    let (value, rest) = bytes.split_first_chunk().ok_or(Error::Body)?;
    *bytes = rest;
    Ok(*value)
}

fn revision(bytes: &mut &[u8]) -> Result<NonZeroU64, Error> {
    NonZeroU64::new(u64::from_be_bytes(take(bytes)?)).ok_or(Error::Body)
}

fn field<'a>(bytes: &mut &'a [u8], max: usize) -> Result<&'a [u8], Error> {
    let len = usize::from(u16::from_be_bytes(take(bytes)?));
    if len > max {
        return Err(Error::Length);
    }
    let (value, rest) = bytes.split_at_checked(len).ok_or(Error::Body)?;
    *bytes = rest;
    Ok(value)
}

fn append(out: &mut Vec<u8>, value: &[u8], max: usize) -> Result<(), Error> {
    if value.len() > max {
        return Err(Error::Length);
    }
    let len = u16::try_from(value.len()).map_err(|_| Error::Length)?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value);
    Ok(())
}

fn name(bytes: &[u8]) -> Result<&str, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Body)?;
    is_application_name(text).then_some(text).ok_or(Error::Body)
}

fn visible(bytes: &[u8]) -> Result<&str, Error> {
    if bytes.is_empty() || !bytes.iter().all(|byte| (0x21..=0x7e).contains(byte)) {
        return Err(Error::Body);
    }
    std::str::from_utf8(bytes).map_err(|_| Error::Body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quic_pli_requires_one_exact_rtcp_packet_and_media_ssrc() {
        let pli = PictureLossIndication {
            sender_ssrc: 17,
            media_ssrc: NonZeroU32::MIN,
        };
        let packet = pli.encode();
        assert_eq!(packet, [0x81, 206, 0, 2, 0, 0, 0, 17, 0, 0, 0, 1]);
        assert_eq!(PictureLossIndication::decode(&packet), Ok(pli));
        assert!(PictureLossIndication::decode(&[0x81, 206, 0, 2]).is_err());
        assert!(
            PictureLossIndication::decode(&[0x81, 206, 0, 2, 0, 0, 0, 17, 0, 0, 0, 0]).is_err()
        );
    }

    #[test]
    fn bootstrap_is_exact_and_rejects_trailing_and_oversize_fields() {
        let original = Admit {
            fabric: FabricOffer::CURRENT,
            attempt: NonZeroU64::MIN,
            room: Some("stage"),
            owner: "proof",
            token: "a.b.c",
            successor: Some(&[0xff]),
        };
        let mut wire = original.encode().unwrap();
        assert_eq!(
            wire,
            b"\0\0\0\0\0\0\0\x01\x01\x01\0\x01\x03\xfc\0\x05stage\0\x05proof\0\x05a.b.c\0\x01\xff"
        );
        let decoded = Admit::decode(&wire).unwrap();
        assert_eq!(decoded.successor, original.successor);
        assert_eq!(decoded.fabric, original.fabric);
        wire.push(0);
        assert_eq!(Admit::decode(&wire).err(), Some(Error::Body));
        let token = "x".repeat(MAX_TOKEN.saturating_add(1));
        assert_eq!(
            Admit {
                token: &token,
                ..original
            }
            .encode(),
            Err(Error::Length)
        );
    }

    #[test]
    fn maximum_fabric_data_record_exceeds_the_noq_floor_with_binding() {
        use pulsebeam_core::fabric::{
            Extension, ExtensionKind, FabricPacket, Header, Profile,
            packet::{Agreement, MAX_PACKET_BYTES, Offer},
        };
        use pulsebeam_proto::data_record::{self, Capture, Clock, Record};

        let clock = Clock {
            id: "camera",
            epoch: 7,
        };
        let capture = Capture {
            clock,
            nanos: -2,
            uncertainty_ns: Some(0x1234_5678),
        };
        let descriptor = [0xdb; 96];
        let act = [0x24; 16];
        let fragment = [0x7b; 1024];
        let record = Record::new(
            data_record::Header {
                unit_seq: 0x89ab_cdef_0123_4567,
                index: 63,
                count: 64,
                unit_len: 65_536,
                capture: Some(capture),
            },
            &fragment,
            Some(clock),
        )
        .unwrap();
        let mut payload = [0; data_record::MAX_RECORD_BYTES];
        assert_eq!(record.write(&mut payload).unwrap(), 1058);
        let packet = FabricPacket::new(
            Header {
                profile: Profile::Data,
                marker: true,
                sequence: 0x1122_3344,
                timestamp: 0x5566_7788,
            },
            &[
                Extension {
                    kind: ExtensionKind::DependencyDescriptor,
                    value: &descriptor,
                },
                Extension {
                    kind: ExtensionKind::AbsoluteCaptureTime,
                    value: &act,
                },
            ],
            &payload,
        )
        .unwrap();
        let offer = Offer::current(Profile::Data, 0x000c);
        let agreement = Agreement::negotiate(Profile::Data, offer, offer).unwrap();
        let mut output = [0; MAX_PACKET_BYTES];
        let len = agreement.write(&packet, &mut output).unwrap();
        assert_eq!(len, 1186);
        let bytes = output.get(..len).unwrap();
        let mut expected = vec![
            99, 3, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0, 116, 2, 96,
        ];
        expected.extend([0xdb; 96]);
        expected.extend([3, 16]);
        expected.extend([0x24; 16]);
        expected.extend([
            0x50, 0x44, 1, 3, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0, 63, 0, 64, 4, 0,
            0, 1, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe, 0x12, 0x34, 0x56, 0x78,
        ]);
        expected.extend([0x7b; 1024]);
        assert_eq!(bytes, expected);
        let decoded = agreement.decode(bytes).unwrap();
        assert_eq!(
            Record::decode_without_descriptor(decoded.payload(), Some(clock)).unwrap(),
            record
        );
        assert!(decoded.same_content(&packet.arrival_view(packet.remaining_hops()).unwrap()));
    }

    #[test]
    fn datagram_keeps_binding_and_rejects_zero_or_insufficient_budget() {
        let datagram = Datagram {
            kind: DatagramKind::Rtcp,
            binding: NonZeroU16::new(0x104).unwrap(),
            payload: &[0x80, 0x60],
        };
        assert_eq!(datagram.encode(4), Err(Error::Length));
        assert_eq!(datagram.encode(5).unwrap(), [3, 1, 4, 0x80, 0x60]);
        assert_eq!(Datagram::decode(&[3, 1, 4, 0x80, 0x60], 5), Ok(datagram));
        for retired in [1, 2] {
            assert_eq!(
                Datagram::decode(&[retired, 1, 4, 0x80, 0x60], 5),
                Err(Error::Kind)
            );
        }
        assert_eq!(
            Datagram::decode(&[3, 0, 0, 0x80, 0x60], 5),
            Err(Error::Body)
        );
    }
}
