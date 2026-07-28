use crate::rtp::RtpPacket;
use pulsebeam_runtime::rand::RngCore;
use std::time::Duration;
use str0m::{
    media::{Frequency, MediaTime},
    rtp::SeqNo,
};
use tokio::time::Instant;

// Timeline maps a succession of input streams onto one output stream that looks
// to the subscriber like it came from a single sender.
//
//   output_seq = input_seq + seq_base
//   output_ts  = input_ts  + ts_base
//
// Both bases are recomputed on every switch (`rebase`).  Sequence numbers are
// offset so the new stream continues where the old one stopped; gaps inside a
// stream are preserved so upstream loss stays visible to the subscriber.
//
// Timestamps are anchored to a fixed epoch rather than chained off the previous
// stream's last timestamp.  Chaining lets every switch add whatever skew that
// switch introduced, and the error accumulates over a call; anchoring makes each
// rebase an absolute correction against real elapsed time instead.
pub struct Timeline {
    clock_rate: Frequency,
    /// Highest output seq_no written so far.
    max_output: SeqNo,
    /// Additive offset: output = input + seq_base.
    seq_base: SeqNo,
    /// Highest output rtp_ts written so far.
    max_output_ts: u64,
    /// Additive offset for RTP timestamps.
    ts_base: u64,
    /// (wall time, output rtp_ts) of the first packet ever forwarded. Every
    /// rebase re-anchors against this so switches do not accumulate skew.
    epoch: Option<(Instant, u64)>,
}

impl std::fmt::Debug for Timeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Timeline")
            .field("max_output", &*self.max_output)
            .field("max_output_ts", &self.max_output_ts)
            .finish_non_exhaustive()
    }
}

impl Timeline {
    /// Create a new timeline that starts output sequence numbers at `base_seq_no`.
    pub fn new_with_base(clock_rate: Frequency, base_seq_no: u16) -> Self {
        Self {
            clock_rate,
            max_output: SeqNo::from(base_seq_no as u64),
            seq_base: SeqNo::default(),
            max_output_ts: 0,
            ts_base: 0,
            epoch: None,
        }
    }

    /// Create a new timeline whose starting sequence number is drawn from `rng`.
    pub fn new<R: RngCore>(clock_rate: Frequency, rng: &mut R) -> Self {
        let base_seq_no = (rng.next_u32() & 0xFFFF) as u16;
        Self::new_with_base(clock_rate, base_seq_no)
    }

    /// Smallest RTP-timestamp advance a switch may make.
    ///
    /// The frame that starts the new stream must never share a timestamp with
    /// the frame that ended the old one, or the subscriber's decoder treats the
    /// two as one frame. One 60fps frame interval is the smallest gap that also
    /// reads as a plausible frame duration.
    #[inline]
    fn min_switch_advance(&self) -> u64 {
        (self.clock_rate.get() as u64 / 60).max(1)
    }

    #[inline]
    fn ticks(&self, d: Duration) -> u64 {
        ((d.as_nanos() * self.clock_rate.get() as u128) / 1_000_000_000u128) as u64
    }

    /// Re-aligns the timeline to a new source stream starting with `packet`.
    pub fn rebase(&mut self, packet: &RtpPacket) {
        self.rebase_inner(packet);
    }

    /// Re-aligns the timeline to a new audio stream.
    pub fn rebase_audio(&mut self, packet: &RtpPacket) {
        self.rebase_inner(packet);
    }

    fn rebase_inner(&mut self, packet: &RtpPacket) {
        let input_seq = *packet.seq_no;
        self.seq_base = self
            .max_output
            .wrapping_add(1)
            .wrapping_sub(input_seq)
            .into();

        let input_ts = packet.rtp_ts.numer();
        self.ts_base = match self.epoch {
            // Nothing forwarded yet: keep the source's own timestamps.
            None => 0,
            Some((epoch_at, epoch_ts)) => {
                let elapsed = packet.playout_time.saturating_duration_since(epoch_at);
                let target = epoch_ts.wrapping_add(self.ticks(elapsed));
                let floor = self.max_output_ts.wrapping_add(self.min_switch_advance());
                let target = if target < floor { floor } else { target };
                debug_assert!(
                    target > self.max_output_ts,
                    "rebase must move the output clock forward"
                );
                target.wrapping_sub(input_ts)
            }
        };
    }

    /// Rewrite `pkt` preserving its position relative to its own stream, so gaps
    /// left by upstream loss stay visible to the subscriber.
    pub fn rewrite(&mut self, pkt: &mut RtpPacket) {
        let output_seq: SeqNo = (*pkt.seq_no).wrapping_add(*self.seq_base).into();
        let output_ts = pkt.rtp_ts.numer().wrapping_add(self.ts_base);
        self.apply(pkt, output_seq, output_ts);
    }

    /// Rewrite `pkt` onto the next free output sequence number.
    ///
    /// Used for a replayed switch burst, which the cache has already ordered and
    /// deduplicated and which may carry synthesized parameter-set packets whose
    /// original sequence numbers are unrelated to the segment.
    pub fn rewrite_sequential(&mut self, pkt: &mut RtpPacket) {
        let output_seq: SeqNo = self.max_output.wrapping_add(1).into();
        let output_ts = pkt.rtp_ts.numer().wrapping_add(self.ts_base);
        self.apply(pkt, output_seq, output_ts);
    }

    /// After a burst rewritten with `rewrite_sequential`, realign the sequence
    /// offset so the stream's next live packet (`last_input_seq + 1`) continues
    /// from where the burst stopped.
    pub fn resync_to_input(&mut self, last_input_seq: SeqNo) {
        self.seq_base = (*self.max_output).wrapping_sub(*last_input_seq).into();
    }

    fn apply(&mut self, pkt: &mut RtpPacket, output_seq: SeqNo, output_ts: u64) {
        pkt.seq_no = output_seq;
        pkt.rtp_ts = MediaTime::new(output_ts, self.clock_rate);

        if self.epoch.is_none() {
            self.epoch = Some((pkt.playout_time, output_ts));
            self.max_output = output_seq;
            self.max_output_ts = output_ts;
            return;
        }

        if output_seq > self.max_output {
            self.max_output = output_seq;
        }
        if output_ts > self.max_output_ts {
            self.max_output_ts = output_ts;
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp::test_utils::{H264StreamBuilder, ParameterSetStyle};

    fn pkt(seq: u64, ts: u64, at: Instant) -> RtpPacket {
        RtpPacket {
            seq_no: seq.into(),
            rtp_ts: MediaTime::new(ts, Frequency::NINETY_KHZ),
            playout_time: at,
            arrival_ts: at,
            ..Default::default()
        }
    }

    #[test]
    fn sequence_and_timestamps_are_continuous_within_a_stream() {
        let t0 = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);

        let mut p1 = pkt(100, 10_000, t0);
        timeline.rebase(&p1);
        timeline.rewrite(&mut p1);

        let mut p2 = pkt(101, 19_000, t0 + Duration::from_millis(100));
        timeline.rewrite(&mut p2);

        assert_eq!(*p2.seq_no, *p1.seq_no + 1);
        assert_eq!(p2.rtp_ts.numer() - p1.rtp_ts.numer(), 9000);
    }

    #[test]
    fn upstream_loss_stays_visible_as_a_sequence_gap() {
        let t0 = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);

        let mut p1 = pkt(100, 10_000, t0);
        timeline.rebase(&p1);
        timeline.rewrite(&mut p1);

        // Packets 101..=104 were lost upstream.
        let mut p2 = pkt(105, 25_000, t0 + Duration::from_millis(160));
        timeline.rewrite(&mut p2);

        assert_eq!(
            *p2.seq_no - *p1.seq_no,
            5,
            "the subscriber must be able to see that 4 packets were lost"
        );
    }

    #[test]
    fn a_switch_is_seamless_in_sequence_and_forward_in_time() {
        let t0 = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);

        let mut a1 = pkt(1000, 10_000, t0);
        timeline.rebase(&a1);
        timeline.rewrite(&mut a1);
        let mut a2 = pkt(1001, 13_000, t0 + Duration::from_millis(33));
        timeline.rewrite(&mut a2);

        // Stream B has completely unrelated sequence and timestamp bases.
        let mut b1 = pkt(5000, 80_000, t0 + Duration::from_millis(100));
        timeline.rebase(&b1);
        timeline.rewrite(&mut b1);

        assert_eq!(*b1.seq_no, (*a2.seq_no).wrapping_add(1));
        assert!(b1.rtp_ts.numer() > a2.rtp_ts.numer());
        assert_eq!(b1.rtp_ts.numer() - a1.rtp_ts.numer(), 9000);
    }

    #[test]
    fn a_switch_to_an_older_source_still_moves_the_clock_forward() {
        let t0 = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);

        let mut a1 = pkt(10, 10_000, t0 + Duration::from_millis(500));
        timeline.rebase(&a1);
        timeline.rewrite(&mut a1);

        // A replayed segment whose first packet is older than what we just sent.
        let mut b1 = pkt(900, 4_000, t0 + Duration::from_millis(300));
        timeline.rebase(&b1);
        timeline.rewrite(&mut b1);

        assert!(
            b1.rtp_ts.numer() > a1.rtp_ts.numer(),
            "two frames must never share a timestamp: {} vs {}",
            a1.rtp_ts.numer(),
            b1.rtp_ts.numer()
        );
    }

    #[test]
    fn repeated_switches_do_not_accumulate_clock_skew() {
        let t0 = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);
        let mut first_out = None;
        let mut last: Option<RtpPacket> = None;

        // Alternate between two sources every 10 frames for 100 switches, each
        // source keeping its own unrelated timestamp base.
        for switch in 0..100u64 {
            let source_base = if switch % 2 == 0 { 7_000_000 } else { 250 };
            for frame in 0..10u64 {
                let elapsed = Duration::from_millis((switch * 10 + frame) * 33);
                let input_ts = source_base + (switch * 10 + frame) * 2970;
                let mut p = pkt(switch * 1000 + frame, input_ts, t0 + elapsed);
                if frame == 0 {
                    timeline.rebase(&p);
                }
                timeline.rewrite(&mut p);
                if first_out.is_none() {
                    first_out = Some(p.clone());
                }
                if let Some(prev) = &last {
                    assert!(
                        p.rtp_ts.numer() > prev.rtp_ts.numer(),
                        "output clock went backwards at switch {switch} frame {frame}"
                    );
                }
                last = Some(p);
            }
        }

        let first = first_out.unwrap();
        let last = last.unwrap();
        let ts_elapsed = last.rtp_ts.numer() - first.rtp_ts.numer();
        let wall_ticks = (last
            .playout_time
            .duration_since(first.playout_time)
            .as_secs_f64()
            * 90_000.0) as u64;
        let skew_ms = ts_elapsed.abs_diff(wall_ticks) as f64 / 90.0;
        assert!(
            skew_ms < 50.0,
            "output clock drifted {skew_ms:.1}ms from real time over 100 switches"
        );
    }

    #[test]
    fn sequential_rewrite_then_resync_continues_the_live_stream() {
        let t0 = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);

        let mut a = pkt(10, 1_000, t0);
        timeline.rebase(&a);
        timeline.rewrite(&mut a);

        // A replayed burst with unrelated, non-contiguous input sequence numbers.
        let mut burst_out = Vec::new();
        for (i, input_seq) in [990u64, 991, 995, 996].into_iter().enumerate() {
            let mut p = pkt(
                input_seq,
                50_000 + i as u64 * 3000,
                t0 + Duration::from_millis(10),
            );
            if i == 0 {
                timeline.rebase(&p);
            }
            timeline.rewrite_sequential(&mut p);
            burst_out.push(p);
        }

        assert!(
            burst_out
                .windows(2)
                .all(|w| *w[1].seq_no == *w[0].seq_no + 1),
            "a replayed burst is contiguous by construction"
        );
        assert_eq!(*burst_out[0].seq_no, *a.seq_no + 1);

        timeline.resync_to_input(996.into());
        let mut live = pkt(997, 62_000, t0 + Duration::from_millis(43));
        timeline.rewrite(&mut live);
        assert_eq!(*live.seq_no, *burst_out.last().unwrap().seq_no + 1);

        // And loss after the burst still shows up as a gap.
        let mut after_loss = pkt(1000, 71_000, t0 + Duration::from_millis(76));
        timeline.rewrite(&mut after_loss);
        assert_eq!(*after_loss.seq_no - *live.seq_no, 3);
    }

    #[test]
    fn reordered_input_is_passed_through_without_reordering_the_output() {
        let t0 = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);

        let mut p1 = pkt(10, 1_000, t0);
        timeline.rebase(&p1);
        timeline.rewrite(&mut p1);

        let mut p3 = pkt(12, 7_000, t0 + Duration::from_millis(66));
        timeline.rewrite(&mut p3);
        let mut p2 = pkt(11, 4_000, t0 + Duration::from_millis(33));
        timeline.rewrite(&mut p2);

        assert_eq!(*p2.seq_no + 1, *p3.seq_no);
        assert!(p2.rtp_ts.numer() < p3.rtp_ts.numer());

        // A late packet must not drag the switch anchor backwards.
        let mut next = pkt(13, 10_000, t0 + Duration::from_millis(99));
        timeline.rebase(&next);
        timeline.rewrite(&mut next);
        assert_eq!(*next.seq_no, *p3.seq_no + 1);
    }

    #[test]
    fn real_h264_streams_switch_without_a_timestamp_collision() {
        let t0 = Instant::now();
        let mut a = H264StreamBuilder::new(1, 100, 90_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);
        let mut b = H264StreamBuilder::new(2, 60_000, 5_000_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0);

        let mut out = Vec::new();
        let mut first = true;
        for p in a.keyframe(3).into_iter().chain(a.delta_frames(10, 3)) {
            let mut p = p;
            if first {
                timeline.rebase(&p);
                first = false;
            }
            timeline.rewrite(&mut p);
            out.push(p);
        }
        let _ = b.delta_frames(11, 2);
        let mut first = true;
        for p in b.keyframe(2).into_iter().chain(b.delta_frames(5, 2)) {
            let mut p = p;
            if first {
                timeline.rebase(&p);
                first = false;
            }
            timeline.rewrite(&mut p);
            out.push(p);
        }

        crate::rtp::conformance::assert_decodable(&out, "timeline-only switch");
    }
}
