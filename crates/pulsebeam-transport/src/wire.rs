//! Association-selected datagram framing. Fabric carries only an attempt ID outside its image.

use pulsebeam_feedback::{PacketId, ReceiverTime, SendStatus};

use crate::fabric_wan::TIMING_REVISION;

pub(crate) const UDP_BUDGET: usize = 1200;
// Fixed-path DTLS 1.3 without CID: reserve a conservative header, tag and
// encrypted-content-type allowance. The driver also enforces the sealed size.
pub(crate) const SECURITY_OVERHEAD: usize = 64;
pub(crate) const DATA_HEADER: usize = 10;
pub(crate) const FABRIC_HEADER: usize = 6;
pub(crate) const FABRIC_IMAGE: usize = 1152;
const FABRIC_DATA: [u8; 2] = [2, 0];
const FABRIC_CONTROL: [u8; 2] = [2, 2];
pub const CONTROL_CONTEXT: [u8; 4] = [0; 4];
pub const CONTROL_FRAME_BYTES: usize = 1024;
const _: () = assert!(FABRIC_IMAGE == 0x0480);
pub(crate) const PAYLOAD_BUDGET: usize = UDP_BUDGET - SECURITY_OVERHEAD - DATA_HEADER;
pub(crate) const MAX_STATUSES: usize = 64;
const FEEDBACK_HEADER: usize = 20;
const STATUS_BYTES: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidFrame;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Capability {
    Offer,
    Accept,
}

impl Capability {
    pub(crate) fn encode(self) -> [u8; 6] {
        let kind = match self {
            Self::Offer => 1,
            Self::Accept => 2,
        };
        [2, 1, kind, TIMING_REVISION, 0x04, 0x80]
    }
}

pub(crate) enum Frame<'a> {
    Capability(Capability),
    Data {
        context: Option<[u8; 4]>,
        attempt: u32,
        payload: &'a [u8],
    },
    Feedback {
        base: PacketId,
        generation: i64,
        statuses: &'a [u8],
    },
}

pub(crate) fn data_header(
    dst: &mut [u8],
    context: [u8; 4],
    attempt: u32,
) -> Result<(), InvalidFrame> {
    let header = dst.get_mut(..DATA_HEADER).ok_or(InvalidFrame)?;
    let mut bytes = header.iter_mut();
    for value in [1, 0]
        .into_iter()
        .chain(context)
        .chain(attempt.to_be_bytes())
    {
        *bytes.next().ok_or(InvalidFrame)? = value;
    }
    Ok(())
}

pub(crate) fn fabric_header(
    dst: &mut [u8],
    attempt: u32,
    control: bool,
) -> Result<(), InvalidFrame> {
    let prefix = if control { FABRIC_CONTROL } else { FABRIC_DATA };
    let header = dst.get_mut(..FABRIC_HEADER).ok_or(InvalidFrame)?;
    for (byte, value) in header
        .iter_mut()
        .zip(prefix.into_iter().chain(attempt.to_be_bytes()))
    {
        *byte = value;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn decode(wire: &[u8]) -> Result<Frame<'_>, InvalidFrame> {
    decode_for(wire, false)
}

pub(crate) fn decode_for(wire: &[u8], fabric: bool) -> Result<Frame<'_>, InvalidFrame> {
    let (prefix, body) = wire.split_at_checked(2).ok_or(InvalidFrame)?;
    match prefix {
        [1, 0] if !fabric => {
            let (context, body) = take::<4>(body)?;
            let (attempt, payload) = take::<4>(body)?;
            if payload.len() > PAYLOAD_BUDGET {
                return Err(InvalidFrame);
            }
            Ok(Frame::Data {
                context: Some(context),
                attempt: u32::from_be_bytes(attempt),
                payload,
            })
        }
        prefix if fabric && (prefix == FABRIC_DATA || prefix == FABRIC_CONTROL) => {
            let (attempt, payload) = take::<4>(body)?;
            let control = prefix == FABRIC_CONTROL;
            let limit = if control {
                CONTROL_FRAME_BYTES
            } else {
                FABRIC_IMAGE
            };
            if payload.len() > limit || (!control && payload.is_empty()) {
                return Err(InvalidFrame);
            }
            Ok(Frame::Data {
                context: control.then_some(CONTROL_CONTEXT),
                attempt: u32::from_be_bytes(attempt),
                payload,
            })
        }
        [2, 1] if fabric => match body {
            [1, TIMING_REVISION, 0x04, 0x80] => Ok(Frame::Capability(Capability::Offer)),
            [2, TIMING_REVISION, 0x04, 0x80] => Ok(Frame::Capability(Capability::Accept)),
            _ => Err(InvalidFrame),
        },
        [1, 1] => {
            let (base, body) = take::<8>(body)?;
            let (generation, body) = take::<8>(body)?;
            let (count, statuses) = take::<2>(body)?;
            let count = usize::from(u16::from_be_bytes(count));
            if count == 0
                || count > MAX_STATUSES
                || statuses.len() != count.saturating_mul(STATUS_BYTES)
            {
                return Err(InvalidFrame);
            }
            let generation = i64::from_be_bytes(generation);
            if generation < 0 {
                return Err(InvalidFrame);
            }
            Ok(Frame::Feedback {
                base: PacketId(u64::from_be_bytes(base)),
                generation,
                statuses,
            })
        }
        _ => Err(InvalidFrame),
    }
}

pub(crate) fn feedback_header(
    dst: &mut [u8],
    base: PacketId,
    generation: i64,
    count: usize,
) -> Result<usize, InvalidFrame> {
    if count == 0 || count > MAX_STATUSES || generation < 0 {
        return Err(InvalidFrame);
    }
    let count = u16::try_from(count).map_err(|_| InvalidFrame)?;
    let header = dst.get_mut(..FEEDBACK_HEADER).ok_or(InvalidFrame)?;
    let values = [1, 1]
        .into_iter()
        .chain(base.0.to_be_bytes())
        .chain(generation.to_be_bytes())
        .chain(count.to_be_bytes());
    for (byte, value) in header.iter_mut().zip(values) {
        *byte = value;
    }
    Ok(FEEDBACK_HEADER)
}

pub(crate) fn encode_status(
    dst: &mut [u8],
    arrival_micros: Option<i64>,
) -> Result<usize, InvalidFrame> {
    let dst = dst.get_mut(..STATUS_BYTES).ok_or(InvalidFrame)?;
    let (flag, bytes) = dst.split_first_mut().ok_or(InvalidFrame)?;
    *flag = u8::from(arrival_micros.is_some());
    bytes.copy_from_slice(&arrival_micros.unwrap_or(0).to_be_bytes());
    Ok(STATUS_BYTES)
}

pub(crate) fn decode_statuses(wire: &[u8], out: &mut Vec<SendStatus>) -> Result<(), InvalidFrame> {
    out.clear();
    if wire.is_empty()
        || !wire.len().is_multiple_of(STATUS_BYTES)
        || wire.len() / STATUS_BYTES > MAX_STATUSES
    {
        return Err(InvalidFrame);
    }
    for chunk in wire.as_chunks::<STATUS_BYTES>().0 {
        let (flag, bytes) = chunk.split_first().ok_or(InvalidFrame)?;
        let micros = i64::from_be_bytes(bytes.try_into().map_err(|_| InvalidFrame)?);
        let arrival = match (*flag, micros) {
            (0, 0) => None,
            (1, value) if value >= 0 => Some(ReceiverTime::Micros(value)),
            _ => return Err(InvalidFrame),
        };
        out.push(SendStatus {
            received: arrival.is_some(),
            arrival,
            ecn: None,
            hold: None,
        });
    }
    Ok(())
}

fn take<const N: usize>(input: &[u8]) -> Result<([u8; N], &[u8]), InvalidFrame> {
    let (head, tail) = input.split_at_checked(N).ok_or(InvalidFrame)?;
    Ok((head.try_into().map_err(|_| InvalidFrame)?, tail))
}
