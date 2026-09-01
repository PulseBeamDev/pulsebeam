//! Codec-agnostic RTP framing.
//!
//! Frame boundaries come from the RTP sequence numbers and per-packet framing
//! flags (start/end of frame), never from parsing the media payload. That is
//! what makes this safe for end-to-end-encrypted media: the bytes can be opaque
//! ciphertext and the pipeline still packetizes, forwards, sheds, and reassembles
//! them. The scalability semantics (keyframes, decode targets) ride alongside in
//! the Dependency Descriptor, also outside the payload.

use std::collections::BTreeMap;

/// Payload bytes that fit in one RTP packet after headers/extensions. Kept well
/// under a 1200-byte MTU to leave room for the DD and other header extensions.
pub const DEFAULT_MTU_PAYLOAD: usize = 1100;
pub const MAX_FRAME_SIZE: usize = 16usize * 1024 * 1024;

/// One packet's worth of a frame, plus where it sits in the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameChunk<'a> {
    pub data: &'a [u8],
    /// First packet of the frame — set the DD's `start_of_frame`.
    pub start_of_frame: bool,
    /// Last packet of the frame — set the DD's `end_of_frame` and the RTP marker.
    pub end_of_frame: bool,
}

/// A reassembled frame plus the sequence-number span it occupied, so the caller
/// can judge inter-frame continuity (a gap between frames = a lost/dropped frame).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReassembledFrame {
    pub data: Vec<u8>,
    pub first_seq: u64,
    pub last_seq: u64,
}

/// Splits an opaque frame into ordered chunks without reading its bytes.
#[derive(Debug, Clone)]
pub struct FramePacketizer {
    mtu: usize,
}

impl Default for FramePacketizer {
    fn default() -> Self {
        Self::new(DEFAULT_MTU_PAYLOAD)
    }
}

impl FramePacketizer {
    pub fn new(mtu: usize) -> Self {
        debug_assert!(mtu > 0, "packet payload budget must be positive");
        Self { mtu: mtu.max(1) }
    }

    /// The number of packets `frame` will produce (always at least one, so an
    /// empty frame still yields a single empty packet carrying its framing).
    pub fn packet_count(&self, frame: &[u8]) -> usize {
        frame.len().div_ceil(self.mtu).max(1)
    }

    pub fn packetize<'a>(&self, frame: &'a [u8]) -> impl Iterator<Item = FrameChunk<'a>> {
        let mtu = self.mtu;
        let total = self.packet_count(frame);
        (0..total).map(move |i| {
            let start = i.saturating_mul(mtu);
            let end = i.saturating_add(1).saturating_mul(mtu).min(frame.len());
            debug_assert!(
                start <= end && end <= frame.len(),
                "chunk {start}..{end} escapes a {}-byte frame",
                frame.len()
            );
            FrameChunk {
                data: frame.get(start..end).unwrap_or_default(),
                start_of_frame: i == 0,
                end_of_frame: i == total.saturating_sub(1),
            }
        })
    }
}

/// Reassembles frames from RTP packets by sequence contiguity and the
/// end-of-frame flag. Tolerates reordering within a bounded window; a frame with
/// a gap that never fills is dropped rather than emitted torn.
#[derive(Debug)]
pub struct FrameDepacketizer {
    /// Buffered payloads by sequence number, pruned as frames complete or evict.
    parts: BTreeMap<u64, BufferedPacket>,
    /// Highest sequence number seen; the window trails it.
    newest: Option<u64>,
    window: usize,
}

#[derive(Debug)]
struct BufferedPacket {
    payload: Vec<u8>,
    start_of_frame: bool,
    end_of_frame: bool,
}

impl Default for FrameDepacketizer {
    fn default() -> Self {
        Self::new(512)
    }
}

impl FrameDepacketizer {
    pub fn new(window: usize) -> Self {
        Self {
            parts: BTreeMap::new(),
            newest: None,
            window: window.max(1),
        }
    }

    /// Feed one packet. Returns a reassembled frame payload once one becomes
    /// contiguously complete — which may be on the end packet, or on a late
    /// middle packet that fills the last gap of an already-buffered frame.
    pub fn push(
        &mut self,
        seq: u64,
        payload: &[u8],
        start_of_frame: bool,
        end_of_frame: bool,
    ) -> Option<ReassembledFrame> {
        self.newest = Some(self.newest.map_or(seq, |n| n.max(seq)));
        self.parts.insert(
            seq,
            BufferedPacket {
                payload: payload.to_vec(),
                start_of_frame,
                end_of_frame,
            },
        );
        self.evict_stale();
        self.try_emit_earliest()
    }

    /// Assemble the earliest frame whose start packet is buffered and whose run
    /// up to an end-of-frame packet is contiguous. Returns and consumes it.
    fn try_emit_earliest(&mut self) -> Option<ReassembledFrame> {
        let start_seq = self
            .parts
            .iter()
            .find(|(_, p)| p.start_of_frame)
            .map(|(&s, _)| s)?;

        let mut seq = start_seq;
        loop {
            let pkt = self.parts.get(&seq)?; // a gap stalls until it fills
            if pkt.end_of_frame {
                let mut frame = Vec::new();
                for s in start_seq..=seq {
                    // The walk above proved every sequence in this range is present.
                    let part = self.parts.get(&s)?;
                    frame.extend_from_slice(&part.payload);
                }
                for s in start_seq..=seq {
                    self.parts.remove(&s);
                }
                return Some(ReassembledFrame {
                    data: frame,
                    first_seq: start_seq,
                    last_seq: seq,
                });
            }
            seq = seq.saturating_add(1);
        }
    }

    fn evict_stale(&mut self) {
        let Some(newest) = self.newest else {
            return;
        };
        let floor = newest.saturating_sub(self.window as u64);
        while let Some((&oldest, _)) = self.parts.iter().next() {
            if oldest >= floor {
                break;
            }
            self.parts.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    use super::*;

    fn chunks(frame: &[u8], mtu: usize) -> Vec<FrameChunk<'_>> {
        FramePacketizer::new(mtu).packetize(frame).collect()
    }

    #[test]
    fn a_frame_smaller_than_the_mtu_is_a_single_packet() {
        let c = chunks(&[1, 2, 3], 1100);
        assert_eq!(c.len(), 1);
        assert!(c[0].start_of_frame && c[0].end_of_frame);
        assert_eq!(c[0].data, &[1, 2, 3]);
    }

    #[test]
    fn an_empty_frame_still_carries_its_framing() {
        let c = chunks(&[], 1100);
        assert_eq!(c.len(), 1);
        assert!(c[0].start_of_frame && c[0].end_of_frame);
        assert!(c[0].data.is_empty());
    }

    #[test]
    fn a_large_frame_splits_with_start_and_end_only_at_the_edges() {
        let frame: Vec<u8> = (0..2500u32)
            .map(|i| u8::try_from(i % 256).expect("masked to a byte"))
            .collect();
        let c = chunks(&frame, 1000);
        assert_eq!(c.len(), 3);
        assert!(c[0].start_of_frame && !c[0].end_of_frame);
        assert!(!c[1].start_of_frame && !c[1].end_of_frame);
        assert!(!c[2].start_of_frame && c[2].end_of_frame);
        let rejoined: Vec<u8> = c.iter().flat_map(|p| p.data.iter().copied()).collect();
        assert_eq!(rejoined, frame);
    }

    /// The core property: any opaque payload round-trips byte-exact through
    /// packetize → depacketize, regardless of content (so encrypted media works).
    #[test]
    fn opaque_payload_round_trips_byte_exact() {
        let frame: Vec<u8> = (0..3333u32)
            .map(|i| u8::try_from((i * 7 + 1) % 256).expect("masked to a byte"))
            .collect();
        let p = FramePacketizer::new(1000);
        let mut d = FrameDepacketizer::default();

        let mut out = None;
        for (i, chunk) in p.packetize(&frame).enumerate() {
            out = d.push(
                i as u64,
                chunk.data,
                chunk.start_of_frame,
                chunk.end_of_frame,
            );
        }
        assert_eq!(out.expect("frame reassembled").data, frame);
    }

    #[test]
    fn reassembles_across_reordered_packets() {
        let frame: Vec<u8> = (0..2500u32)
            .map(|i| u8::try_from(i % 256).expect("masked to a byte"))
            .collect();
        let p = FramePacketizer::new(1000);
        let chunks: Vec<_> = p.packetize(&frame).collect();
        let mut d = FrameDepacketizer::default();

        // Deliver the middle packet last.
        assert!(d.push(0, chunks[0].data, true, false).is_none());
        assert!(
            d.push(2, chunks[2].data, false, true).is_none(),
            "gap: not yet complete"
        );
        let out = d.push(1, chunks[1].data, false, false);
        assert_eq!(out.expect("completes once the gap fills").data, frame);
    }

    #[test]
    fn a_frame_with_a_permanent_gap_is_never_emitted() {
        let frame: Vec<u8> = (0..2500u32)
            .map(|i| u8::try_from(i % 256).expect("masked to a byte"))
            .collect();
        let p = FramePacketizer::new(1000);
        let chunks: Vec<_> = p.packetize(&frame).collect();
        let mut d = FrameDepacketizer::default();

        d.push(0, chunks[0].data, true, false);
        // Packet seq 1 is lost; the end packet cannot assemble a torn frame.
        assert!(d.push(2, chunks[2].data, false, true).is_none());
    }

    #[test]
    fn two_frames_in_sequence_each_emit_once() {
        let f1 = vec![1u8; 1500];
        let f2 = vec![2u8; 800];
        let p = FramePacketizer::new(1000);
        let mut d = FrameDepacketizer::default();

        let mut seq = 0u64;
        let mut emitted = Vec::new();
        for chunk in p.packetize(&f1) {
            if let Some(f) = d.push(seq, chunk.data, chunk.start_of_frame, chunk.end_of_frame) {
                emitted.push(f.data);
            }
            seq += 1;
        }
        for chunk in p.packetize(&f2) {
            if let Some(f) = d.push(seq, chunk.data, chunk.start_of_frame, chunk.end_of_frame) {
                emitted.push(f.data);
            }
            seq += 1;
        }
        assert_eq!(emitted, vec![f1, f2]);
    }
}
