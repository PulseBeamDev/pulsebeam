#![allow(
    clippy::disallowed_types,
    reason = "RTP values own immutable Bytes payloads and negotiated URI strings"
)]

use std::{sync::Arc, time::Instant};

use bytes::Bytes;

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
/// Rejected writes do not consume an index. `wallclock` relates `timestamp` to the
/// connection's fixed protocol clock anchor for RTCP Sender Reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpWrite {
    pub sequence: u64,
    pub timestamp: u32,
    pub wallclock: Instant,
    pub marker: bool,
    pub payload: Bytes,
    pub extensions: Vec<RtpExtension>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpHeader {
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload_type: u8,
    pub marker: bool,
}

/// Authenticated primary RTP, including ordinary RTX decapsulation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpPacket {
    pub arrival: Instant,
    pub header: RtpHeader,
    pub payload: Bytes,
    pub extensions: Vec<RtpExtension>,
}

impl RtpPacket {
    pub(crate) fn queued_bytes(&self) -> usize {
        self.payload.len().saturating_add(12).saturating_add(
            self.extensions
                .iter()
                .map(|value| value.value.len().saturating_add(4))
                .sum::<usize>(),
        )
    }
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
