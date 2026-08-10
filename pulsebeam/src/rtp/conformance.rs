#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::string_slice,
    clippy::indexing_slicing
)] // test / simulation support
//! Overflow is allowed here: this module is `#[cfg(test)]`-only, and a check
//! whose own arithmetic overflows should fail loudly rather than clamp into a
//! passing verdict about the stream it is judging.
#![allow(clippy::arithmetic_side_effects)]

//! Invariants the egress RTP stream must satisfy for a subscriber to decode it.
//!
//! Subscribers have no jitter buffer in front of the SFU, so anything the
//! forwarder emits is what the decoder sees. These checks encode the contract
//! a switching forwarder owes that decoder, independent of how switching is
//! implemented internally.

use ahash::{HashSet, HashSetExt};

use crate::rtp::RtpPacket;

#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    pub index: usize,
    pub reason: String,
}

/// Verifies an emitted egress stream, assuming nothing was lost upstream.
///
/// Every keyframe must carry SPS and PPS, because the SFU keeps one egress SSRC
/// across switches while each simulcast layer has its own parameter sets.
///
/// Sequence gaps are not themselves defects — they are how loss is reported, and
/// a subscriber knows to discard what they damage. The dangerous case is the
/// opposite one: a frame the forwarder cut short but delivered contiguously, so
/// the decoder has no way to know it is incomplete and renders it anyway.
///
/// Frame structure is judged in sequence order rather than the order packets
/// were written, because sequence order is what the subscriber reassembles.
/// Emitting a packet late is the network's doing and ours to pass on faithfully.
pub fn check_egress(packets: &[RtpPacket]) -> Vec<Violation> {
    let mut violations = Vec::new();
    let mut push = |index: usize, reason: String| violations.push(Violation { index, reason });

    if packets.is_empty() {
        return violations;
    }

    // A packet may legitimately come out below the ones before it: the network
    // reordered it and we forward it where it belongs. What must never happen is
    // the same output sequence number being used twice.
    let mut seen = HashSet::new();
    for (i, pkt) in packets.iter().enumerate() {
        if !seen.insert(*pkt.seq_no) {
            push(i, format!("output sequence {} emitted twice", *pkt.seq_no));
        }
    }

    let mut ordered: Vec<&RtpPacket> = packets.iter().collect();
    ordered.sort_by_key(|p| *p.seq_no);
    let packets: Vec<RtpPacket> = ordered.into_iter().cloned().collect();
    let packets = &packets[..];

    for (i, w) in packets.windows(2).enumerate() {
        let (prev, cur) = (&w[0], &w[1]);
        if cur.rtp_ts.numer() < prev.rtp_ts.numer() {
            push(
                i + 1,
                format!(
                    "rtp timestamp went backwards: sequence {} carried {} but {} carried {}",
                    *prev.seq_no,
                    prev.rtp_ts.numer(),
                    *cur.seq_no,
                    cur.rtp_ts.numer()
                ),
            );
        }
    }

    // Frames are delimited by the RTP timestamp. Two frames must never share a
    // timestamp, and a frame's packets must never be split by another frame's.
    let mut frame_starts: Vec<(usize, u64)> = Vec::new();
    for (i, pkt) in packets.iter().enumerate() {
        let ts = pkt.rtp_ts.numer();
        if frame_starts.last().map(|&(_, t)| t) != Some(ts) {
            if let Some(pos) = frame_starts.iter().position(|&(_, t)| t == ts) {
                push(
                    i,
                    format!(
                        "frame with rtp_ts {ts} resumed at index {i} after starting at {}",
                        frame_starts[pos].0
                    ),
                );
            }
            frame_starts.push((i, ts));
        }
    }

    // Every decodable entry point must carry its parameter sets. This is the
    // invariant that a naive keyframe-only cache violates.
    let mut frame_start = 0usize;
    let mut frame_ts = packets[0].rtp_ts.numer();
    let mut seen_decodable = false;
    let mut i = 0usize;
    while i <= packets.len() {
        let boundary = i == packets.len() || packets[i].rtp_ts.numer() != frame_ts;
        if boundary {
            let frame = &packets[frame_start..i];

            // A frame the forwarder cut short must be followed by a sequence
            // gap, or the subscriber reassembles a partial frame believing it
            // is whole and feeds it to the decoder.
            if i < packets.len() && !frame.iter().any(|p| p.marker) {
                let last = &packets[i - 1];
                let next = &packets[i];
                if *next.seq_no == (*last.seq_no).wrapping_add(1) {
                    push(
                        frame_start,
                        format!(
                            "frame at rtp_ts {frame_ts} was truncated (no marker) but the next \
                             frame follows contiguously, so the subscriber cannot tell it is \
                             incomplete"
                        ),
                    );
                }
            }

            let has_idr = frame.iter().any(|p| p.nal.idr());
            if has_idr {
                let has_sps = frame.iter().any(|p| p.nal.sps());
                let has_pps = frame.iter().any(|p| p.nal.pps());
                if !has_sps || !has_pps {
                    push(
                        frame_start,
                        format!(
                            "keyframe at rtp_ts {frame_ts} is missing parameter sets (sps={has_sps}, pps={has_pps})"
                        ),
                    );
                }
                seen_decodable = true;
            }
            if i < packets.len() {
                frame_start = i;
                frame_ts = packets[i].rtp_ts.numer();
            }
        }
        i += 1;
    }

    if !seen_decodable {
        push(0, "stream contains no decodable keyframe".to_string());
    }

    violations
}

/// Panics with a readable report if `packets` violates any egress invariant.
#[track_caller]
pub fn assert_decodable(packets: &[RtpPacket], what: &str) {
    let violations = check_egress(packets);
    if violations.is_empty() {
        return;
    }
    let mut msg = format!(
        "{what}: {} egress violation(s) across {} packets\n",
        violations.len(),
        packets.len()
    );
    for v in violations.iter().take(20) {
        msg.push_str(&format!("  [{}] {}\n", v.index, v.reason));
    }
    msg.push_str("\nemitted stream:\n");
    for (i, p) in packets.iter().enumerate().take(40) {
        msg.push_str(&format!(
            "  {i:>3} seq={} ts={} marker={} nal={:?}\n",
            *p.seq_no,
            p.rtp_ts.numer(),
            p.marker,
            p.nal
        ));
    }
    panic!("{msg}");
}
