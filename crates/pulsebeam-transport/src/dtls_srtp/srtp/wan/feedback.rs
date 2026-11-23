use std::num::NonZeroU32;

use super::{FEEDBACK_SSRC, MAX_DATAGRAM_BYTES, RTCP_TRAILER_BYTES, SEQUENCE_HALF, SrtpError};

const HEADER_BYTES: usize = 16;
const TIMESTAMP_BYTES: usize = 4;
const MAX_PLAINTEXT_BYTES: usize = MAX_DATAGRAM_BYTES - RTCP_TRAILER_BYTES;
pub const MAX_METRICS: usize = (MAX_PLAINTEXT_BYTES - HEADER_BYTES - TIMESTAMP_BYTES) / 2;
const RECEIVED: u16 = 0x8000;

/// The RFC 8888 metric as transmitted. A missing metric is not a loss decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metric(u16);

impl Metric {
    pub const MISSING: Self = Self(0);

    /// `arrival` uses the RFC 8888 13-bit field, including its special values.
    pub fn received(ecn: u8, arrival: u16) -> Option<Self> {
        (ecn < 4 && arrival < 0x2000).then(|| Self(RECEIVED | (u16::from(ecn) << 13) | arrival))
    }

    pub fn observation(self) -> Option<(u8, u16)> {
        (self.0 & RECEIVED != 0).then_some((((self.0 >> 13) & 3) as u8, self.0 & 0x1fff))
    }
}

/// One reduced-size CCFB report for a single half-range route window.
/// Parsing does not authenticate the report or authorize its route.
#[derive(Debug)]
pub struct Report<'a> {
    pub ssrc: NonZeroU32,
    pub begin: u16,
    pub timestamp: u32,
    metrics: &'a [[u8; 2]],
}

impl<'a> Report<'a> {
    pub fn parse(plaintext: &'a [u8]) -> Result<Self, SrtpError> {
        header(plaintext, plaintext.len())?;
        let fields = plaintext
            .get(8..HEADER_BYTES)
            .ok_or(SrtpError::InvalidPacket)?;
        let &[a, b, c, d, e, f, g, h] = fields else {
            return Err(SrtpError::InvalidPacket);
        };
        let ssrc =
            NonZeroU32::new(u32::from_be_bytes([a, b, c, d])).ok_or(SrtpError::InvalidPacket)?;
        let begin = u16::from_be_bytes([e, f]);
        let count = usize::from(u16::from_be_bytes([g, h]));
        let size = size(ssrc, begin, count)?;
        if plaintext.len() != size {
            return Err(SrtpError::InvalidPacket);
        }
        let metric_end = HEADER_BYTES.saturating_add(count.saturating_mul(2));
        let (body, timestamp) = plaintext
            .split_last_chunk()
            .ok_or(SrtpError::InvalidPacket)?;
        let metrics = body
            .get(HEADER_BYTES..metric_end)
            .ok_or(SrtpError::InvalidPacket)?
            .as_chunks::<2>()
            .0;
        if metrics.iter().any(|bytes| {
            let raw = u16::from_be_bytes(*bytes);
            raw != 0 && raw & RECEIVED == 0
        }) || body
            .get(metric_end..)
            .ok_or(SrtpError::InvalidPacket)?
            .iter()
            .any(|&byte| byte != 0)
        {
            return Err(SrtpError::InvalidPacket);
        }
        let timestamp = u32::from_be_bytes(*timestamp);
        Ok(Self {
            ssrc,
            begin,
            timestamp,
            metrics,
        })
    }

    pub fn metrics(&self) -> impl ExactSizeIterator<Item = Metric> + '_ {
        self.metrics
            .iter()
            .map(|bytes| Metric(u16::from_be_bytes(*bytes)))
    }

    pub fn encode(
        ssrc: NonZeroU32,
        begin: u16,
        timestamp: u32,
        metrics: &[Metric],
    ) -> Result<Vec<u8>, SrtpError> {
        let size = size(ssrc, begin, metrics.len())?;
        let words =
            u16::try_from((size / 4).saturating_sub(1)).map_err(|_| SrtpError::OutputTooLarge)?;
        let count = u16::try_from(metrics.len()).map_err(|_| SrtpError::OutputTooLarge)?;
        let mut output = Vec::with_capacity(size);
        output.extend([0x8b, 205]);
        output.extend(words.to_be_bytes());
        output.extend(FEEDBACK_SSRC.to_be_bytes());
        output.extend(ssrc.get().to_be_bytes());
        output.extend(begin.to_be_bytes());
        output.extend(count.to_be_bytes());
        for metric in metrics {
            output.extend(metric.0.to_be_bytes());
        }
        if !metrics.len().is_multiple_of(2) {
            output.extend([0, 0]);
        }
        output.extend(timestamp.to_be_bytes());
        Ok(output)
    }
}

fn size(ssrc: NonZeroU32, begin: u16, count: usize) -> Result<usize, SrtpError> {
    if count > MAX_METRICS {
        return Err(SrtpError::OutputTooLarge);
    }
    if count == 0
        || ssrc.get() == FEEDBACK_SSRC
        || usize::from(begin & !SEQUENCE_HALF).saturating_add(count) > usize::from(SEQUENCE_HALF)
    {
        return Err(SrtpError::InvalidPacket);
    }
    Ok(HEADER_BYTES
        .saturating_add(count.next_multiple_of(2).saturating_mul(2))
        .saturating_add(TIMESTAMP_BYTES))
}

pub(super) fn header(packet: &[u8], plaintext_len: usize) -> Result<(), SrtpError> {
    let [0x8b, 205, hi, lo, a, b, c, d, ..] = packet else {
        return Err(SrtpError::InvalidPacket);
    };
    if plaintext_len > MAX_PLAINTEXT_BYTES {
        return Err(SrtpError::OutputTooLarge);
    }
    if plaintext_len < HEADER_BYTES + 4 + TIMESTAMP_BYTES
        || usize::from(u16::from_be_bytes([*hi, *lo]))
            .saturating_add(1)
            .saturating_mul(4)
            != plaintext_len
        || u32::from_be_bytes([*a, *b, *c, *d]) != FEEDBACK_SSRC
    {
        return Err(SrtpError::InvalidPacket);
    }
    Ok(())
}
