//! Bounded application records independent of the QUIC endpoint.

use std::num::{NonZeroU16, NonZeroU64};

use pulsebeam_proto::state::{MAX_SUCCESSOR_RECEIPT_BYTES, validation::is_application_name};

use crate::frame::{Error, MAX_TOKEN};

const MAX_NAME: usize = 36;
const MAX_PROOF: usize = 256;
const MAX_CAPABILITIES: usize = 4096;
const DATAGRAM_HEADER: usize = 3;

// Do not derive Debug for types that borrow credentials.
pub struct Admit<'a> {
    pub attempt: NonZeroU64,
    pub room: Option<&'a str>,
    pub owner: &'a str,
    pub token: &'a str,
    pub successor: Option<&'a [u8]>,
}

impl<'a> Admit<'a> {
    pub fn decode(mut body: &'a [u8]) -> Result<Self, Error> {
        let attempt = revision(&mut body)?;
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
    let participant = name(field(&mut body, MAX_NAME)?)?;
    let room = field(&mut body, MAX_NAME)?;
    let room = (!room.is_empty()).then(|| name(room)).transpose()?;
    let capabilities = field(&mut body, MAX_CAPABILITIES)?;
    if !body.is_empty() {
        return Err(Error::Body);
    }
    Ok(Ok(Accepted {
        connection,
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
    Data = 1,
    Rtp = 2,
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
            1 => DatagramKind::Data,
            2 => DatagramKind::Rtp,
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
    fn bootstrap_is_exact_and_rejects_trailing_and_oversize_fields() {
        let original = Admit {
            attempt: NonZeroU64::MIN,
            room: Some("stage"),
            owner: "proof",
            token: "a.b.c",
            successor: Some(&[0xff]),
        };
        let mut wire = original.encode().unwrap();
        assert_eq!(
            wire,
            b"\0\0\0\0\0\0\0\x01\0\x05stage\0\x05proof\0\x05a.b.c\0\x01\xff"
        );
        let decoded = Admit::decode(&wire).unwrap();
        assert_eq!(decoded.successor, original.successor);
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
    fn datagram_keeps_binding_and_rejects_zero_or_insufficient_budget() {
        let datagram = Datagram {
            kind: DatagramKind::Rtp,
            binding: NonZeroU16::new(0x104).unwrap(),
            payload: &[0x80, 0x60],
        };
        assert_eq!(datagram.encode(4), Err(Error::Length));
        assert_eq!(datagram.encode(5).unwrap(), [2, 1, 4, 0x80, 0x60]);
        assert_eq!(Datagram::decode(&[2, 1, 4, 0x80, 0x60], 5), Ok(datagram));
        assert_eq!(
            Datagram::decode(&[2, 0, 0, 0x80, 0x60], 5),
            Err(Error::Body)
        );
    }
}
