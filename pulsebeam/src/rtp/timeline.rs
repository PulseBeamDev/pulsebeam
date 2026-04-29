use crate::rtp::RtpPacket;
use pulsebeam_runtime::rand::{self, RngCore};
use str0m::{
    media::{Frequency, MediaTime},
    rtp::SeqNo,
};
use tokio::time::Instant;

// Timeline allows switching between input streams that outputs a stream with a
// continuous seq_no and playout_time as if it comes from a single stream.
//
// Sequence rewriting:
//
//   output = (input_seq + base) mod 2^16
//
// `base` is adjusted on every stream switch (rebase) to make the new stream's
// first packet follow the previous stream's last output.  `drop_count` decrements
// `base` by the number of upstream-filtered packets between consecutive forwarded
// ones, closing those gaps in the output seq space without needing to see the
// dropped packets here.
#[derive(Debug)]
pub struct Timeline {
    clock_rate: Frequency,
    /// The last output seq_no actually written (used to compute `base` on rebase).
    max_output: u16,
    /// Additive offset: output = (input + base) & 0xFFFF.
    base: u16,
    /// Whether any packet has been forwarded yet.
    started: bool,
    anchor: Option<Instant>,
}

impl Timeline {
    /// Create a new timeline that starts output sequence numbers at `base_seq_no`.
    pub fn new_with_base(clock_rate: Frequency, base_seq_no: u16) -> Self {
        Self {
            clock_rate,
            max_output: base_seq_no,
            base: 0,
            started: false,
            anchor: None,
        }
    }

    /// Create a new timeline using a pseudo-random base sequence number.
    pub fn new(clock_rate: Frequency) -> Self {
        let mut rng = rand::os_rng();
        Self::new_with_rng(clock_rate, &mut rng)
    }

    /// Create a new timeline by drawing the base sequence number from the given RNG.
    pub fn new_with_rng<R: RngCore>(clock_rate: Frequency, rng: &mut R) -> Self {
        let base_seq_no = (rng.next_u32() & 0xFFFF) as u16;
        Self::new_with_base(clock_rate, base_seq_no)
    }

    /// Re-aligns the timeline to a new video stream starting with `packet`.
    /// `packet` must be a keyframe (start of a new GOP).
    pub fn rebase(&mut self, packet: &RtpPacket) {
        debug_assert!(
            packet.is_keyframe_start,
            "Rebase should only happen on keyframe boundaries"
        );
        self.rebase_inner(packet);
    }

    /// Re-aligns the timeline to a new audio stream.
    ///
    /// Identical to `rebase` but without the keyframe assertion.
    pub fn rebase_audio(&mut self, packet: &RtpPacket) {
        self.rebase_inner(packet);
    }

    fn rebase_inner(&mut self, packet: &RtpPacket) {
        let input_seq = *packet.seq_no as u16;
        // Make the first output from the new stream follow max_output.
        // output = (input + base) mod 2^16 = max_output + 1
        // => base = (max_output + 1 - input) mod 2^16
        self.base = self.max_output.wrapping_add(1).wrapping_sub(input_seq);
        self.started = false;

        if self.anchor.is_none() {
            self.anchor = Some(packet.playout_time);
        }
    }

    /// Adjust `base` to account for `n` upstream-filtered packets that will
    /// never arrive here.  Call this before `rewrite` for the packet immediately
    /// following the filtered run.
    pub fn drop_count(&mut self, n: u16) {
        self.base = self.base.wrapping_sub(n);
    }

    pub fn rewrite(&mut self, pkt: &mut RtpPacket) {
        let input_seq = *pkt.seq_no as u16;
        let output_seq = input_seq.wrapping_add(self.base);
        pkt.seq_no = SeqNo::from(output_seq as u64);

        if !self.started || wrapping_gt(output_seq, self.max_output) {
            self.max_output = output_seq;
        }
        self.started = true;

        let anchor = self
            .anchor
            .expect("rebase must have occured before rewriting to create the first anchor");

        let duration = pkt.playout_time.saturating_duration_since(anchor);
        pkt.rtp_ts = MediaTime::from(duration).rebase(self.clock_rate);
    }
}

/// Returns true if `a` is strictly after `b` in the wrapping u16 seq space.
#[inline]
fn wrapping_gt(a: u16, b: u16) -> bool {
    a.wrapping_sub(b) < 0x8000 && a != b
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_simple_continuity() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);

        // Packet 1: First packet (Keyframe) - requires rebase
        let mut p1 = RtpPacket {
            seq_no: 100.into(),
            playout_time: start_time,
            is_keyframe_start: true,
            ..Default::default()
        };

        timeline.rebase(&p1);
        timeline.rewrite(&mut p1);
        let base_seq = *p1.seq_no;

        // Packet 2: Regular packet, 100ms later
        let mut p2 = RtpPacket {
            seq_no: 101.into(),
            playout_time: start_time + Duration::from_millis(100),
            is_keyframe_start: false,
            ..Default::default()
        };

        timeline.rewrite(&mut p2);

        // Verify Sequence Continuity
        assert_eq!(
            *p2.seq_no,
            base_seq + 1,
            "Sequence number should increment by 1"
        );

        // Verify Timestamp: 100ms at 90kHz = 9000 ticks
        let ts_diff = p2.rtp_ts.numer().wrapping_sub(p1.rtp_ts.numer());
        assert_eq!(ts_diff, 9000, "Timestamp should correspond to 100ms delta");
    }

    #[test]
    fn test_switching_streams() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);

        // --- Stream A (Seq 1000-1001) ---
        let mut p_a1 = RtpPacket {
            seq_no: 1000.into(),
            playout_time: start_time,
            is_keyframe_start: true,
            ..Default::default()
        };

        timeline.rebase(&p_a1);
        timeline.rewrite(&mut p_a1);

        let mut p_a2 = RtpPacket {
            seq_no: 1001.into(),
            playout_time: start_time + Duration::from_millis(33),
            is_keyframe_start: false,
            ..Default::default()
        };
        timeline.rewrite(&mut p_a2);

        // --- Switch to Stream B (Seq 5000) ---
        // Stream B arrives 100ms after start, starting at random Seq 5000
        let mut p_b1 = RtpPacket {
            seq_no: 5000.into(),
            playout_time: start_time + Duration::from_millis(100),
            is_keyframe_start: true,
            ..Default::default()
        };

        timeline.rebase(&p_b1); // Rebase aligns Stream B to follow Stream A
        timeline.rewrite(&mut p_b1);

        // Verify Continuity across switch
        assert_eq!(
            *p_b1.seq_no,
            (*p_a2.seq_no).wrapping_add(1),
            "Output sequence must be continuous across switch"
        );

        // Verify Timestamp linearity
        // A1=0ms, B1=100ms. Diff should be 9000 ticks.
        // The timeline calculates B1 based on (B1.time - anchor), where anchor is A1.time
        let ts_diff = p_b1.rtp_ts.numer().wrapping_sub(p_a1.rtp_ts.numer());
        assert_eq!(ts_diff, 9000, "Timestamp should be linear across switch");
    }

    #[test]
    fn test_sequence_wrapping_u64() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new(Frequency::NINETY_KHZ);

        // Start normally
        let mut p1 = RtpPacket {
            seq_no: 10.into(),
            playout_time: start_time,
            is_keyframe_start: true,
            ..Default::default()
        };
        timeline.rebase(&p1);
        timeline.rewrite(&mut p1);

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
        let mut p_next = RtpPacket {
            seq_no: u64::MAX.into(), // Input at boundary
            playout_time: start_time + Duration::from_millis(33),
            is_keyframe_start: true,
            ..Default::default()
        };

        // Switch to high sequence number
        timeline.rebase(&p_next);
        timeline.rewrite(&mut p_next);

        assert_eq!(*p_next.seq_no, (*p1.seq_no).wrapping_add(1));

        // Next packet wraps the input
        let mut p_wrap = RtpPacket {
            seq_no: 0.into(),
            playout_time: start_time + Duration::from_millis(66),
            is_keyframe_start: false,
            ..Default::default()
        };

        // Normal rewrite (no rebase needed for contiguous input)
        // Input: u64::MAX -> 0 (wrapped)
        // Output: X+1 -> X+2
        timeline.rewrite(&mut p_wrap);
        assert_eq!(*p_wrap.seq_no, (*p_next.seq_no).wrapping_add(1));
    }

    #[test]
    #[should_panic(expected = "rebase must have occured before rewriting")]
    fn test_panic_if_no_rebase() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);

        // Packet without rebase
        let mut p1 = RtpPacket {
            seq_no: 100.into(),
            playout_time: start_time,
            is_keyframe_start: true,
            ..Default::default()
        };

        // This should panic because anchor is None
        timeline.rewrite(&mut p1);
    }

    #[test]
    fn test_late_packet_ordering() {
        // Ensures that if a packet arrives late (playout time < previous),
        // the timestamp is calculated correctly relative to anchor.
        let start_time = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);

        // Packet 1 (Base)
        let mut p1 = RtpPacket {
            seq_no: 10.into(),
            playout_time: start_time + Duration::from_millis(100),
            is_keyframe_start: true,
            ..Default::default()
        };
        timeline.rebase(&p1); // Sets anchor at T+100ms
        timeline.rewrite(&mut p1);

        // Packet 2 (Arrives with earlier playout time due to reordering/jitter)
        // T+50ms (Before anchor)
        // Since `saturating_duration_since` is used, this should clamp to 0
        // or handle gracefully depending on logic.
        // The current logic: playout.saturating_duration_since(anchor)
        // If playout < anchor, result is 0.
        let mut p2 = RtpPacket {
            seq_no: 11.into(),
            playout_time: start_time + Duration::from_millis(50),
            is_keyframe_start: false,
            ..Default::default()
        };

        timeline.rewrite(&mut p2);

        // Anchor is at T+100. P2 is at T+50.
        // saturating_duration_since(100) returns 0.
        assert_eq!(p2.rtp_ts.numer(), 0);
    }
}
