//! Socket-free stages for producing logical RTP packets from encoded units.

use std::num::NonZeroUsize;

use pulsebeam_core::{
    fabric::Extension,
    h264::{MAX_ACCESS_UNIT_SIZE, Packetization, PacketizedChunk, Packetizer},
};

pub mod descriptor;
pub mod jitter;
pub mod units;

/// Source packet coordinates. The installed publication supplies the SSRC,
/// payload type and codec profile.
pub struct SendPacket<'a> {
    /// Extended RTP sequence; its low 16 bits are the RTP sequence number.
    pub sequence: u32,
    pub timestamp: u32,
    pub marker: bool,
    /// Semantic extensions, not SDP numeric IDs. Unnegotiated advisory
    /// metadata may be omitted; unnegotiated dependency/timing facts fail.
    pub extensions: &'a [Extension<'a>],
    pub payload: &'a [u8],
}

/// Logical RTP packet budget, independent of any carrier framing or path MTU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketBudget(NonZeroUsize);

impl PacketBudget {
    /// Reserve the 12-byte base header plus any CSRCs, extension headers,
    /// elements and padding the caller will add. The remainder is payload.
    pub fn new(packet_bytes: usize, extra_header_bytes: usize) -> Option<Self> {
        let payload = packet_bytes.checked_sub(12_usize.checked_add(extra_header_bytes)?)?;
        NonZeroUsize::new(payload).map(Self)
    }

    pub const fn payload_bytes(self) -> usize {
        self.0.get()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PacketizeError {
    Empty,
    TooLarge,
}

/// Packetized RTP, with independent frame boundaries (the Opus marker is not
/// a frame end). Keep this value if the raw sender asks you to retry.
#[derive(Debug, PartialEq, Eq)]
pub struct Packet<P> {
    pub sequence: u32,
    pub timestamp: u32,
    pub marker: bool,
    pub first: bool,
    pub last: bool,
    pub payload: P,
}

impl<P: AsRef<[u8]>> Packet<P> {
    pub fn as_send_packet<'a>(&'a self, extensions: &'a [Extension<'a>]) -> SendPacket<'a> {
        SendPacket {
            sequence: self.sequence,
            timestamp: self.timestamp,
            marker: self.marker,
            extensions,
            payload: self.payload.as_ref(),
        }
    }
}

/// An Opus packet is one RTP payload. It cannot be split to fit the budget.
#[derive(Clone, Copy)]
pub struct Opus(PacketBudget);

impl Opus {
    pub const fn new(budget: PacketBudget) -> Self {
        Self(budget)
    }

    pub fn packetize(
        self,
        sequence: u32,
        timestamp: u32,
        marker: bool,
        unit: &[u8],
    ) -> Result<Packet<&[u8]>, PacketizeError> {
        if unit.is_empty() {
            return Err(PacketizeError::Empty);
        }
        if unit.len() > self.0.payload_bytes() {
            return Err(PacketizeError::TooLarge);
        }
        Ok(Packet {
            sequence,
            timestamp,
            marker,
            first: true,
            last: true,
            payload: unit,
        })
    }
}

/// Annex-B H.264 packetization, using single NAL and FU-A payloads.
#[derive(Clone)]
pub struct H264(Packetizer);

impl H264 {
    pub fn new(budget: PacketBudget) -> Option<Self> {
        // FU-A needs two header bytes and at least one NAL byte.
        (budget.payload_bytes() >= 3).then(|| Self(Packetizer::new(budget.payload_bytes())))
    }

    /// Consume one bounded access unit and allocate at most the next payload
    /// per iterator step. The caller owns the source sequence across units.
    pub fn packetize(
        &self,
        first_sequence: u32,
        timestamp: u32,
        unit: Vec<u8>,
    ) -> Result<H264Packets, PacketizeError> {
        if unit.len() > MAX_ACCESS_UNIT_SIZE {
            return Err(PacketizeError::TooLarge);
        }
        let mut chunks = self.0.clone().into_packets(unit);
        let first = chunks.next().ok_or(PacketizeError::Empty)?;
        Ok(H264Packets {
            chunks,
            first: Some(first),
            sequence: first_sequence,
            timestamp,
        })
    }
}

pub struct H264Packets {
    chunks: Packetization,
    first: Option<PacketizedChunk>,
    sequence: u32,
    timestamp: u32,
}

impl Iterator for H264Packets {
    type Item = Packet<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        let chunk = self.first.take().or_else(|| self.chunks.next())?;
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        Some(Packet {
            sequence,
            timestamp: self.timestamp,
            marker: chunk.end_of_frame,
            first: chunk.start_of_frame,
            last: chunk.end_of_frame,
            payload: chunk.payload,
        })
    }
}

#[cfg(test)]
mod tests;
