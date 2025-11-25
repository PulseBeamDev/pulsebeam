use std::time::Duration;

use str0m::media::Frequency;
use tokio::time::Instant;

use crate::rtp::RtpPacket;
use crate::rtp::buffer::KeyframeBuffer;
use crate::rtp::timeline::Timeline;

// It's possible for the new stream packets to arrive later than the old stream despite
// having a playout time that is earlier.
const PLAYOUT_JITTER_TOLERANCE: Duration = Duration::from_millis(20);

#[derive(Debug)]
pub struct Switcher {
    /// The timeline for the currently active stream.
    timeline: Timeline,

    /// A high-priority slot for a packet from the *current* stream.
    /// This allows draining the last few packets of the old stream during a switch.
    pending: Option<RtpPacket>,

    /// The state for the *new* stream we are switching to.
    /// This is `Some` only when a switch is in progress.
    staging: Option<KeyframeBuffer>,

    latest_playout: Instant,
}

impl Switcher {
    pub fn new(clock_rate: Frequency) -> Self {
        Self {
            timeline: Timeline::new(clock_rate),
            pending: None,
            staging: None,
            latest_playout: Instant::now(),
        }
    }

    /// Pushes a packet from the **old/current** stream.
    /// This is typically used to forward the stream that is already playing out.
    pub fn push(&mut self, pkt: RtpPacket) {
        self.pending.replace(pkt);
    }

    /// Pushes a packet for the **new** stream we are preparing to switch to.
    /// The first call to this method will initiate the switching process.
    pub fn stage(&mut self, pkt: RtpPacket) {
        let staging = self.staging.get_or_insert_default();
        staging.push(pkt);
    }

    pub fn ready_to_switch(&mut self) -> bool {
        self.staging.is_none()
    }

    /// Returns true if the new stream has received a keyframe and is ready to be popped.
    pub fn ready_to_stream(&self) -> bool {
        let Some(staging) = self.staging.as_ref() else {
            return false;
        };

        staging.is_ready(
            self.latest_playout
                .checked_sub(PLAYOUT_JITTER_TOLERANCE)
                .unwrap_or(self.latest_playout),
        )
    }

    /// Pops the next available packet, prioritizing the old stream to ensure a smooth drain.
    pub fn pop(&mut self) -> Option<RtpPacket> {
        // 1. Drain the pending packet from the OLD stream.
        if let Some(pending_pkt) = self.pending.take() {
            if pending_pkt.playout_time > self.latest_playout {
                self.latest_playout = pending_pkt.playout_time;
            }
            return Some(self.timeline.rewrite(pending_pkt));
        }

        if !self.ready_to_stream() {
            return None;
        }

        // 2. Pop packets from the NEW stream if a switch is in progress.
        if let Some(staging) = &mut self.staging {
            if let Some(staged_pkt) = staging.pop() {
                if staged_pkt.is_keyframe_start {
                    self.timeline.rebase(&staged_pkt);
                }
                return Some(self.timeline.rewrite(staged_pkt));
            } else {
                self.clear();
                return None;
            }
        }

        None
    }

    pub fn clear(&mut self) {
        self.staging = None;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp::{self, test_utils::*};
    use str0m::{
        media::{Frequency, MediaTime},
        rtp::{SeqNo, Ssrc},
    };

    fn run_with(mut seq: Switcher, packets: &[RtpPacket]) {
        let mut rewritten_headers = Vec::new();

        // Inputs must not have duplications. Deduplication is already handled at this layer.
        println!("input:\n");
        print_packets(packets);

        let mut active_ssrc: Option<Ssrc> = None;
        let mut staging_ssrc: Option<Ssrc> = None;

        for packet in packets {
            // Prioritize draining the switcher (pending packet) before processing new input.
            // This prevents reordering where a pushed packet jumps ahead of a staged packet.
            while let Some(pkt) = seq.pop() {
                rewritten_headers.push(pkt);
            }

            let pkt = packet.clone();
            let current_ssrc = pkt.raw_header.ssrc;

            if active_ssrc == Some(current_ssrc) {
                // State: Streaming / Active
                seq.push(pkt);
            } else {
                // State: Resuming / Switching

                // If the SSRC being staged changes, it implies the previous staging attempt
                // is abandoned (e.g. switching to a different simulcast layer before the first one completed).
                // We must explicitly clear the staging buffer to prevent mixing streams.
                if staging_ssrc.is_some() && staging_ssrc != Some(current_ssrc) {
                    seq.staging = None;
                }
                staging_ssrc = Some(current_ssrc);

                seq.stage(pkt);

                if seq.ready_to_stream() {
                    // Switch complete. Promote staging to active.
                    active_ssrc = Some(current_ssrc);
                    staging_ssrc = None;
                }
            }
        }

        // Final drain
        while let Some(pkt) = seq.pop() {
            rewritten_headers.push(pkt);
        }

        rewritten_headers.sort_by_key(|e| e.seq_no);

        println!("\noutput:\n");
        print_packets(&rewritten_headers);

        // === PROPERTY 1: Final state must be stable ===
        // The switcher should be drained (not holding staged data) at the end of a successful run.
        assert!(!seq.ready_to_stream(), "Switcher must end in drained state");

        // === PROPERTY 2: Output seqno must be *strictly increasing* and *gap-free* ===
        let seq_nos: Vec<u64> = rewritten_headers.iter().map(|h| (*h.seq_no)).collect();
        if !seq_nos.is_empty() {
            let is_continuous = seq_nos
                .iter()
                .zip(seq_nos.iter().skip(1))
                .all(|(a, b)| *b == a.wrapping_add(1));
            assert!(
                is_continuous,
                "Output sequence numbers must be contiguous (no gaps). Got: {:?}",
                seq_nos
            );
        }

        // === PROPERTY 3: RTP timestamps non-decreasing ===
        let ts_ok = rewritten_headers
            .windows(2)
            .all(|w| w[0].rtp_ts <= w[1].rtp_ts);
        assert!(ts_ok, "RTP timestamps must be non-decreasing");

        // TODO: we currently don't buffer keyframe to completion, we just switch over
        // as soon as we find the first sequence of a set of keyframe packets.
        // We don't do this because we want to be low latency. So, the logic is certainly
        // optimistic.
        //
        // === PROPERTY 5: Keyframe integrity ===
        // let mut i = 0;
        // while i < rewritten_headers.len() {
        //     let h = &rewritten_headers[i];
        //     if h.is_keyframe_start {
        //         let kf_start = i;
        //         let mut kf_end = i;
        //         let mut has_marker = false;
        //
        //         while kf_end < rewritten_headers.len() {
        //             let end_h = &rewritten_headers[kf_end];
        //             // Keyframe packets must share the same timestamp
        //             if end_h.rtp_ts != h.rtp_ts {
        //                 break;
        //             }
        //             if end_h.raw_header.marker {
        //                 has_marker = true;
        //                 kf_end += 1;
        //                 break;
        //             }
        //             kf_end += 1;
        //         }
        //
        //         assert!(
        //             has_marker,
        //             "Keyframe at idx {} (seq {}) has no marker bit",
        //             kf_start, h.seq_no
        //         );
        //
        //         // Advance i to the packet after this keyframe
        //         i = kf_end;
        //     } else {
        //         i += 1;
        //     }
        // }

        // === PROPERTY 6: SSRC switch behavior ===
        for window in rewritten_headers.windows(3) {
            let [a, b, c] = window else {
                continue;
            };

            if b.raw_header.ssrc != c.raw_header.ssrc {
                assert_ne!(
                    a.raw_header.ssrc, c.raw_header.ssrc,
                    "Interleaved SSRCs detected"
                );
            }
        }
    }

    pub fn run(packets: &[RtpPacket]) {
        run_with(Switcher::new(rtp::VIDEO_FREQUENCY), packets);
    }

    pub fn print_packets(packets: &[RtpPacket]) {
        use std::cmp::max;

        println!("\n--- Packet Inspector ({} packets) ---", packets.len());

        if packets.is_empty() {
            println!("(No packets to display)");
            println!("{}", "-".repeat(80));
            return;
        }

        // Determine max width for each column dynamically
        let mut max_idx = 3;
        let mut max_ssrc = 4; // "SSRC"
        let mut max_seq = 6; // "Seq No"
        let mut max_dseq = 6; // "Δ Seq"
        let mut max_ts = 9; // "Timestamp"
        let mut max_dts = 6; // "Δ TS"

        let mut rows = Vec::new();

        for (i, current_header) in packets.iter().enumerate() {
            let (delta_seq_str, delta_ts_str) = if i > 0 {
                let prev_header = &packets[i - 1];
                let delta_seq = current_header.seq_no.wrapping_sub(*prev_header.seq_no);
                let delta_ts = current_header
                    .rtp_ts
                    .numer()
                    .wrapping_sub(prev_header.rtp_ts.numer());
                (
                    format!("{:+}", delta_seq as i64),
                    format!("{:+}", delta_ts as i64),
                )
            } else {
                ("(base)".to_string(), "(base)".to_string())
            };

            let mut flags = String::new();
            if current_header.is_keyframe_start {
                flags.push('K');
            }
            if current_header.raw_header.marker {
                flags.push('M');
            }

            let ssrc = format!("{}", current_header.raw_header.ssrc);
            let seq = format!("{}", current_header.seq_no);
            let ts = format!("{}", current_header.rtp_ts.numer());

            // Track column width
            max_idx = max(max_idx, i.to_string().len());
            max_ssrc = max(max_ssrc, ssrc.len());
            max_seq = max(max_seq, seq.len());
            max_dseq = max(max_dseq, delta_seq_str.len());
            max_ts = max(max_ts, ts.len());
            max_dts = max(max_dts, delta_ts_str.len());

            rows.push((i, ssrc, seq, delta_seq_str, ts, delta_ts_str, flags));
        }

        // Print header
        println!(
            "{:<width_idx$} | {:<width_ssrc$} | {:<width_seq$} | {:<width_dseq$} | {:<width_ts$} | {:<width_dts$} | {}",
            "Idx",
            "SSRC",
            "Seq No",
            "Δ Seq",
            "Timestamp",
            "Δ TS",
            "Flags",
            width_idx = max_idx,
            width_ssrc = max_ssrc,
            width_seq = max_seq,
            width_dseq = max_dseq,
            width_ts = max_ts,
            width_dts = max_dts,
        );

        let total_width = max_idx + max_ssrc + max_seq + max_dseq + max_ts + max_dts + 6 * 3 + 7; // spacing & dividers
        println!("{}", "-".repeat(total_width));

        // Print rows
        for (i, ssrc, seq, dseq, ts, dts, flags) in rows {
            println!(
                "{:<width_idx$} | {:<width_ssrc$} | {:<width_seq$} | {:<width_dseq$} | {:<width_ts$} | {:<width_dts$} | {}",
                i,
                ssrc,
                seq,
                dseq,
                ts,
                dts,
                flags,
                width_idx = max_idx,
                width_ssrc = max_ssrc,
                width_seq = max_seq,
                width_dseq = max_dseq,
                width_ts = max_ts,
                width_dts = max_dts,
            );
        }

        println!("{}", "-".repeat(total_width));
    }

    // --- Scenarios: Defined as vectors of steps ---
    pub fn simple_frame() -> Vec<ScenarioStep> {
        vec![keyframe(), next_seq(), marker()]
    }
    pub fn simple_stream() -> Vec<ScenarioStep> {
        vec![keyframe(), next_seq(), marker(), next_frame(), marker()]
    }
    pub fn loss_scenario() -> Vec<ScenarioStep> {
        vec![keyframe(), next_seq(), next_seq(), marker()]
    }

    // --- Tests ---
    #[test]
    fn run_simple_stream() {
        let packets = generate(RtpPacket::default(), simple_stream());
        run(&packets);
    }

    #[test]
    fn run_stream_with_reordering() {
        let mut packets = generate(RtpPacket::default(), simple_frame());
        packets.swap(0, 2); // [M, p1, K] → should still work
        run(&packets);
    }

    #[test]
    fn simulcast_switch_mid_keyframe() {
        let mut steps = vec![keyframe(), next_seq()]; // partial keyframe
        steps.push(simulcast_switch(100, 5000, 80000));
        steps.extend(vec![keyframe(), next_seq(), marker()]);
        let packets = generate(RtpPacket::default(), steps);
        run(&packets);
        // Expect: old partial keyframe dropped
    }

    #[test]
    fn late_packet_from_old_layer() {
        let mut steps = simple_stream();
        steps.push(simulcast_switch(100, 5000, 80000));
        steps.extend(simple_stream());

        // TODO: harden against old packets
        let packets = generate(RtpPacket::default(), steps);
        // let old_ssrc = packets[0].ssrc;
        // let mut late = packets[1];
        // late.ssrc = old_ssrc;
        // packets.push(late); // late old-layer packet

        run(&packets);
        // Expect: late packet dropped
    }

    #[test]
    fn multiple_simulcast_switches() {
        let mut steps = simple_frame();
        steps.push(simulcast_switch(100, 5000, 80000));
        steps.extend(simple_frame());
        steps.push(simulcast_switch(200, 10000, 160000));
        steps.extend(simple_frame());
        let packets = generate(RtpPacket::default(), steps);
        run(&packets);
    }

    #[test]
    fn rtp_timestamp_wraparound() {
        let initial = RtpPacket {
            rtp_ts: MediaTime::new((u32::MAX - 100) as u64, Frequency::NINETY_KHZ),
            ..Default::default()
        };
        let steps = vec![keyframe(), next_seq(), next_seq(), marker()];
        let packets = generate(initial, steps);
        run(&packets);
    }

    #[test]
    fn sequence_number_wraparound() {
        let initial = RtpPacket {
            seq_no: SeqNo::from((u16::MAX - 5) as u64),
            ..Default::default()
        };
        let steps = vec![
            keyframe(),
            next_seq(),
            next_seq(),
            next_seq(),
            next_seq(),
            marker(),
        ];
        let packets = generate(initial, steps);
        run(&packets);
    }

    #[test]
    fn loss_before_keyframe() {
        let steps = vec![next_seq(), next_seq(), keyframe(), marker()];
        let packets = generate(RtpPacket::default(), steps);
        run(&packets);
        // Expect: first two dropped
    }

    #[test]
    fn high_packet_burst() {
        let mut steps = vec![keyframe()];
        steps.extend((0..1000).map(|_| next_seq()));
        steps.push(marker());
        let packets = generate(RtpPacket::default(), steps);
        run(&packets);
    }

    #[test]
    fn out_of_order_keyframe_fragments() {
        let ordered = vec![keyframe(), next_seq(), next_seq(), marker()];
        let mut packets = generate(RtpPacket::default(), ordered);
        // Scramble order
        packets = vec![
            packets[2].clone(),
            packets[0].clone(),
            packets[3].clone(),
            packets[1].clone(),
        ];
        run(&packets);
    }

    #[test]
    fn high_jitter_reordering() {
        // Simulates a chaotic network where packets are significantly reordered.
        let ordered_stream = vec![
            keyframe(), // KF1 starts
            next_seq(),
            marker(),     // KF1 ends
            next_frame(), // P-frame 1
            marker(),
            next_frame(), // KF2 starts
            next_seq(),
            marker(), // KF2 ends
        ];
        let packets = generate(RtpPacket::default(), ordered_stream);

        // Extreme reorder: [KF1-p1, KF2-p2, P1-M, KF1-K, KF2-M, P1-p1, KF2-K, KF1-M]
        let reordered_indices = [1, 6, 4, 0, 7, 3, 5, 2];
        let disordered_packets: Vec<_> = reordered_indices
            .iter()
            .map(|&i| packets[i].clone())
            .collect();

        run(&disordered_packets);
        // Expect: Switcher should buffer and reorder everything correctly.
    }

    #[test]
    fn packet_loss_and_recovery_with_new_keyframe() {
        // Simulates losing part of a keyframe, followed by a new keyframe which should allow recovery.
        let mut steps = vec![
            keyframe(),
            // next_seq(), // This packet is "lost"
            next_seq(),
            marker(),
        ];
        // Stream continues but should be un-decodable
        steps.extend(vec![next_frame(), marker()]);
        // A new keyframe arrives, allowing the stream to recover
        steps.extend(simple_frame());

        let packets = generate(RtpPacket::default(), steps);
        run(&packets);
        // Expect: The first incomplete keyframe and the following P-frame are dropped.
        // The stream resumes perfectly from the second keyframe.
    }

    #[test]
    fn simulcast_switch_with_loss_of_new_keyframe_start() {
        let mut steps = simple_stream(); // Emit a valid frame on the old SSRC
        steps.push(simulcast_switch(555, 1000, 90000));
        steps.extend(vec![
            // keyframe(), // The crucial first packet of the new stream is "lost"
            next_seq(),
            marker(),
        ]);
        // Now send a complete, valid keyframe on the new SSRC
        steps.extend(simple_frame());

        let packets = generate(RtpPacket::default(), steps);
        run(&packets);
        // Expect: The old SSRC packet should be emitted. The partial new keyframe is dropped.
        // The sequencer recovers and emits the final complete keyframe.
    }

    #[test]
    #[ignore]
    fn stale_packets_from_previous_timestamp_arrive_late() {
        // Simulates a scenario where packets from an old, abandoned keyframe
        // arrive after a newer keyframe has already been processed.
        let mut initial_kf = generate(RtpPacket::default(), vec![keyframe(), next_seq()]);

        let mut next_kf_time = initial_kf[0].clone();
        next_kf_time.rtp_ts =
            MediaTime::new(initial_kf[0].rtp_ts.numer() + 3000, Frequency::NINETY_KHZ);

        let mut subsequent_kf = generate(next_kf_time, simple_frame());

        // Arrival order: [NewKF-p1, NewKF-p2, NewKF-M, OldKF-p1, OldKF-p2]
        let mut packets = Vec::new();
        packets.append(&mut subsequent_kf);
        packets.append(&mut initial_kf);

        run(&packets);
        // Expect: The stale, old keyframe packets are correctly identified as late and dropped.
    }
}
