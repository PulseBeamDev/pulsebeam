use crate::rtp::RtpPacket;
use pulsebeam_runtime::rand::RngCore;
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
    max_output: SeqNo,
    /// Additive offset: output = input + base.
    seq_base: SeqNo,
    /// Whether any packet has been forwarded yet.
    started: bool,
    /// The last output rtp_ts written (used to compute `ts_base` on rebase).
    max_output_ts: u64,
    /// Additive offset for RTP timestamps.
    ts_base: u64,
    /// The playout time of the last output packet.
    last_playout_time: Option<Instant>,
}

impl Timeline {
    /// Create a new timeline that starts output sequence numbers at `base_seq_no`.
    pub fn new_with_base(clock_rate: Frequency, base_seq_no: u16) -> Self {
        Self {
            clock_rate,
            max_output: SeqNo::from(base_seq_no as u64),
            seq_base: SeqNo::default(),
            started: false,
            max_output_ts: 0,
            ts_base: 0,
            last_playout_time: None,
        }
    }

    /// Create a new timeline whose starting sequence number is drawn from `rng`.
    pub fn new<R: RngCore>(clock_rate: Frequency, rng: &mut R) -> Self {
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
        let input_seq = *packet.seq_no;
        // Make the first output from the new stream follow max_output.
        self.seq_base = self
            .max_output
            .wrapping_add(1)
            .wrapping_sub(input_seq)
            .into();
        self.started = false;

        let input_ts = packet.rtp_ts.numer();

        if let Some(last_playout) = self.last_playout_time {
            let time_delta = packet.playout_time.saturating_duration_since(last_playout);
            let ts_delta = (time_delta.as_secs_f64() * (self.clock_rate.get() as f64)) as u64;
            let expected_ts = self.max_output_ts.wrapping_add(ts_delta);
            self.ts_base = expected_ts.wrapping_sub(input_ts);
        } else {
            // Preserve the original timestamp offset on the first packet.
            self.ts_base = 0;
        }
    }

    /// Adjust `base` to account for `n` upstream-filtered packets that will
    /// never arrive here.  Call this before `rewrite` for the packet immediately
    /// following the filtered run.
    pub fn drop_count(&mut self, n: u16) {
        self.seq_base = self.seq_base.wrapping_sub(n as u64).into();
    }

    pub fn rewrite(&mut self, pkt: &mut RtpPacket) {
        let input_seq = *pkt.seq_no;
        let output_seq = input_seq.wrapping_add(*self.seq_base).into();
        pkt.seq_no = output_seq;

        if !self.started || output_seq > self.max_output {
            self.max_output = output_seq;
        }
        self.started = true;

        let input_ts = pkt.rtp_ts.numer();
        let output_ts = input_ts.wrapping_add(self.ts_base);
        pkt.rtp_ts = MediaTime::new(output_ts, self.clock_rate);

        self.max_output_ts = output_ts;
        self.last_playout_time = Some(pkt.playout_time);
    }
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
            rtp_ts: MediaTime::new(10000, Frequency::NINETY_KHZ),
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
            rtp_ts: MediaTime::new(19000, Frequency::NINETY_KHZ),
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
            rtp_ts: MediaTime::new(10000, Frequency::NINETY_KHZ),
            playout_time: start_time,
            is_keyframe_start: true,
            ..Default::default()
        };

        timeline.rebase(&p_a1);
        timeline.rewrite(&mut p_a1);

        let mut p_a2 = RtpPacket {
            seq_no: 1001.into(),
            rtp_ts: MediaTime::new(13000, Frequency::NINETY_KHZ),
            playout_time: start_time + Duration::from_millis(33),
            is_keyframe_start: false,
            ..Default::default()
        };
        timeline.rewrite(&mut p_a2);

        // --- Switch to Stream B (Seq 5000) ---
        // Stream B arrives 100ms after start, starting at random Seq 5000
        let mut p_b1 = RtpPacket {
            seq_no: 5000.into(),
            rtp_ts: MediaTime::new(80000, Frequency::NINETY_KHZ), // Random initial timestamp
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
        // A2 was at 33ms. B1 is at 100ms. Time delta is 67ms.
        // 67ms at 90kHz = 6030 ticks.
        // So B1 should be A2 + 6030 ticks.
        let ts_diff = p_b1.rtp_ts.numer().wrapping_sub(p_a2.rtp_ts.numer());
        assert_eq!(ts_diff, 6030, "Timestamp should be linear across switch");
    }

    #[test]
    fn test_sequence_wrapping_u64() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new(
            Frequency::NINETY_KHZ,
            &mut pulsebeam_runtime::rand::seeded_rng(42),
        );

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

    // removed test test_panic_if_no_rebase

    #[test]
    fn test_late_packet_ordering() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);

        let mut p1 = RtpPacket {
            seq_no: 10.into(),
            rtp_ts: MediaTime::new(1000, Frequency::NINETY_KHZ),
            playout_time: start_time + Duration::from_millis(100),
            is_keyframe_start: true,
            ..Default::default()
        };
        timeline.rebase(&p1);
        timeline.rewrite(&mut p1);

        let mut p2 = RtpPacket {
            seq_no: 11.into(),
            rtp_ts: MediaTime::new(1500, Frequency::NINETY_KHZ),
            playout_time: start_time + Duration::from_millis(50),
            is_keyframe_start: false,
            ..Default::default()
        };

        timeline.rewrite(&mut p2);

        // rtp_ts should just be linearly transformed, preserving input timing
        assert_eq!(p2.rtp_ts.numer().wrapping_sub(p1.rtp_ts.numer()), 500);
    }
}
