use crate::rtp::RtpPacket;
use str0m::{
    media::{Frequency, MediaTime},
    rtp::SeqNo,
};
use tokio::time::Instant;

// Timeline allows switching between input streams that
// output a stream with a continuous seq_no and playout_time
// as if it comes from a single stream.
#[derive(Debug)]
pub struct Timeline {
    clock_rate: Frequency,
    highest_seq_no: SeqNo,
    offset_seq_no: u64,
    anchor: Option<Instant>,
}

impl Timeline {
    pub fn new(clock_rate: Frequency) -> Self {
        let base_seq_no: u16 = rand::random();
        Self {
            clock_rate,
            highest_seq_no: SeqNo::from(base_seq_no as u64),
            offset_seq_no: 0,
            anchor: None,
        }
    }

    /// Re-aligns the timeline to a new stream starting with `packet`.
    /// `packet` must be a keyframe (start of a new GOP).
    pub fn rebase(&mut self, packet: &RtpPacket) {
        debug_assert!(
            packet.is_keyframe_start,
            "Rebase should only happen on keyframe boundaries"
        );

        // We want the new packet to immediately follow the last output packet.
        // target = highest_output + 1
        // target = input + offset  =>  offset = target - input
        let target_seq_no = self.highest_seq_no.wrapping_add(1);
        self.offset_seq_no = target_seq_no.wrapping_sub(*packet.seq_no);

        // Initialize anchor if this is the very first packet
        if self.anchor.is_none() {
            self.anchor = Some(packet.playout_time);
        }
    }

    pub fn rewrite(&mut self, mut pkt: RtpPacket) -> RtpPacket {
        let new_seq_u64 = pkt.seq_no.wrapping_add(self.offset_seq_no);
        pkt.seq_no = new_seq_u64.into();

        if new_seq_u64 > *self.highest_seq_no {
            self.highest_seq_no = pkt.seq_no;
        }

        let anchor = self
            .anchor
            .expect("rebase must have occured before rewriting to create the first anchor");

        let duration = pkt.playout_time.saturating_duration_since(anchor);
        pkt.rtp_ts = MediaTime::from(duration).rebase(self.clock_rate);

        pkt
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_simple_continuity() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new(Frequency::NINETY_KHZ);

        // Packet 1: First packet (Keyframe) - requires rebase
        let p1 = RtpPacket {
            seq_no: 100.into(),
            playout_time: start_time,
            is_keyframe_start: true,
            ..Default::default()
        };

        timeline.rebase(&p1);
        let out1 = timeline.rewrite(p1);
        let base_seq = *out1.seq_no;

        // Packet 2: Regular packet, 100ms later
        let p2 = RtpPacket {
            seq_no: 101.into(),
            playout_time: start_time + Duration::from_millis(100),
            is_keyframe_start: false,
            ..Default::default()
        };

        let out2 = timeline.rewrite(p2);

        // Verify Sequence Continuity
        assert_eq!(
            *out2.seq_no,
            base_seq + 1,
            "Sequence number should increment by 1"
        );

        // Verify Timestamp: 100ms at 90kHz = 9000 ticks
        let ts_diff = out2.rtp_ts.numer().wrapping_sub(out1.rtp_ts.numer());
        assert_eq!(ts_diff, 9000, "Timestamp should correspond to 100ms delta");
    }

    #[test]
    fn test_switching_streams() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new(Frequency::NINETY_KHZ);

        // --- Stream A (Seq 1000-1001) ---
        let p_a1 = RtpPacket {
            seq_no: 1000.into(),
            playout_time: start_time,
            is_keyframe_start: true,
            ..Default::default()
        };

        timeline.rebase(&p_a1);
        let out_a1 = timeline.rewrite(p_a1);

        let p_a2 = RtpPacket {
            seq_no: 1001.into(),
            playout_time: start_time + Duration::from_millis(33),
            is_keyframe_start: false,
            ..Default::default()
        };
        let out_a2 = timeline.rewrite(p_a2);

        // --- Switch to Stream B (Seq 5000) ---
        // Stream B arrives 100ms after start, starting at random Seq 5000
        let p_b1 = RtpPacket {
            seq_no: 5000.into(),
            playout_time: start_time + Duration::from_millis(100),
            is_keyframe_start: true,
            ..Default::default()
        };

        timeline.rebase(&p_b1); // Rebase aligns Stream B to follow Stream A
        let out_b1 = timeline.rewrite(p_b1);

        // Verify Continuity across switch
        assert_eq!(
            *out_b1.seq_no,
            (*out_a2.seq_no).wrapping_add(1),
            "Output sequence must be continuous across switch"
        );

        // Verify Timestamp linearity
        // A1=0ms, B1=100ms. Diff should be 9000 ticks.
        // The timeline calculates B1 based on (B1.time - anchor), where anchor is A1.time
        let ts_diff = out_b1.rtp_ts.numer().wrapping_sub(out_a1.rtp_ts.numer());
        assert_eq!(ts_diff, 9000, "Timestamp should be linear across switch");
    }

    #[test]
    fn test_sequence_wrapping_u64() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new(Frequency::NINETY_KHZ);

        // Start normally
        let p1 = RtpPacket {
            seq_no: 10.into(),
            playout_time: start_time,
            is_keyframe_start: true,
            ..Default::default()
        };
        timeline.rebase(&p1);
        let out1 = timeline.rewrite(p1);

        // Manually force the internal highest_seq_no to u64 boundary - 1
        // We can't modify private state directly, so we simulate a stream
        // that pushes the timeline to the edge.
        // Or, we rely on the fact that `rebase` calculates offset based on wrapping arithmetic.

        // Let's simulate a switch to a stream where the calculation wraps.
        // Current Out: X.
        // We want next Out: X+1.
        // Input is Y.
        // Offset = (X+1) - Y.

        // We simply verify strict +1 increments even if input jumps largely
        let p_next = RtpPacket {
            seq_no: u64::MAX.into(), // Input at boundary
            playout_time: start_time + Duration::from_millis(33),
            is_keyframe_start: true,
            ..Default::default()
        };

        // Switch to high sequence number
        timeline.rebase(&p_next);
        let out_next = timeline.rewrite(p_next);

        assert_eq!(*out_next.seq_no, (*out1.seq_no).wrapping_add(1));

        // Next packet wraps the input
        let p_wrap = RtpPacket {
            seq_no: 0.into(),
            playout_time: start_time + Duration::from_millis(66),
            is_keyframe_start: false,
            ..Default::default()
        };

        // Normal rewrite (no rebase needed for contiguous input)
        // Input: u64::MAX -> 0 (wrapped)
        // Output: X+1 -> X+2
        let out_wrap = timeline.rewrite(p_wrap);
        assert_eq!(*out_wrap.seq_no, (*out_next.seq_no).wrapping_add(1));
    }

    #[test]
    #[should_panic(expected = "rebase must have occured before rewriting")]
    fn test_panic_if_no_rebase() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new(Frequency::NINETY_KHZ);

        // Packet without rebase
        let p1 = RtpPacket {
            seq_no: 100.into(),
            playout_time: start_time,
            is_keyframe_start: true,
            ..Default::default()
        };

        // This should panic because anchor is None
        timeline.rewrite(p1);
    }

    #[test]
    fn test_late_packet_ordering() {
        // Ensures that if a packet arrives late (playout time < previous),
        // the timestamp is calculated correctly relative to anchor.
        let start_time = Instant::now();
        let mut timeline = Timeline::new(Frequency::NINETY_KHZ);

        // Packet 1 (Base)
        let p1 = RtpPacket {
            seq_no: 10.into(),
            playout_time: start_time + Duration::from_millis(100),
            is_keyframe_start: true,
            ..Default::default()
        };
        timeline.rebase(&p1); // Sets anchor at T+100ms
        let _out1 = timeline.rewrite(p1);

        // Packet 2 (Arrives with earlier playout time due to reordering/jitter)
        // T+50ms (Before anchor)
        // Since `saturating_duration_since` is used, this should clamp to 0
        // or handle gracefully depending on logic.
        // The current logic: playout.saturating_duration_since(anchor)
        // If playout < anchor, result is 0.
        let p2 = RtpPacket {
            seq_no: 11.into(),
            playout_time: start_time + Duration::from_millis(50),
            is_keyframe_start: false,
            ..Default::default()
        };

        let out2 = timeline.rewrite(p2);

        // Anchor is at T+100. P2 is at T+50.
        // saturating_duration_since(100) returns 0.
        assert_eq!(out2.rtp_ts.numer(), 0);
    }
}
