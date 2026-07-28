//! Invariants the egress RTP stream must satisfy for a subscriber to decode it.
//!
//! Subscribers have no jitter buffer in front of the SFU, so anything the
//! forwarder emits is what the decoder sees. These checks encode the contract
//! a switching forwarder owes that decoder, independent of how switching is
//! implemented internally.

use crate::rtp::RtpPacket;

#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    pub index: usize,
    pub reason: String,
}

/// Verifies an emitted egress stream.
///
/// `expect_leading_parameter_sets` requires that the very first decodable frame
/// carries SPS and PPS; every later keyframe must carry them too, because the
/// SFU keeps one egress SSRC across switches and each simulcast layer has its
/// own SPS.
pub fn check_egress(packets: &[RtpPacket]) -> Vec<Violation> {
    let mut violations = Vec::new();
    let mut push = |index: usize, reason: String| violations.push(Violation { index, reason });

    if packets.is_empty() {
        return violations;
    }

    for (i, w) in packets.windows(2).enumerate() {
        let (prev, cur) = (&w[0], &w[1]);
        let expected = (*prev.seq_no).wrapping_add(1);
        if *cur.seq_no != expected {
            push(
                i + 1,
                format!(
                    "sequence discontinuity: {} followed by {} (expected {})",
                    *prev.seq_no, *cur.seq_no, expected
                ),
            );
        }
        if cur.rtp_ts.numer() < prev.rtp_ts.numer() {
            push(
                i + 1,
                format!(
                    "rtp timestamp went backwards: {} followed by {}",
                    prev.rtp_ts.numer(),
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
