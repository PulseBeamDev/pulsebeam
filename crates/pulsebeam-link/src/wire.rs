//! Private authenticated Link framing, independent of application payloads.

use pulsebeam_feedback::{PacketId, ReceiverTime, SendStatus};

pub(crate) const UDP_BUDGET: usize = 1200;
// Fixed-path DTLS 1.3 without CID: reserve a conservative header, tag and
// encrypted-content-type allowance. The driver also enforces the sealed size.
pub(crate) const SECURITY_OVERHEAD: usize = 64;
pub(crate) const DATA_HEADER: usize = 10;
pub(crate) const PAYLOAD_BUDGET: usize = UDP_BUDGET - SECURITY_OVERHEAD - DATA_HEADER;
pub(crate) const MAX_STATUSES: usize = 64;
const FEEDBACK_HEADER: usize = 20;
const STATUS_BYTES: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidFrame;

pub(crate) enum Frame<'a> {
    Data {
        route: u32,
        attempt: u32,
        payload: &'a [u8],
    },
    Feedback {
        base: PacketId,
        generation: i64,
        statuses: &'a [u8],
    },
}

pub(crate) fn data_header(dst: &mut [u8], route: u32, attempt: u32) -> Result<(), InvalidFrame> {
    let header = dst.get_mut(..DATA_HEADER).ok_or(InvalidFrame)?;
    let mut bytes = header.iter_mut();
    for value in [1, 0]
        .into_iter()
        .chain(route.to_be_bytes())
        .chain(attempt.to_be_bytes())
    {
        *bytes.next().ok_or(InvalidFrame)? = value;
    }
    Ok(())
}

pub(crate) fn decode(wire: &[u8]) -> Result<Frame<'_>, InvalidFrame> {
    let (prefix, body) = wire.split_at_checked(2).ok_or(InvalidFrame)?;
    match prefix {
        [1, 0] => {
            let (route, body) = take::<4>(body)?;
            let (attempt, payload) = take::<4>(body)?;
            if payload.len() > PAYLOAD_BUDGET {
                return Err(InvalidFrame);
            }
            Ok(Frame::Data {
                route: u32::from_be_bytes(route),
                attempt: u32::from_be_bytes(attempt),
                payload,
            })
        }
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
    if wire.is_empty() || wire.len() % STATUS_BYTES != 0 || wire.len() / STATUS_BYTES > MAX_STATUSES
    {
        return Err(InvalidFrame);
    }
    for chunk in wire.chunks_exact(STATUS_BYTES) {
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
