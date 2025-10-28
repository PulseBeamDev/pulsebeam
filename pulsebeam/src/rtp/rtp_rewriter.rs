use crate::rtp::PacketTiming;
use str0m::media::MediaTime;
use str0m::rtp::SeqNo;

/// Manages the state for rewriting RTP headers to create a single, clean, and
/// contiguous stream for a subscriber.
///
/// This rewriter operates under the following assumptions:
/// 1. Packets from a single stream arrive in-order (thanks to an upstream jitter buffer).
/// 2. Layer switches are explicitly signaled via the `is_switch` flag.
/// 3. During a switch, it prioritizes the new stream, dropping any lingering packets
///    from the old one.
#[derive(Debug)]
pub struct RtpRewriter {
    /// The offset applied to incoming sequence numbers to maintain continuity.
    seq_no_offset: u64,
    /// The offset applied to incoming timestamps to maintain correct timing.
    timestamp_offset: u64,

    /// The highest sequence number that has been forwarded to the subscriber.
    highest_forwarded_seq: SeqNo,
    /// The rewritten timestamp of the packet with `highest_forwarded_seq`.
    highest_forwarded_ts: MediaTime,
    initialized: bool,
}

impl Default for RtpRewriter {
    fn default() -> Self {
        Self {
            seq_no_offset: 0,
            timestamp_offset: 0,
            highest_forwarded_seq: SeqNo::new(),
            highest_forwarded_ts: MediaTime::from_90khz(0),
            initialized: false,
        }
    }
}

impl RtpRewriter {
    /// Creates a new `RtpRewriter` with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rewrites the sequence number and timestamp of an RTP packet.
    ///
    /// It returns `Some((SeqNo, MediaTime))` with the new values if the packet should be
    /// forwarded, or `None` if the packet should be dropped (e.g., it's a
    /// lingering packet from an old stream after a switch).
    pub fn rewrite(
        &mut self,
        packet: &impl PacketTiming,
        is_switch: bool,
    ) -> Option<(SeqNo, MediaTime)> {
        if is_switch {
            self.reset(packet);
        };

        let next_seq = SeqNo::from(packet.seq_no().wrapping_add(self.seq_no_offset));

        let ts = packet.rtp_timestamp();
        let next_ts = MediaTime::new(
            ts.numer().wrapping_add(self.timestamp_offset),
            ts.frequency(),
        );

        if self.initialized {
            if next_seq < self.highest_forwarded_seq || next_ts < self.highest_forwarded_ts {
                tracing::warn!("unexpected out-of-order, dropping packets");
                return None;
            }

            if next_seq == self.highest_forwarded_seq {
                tracing::warn!("unexpected duplication, dropping packets");
                return None;
            }
        }
        self.highest_forwarded_seq = next_seq;
        self.highest_forwarded_ts = next_ts;
        self.initialized = true;
        tracing::debug!("next_seq={next_seq:?},next_ts={next_ts:?}");
        Some((next_seq, next_ts))
    }

    /// Resets the rewriter's state to seamlessly transition to a new stream.
    fn reset(&mut self, packet: &impl PacketTiming) {
        let target_seq = self.highest_forwarded_seq.wrapping_add(1);
        self.seq_no_offset = target_seq.wrapping_sub(*packet.seq_no());

        let rtp_timestamp = packet.rtp_timestamp();
        self.highest_forwarded_ts = self.highest_forwarded_ts.rebase(rtp_timestamp.frequency());
        let target_ts = self.highest_forwarded_ts.numer();
        self.timestamp_offset = target_ts.wrapping_sub(rtp_timestamp.numer());

        tracing::info!(
            incoming_seq = *packet.seq_no(),
            target_seq = target_seq,
            seq_offset = self.seq_no_offset,
            ts_offset = self.timestamp_offset,
            "RTP rewriter reset for new stream"
        );
    }
}

#[cfg(test)]
mod test {
    use str0m::{media::MediaTime, rtp::SeqNo};

    use super::*;
    use crate::rtp::TimingHeader;

    #[test]
    fn test_continuous_stream_after_switch() {
        let mut rewriter = RtpRewriter::new();
        let p1 = TimingHeader::new(SeqNo::from(300), MediaTime::from_90khz(3_000));
        assert_eq!(
            rewriter.rewrite(&p1, false).unwrap(),
            (SeqNo::from(300), MediaTime::from_90khz(3_000))
        );

        let p2 = TimingHeader::new(SeqNo::from(1001), MediaTime::from_90khz(90_000));
        assert_eq!(
            rewriter.rewrite(&p2, true).unwrap(),
            (SeqNo::from(301), MediaTime::from_90khz(3_000))
        );

        let p3 = TimingHeader::new(SeqNo::from(1002), MediaTime::from_90khz(93_000));
        assert_eq!(
            rewriter.rewrite(&p3, false).unwrap(),
            (SeqNo::from(302), MediaTime::from_90khz(6_000))
        );
    }

    #[test]
    fn test_sequence_continuity_across_layer_switch() {
        let mut r = RtpRewriter::new();

        // Layer A (base)
        let p1 = TimingHeader::new(SeqNo::from(300), MediaTime::from_90khz(3_000));
        let out1 = r.rewrite(&p1, false).unwrap();
        assert_eq!(out1.0, SeqNo::from(300));

        let p2 = TimingHeader::new(SeqNo::from(301), MediaTime::from_90khz(6_000));
        let out2 = r.rewrite(&p2, false).unwrap();
        assert_eq!(out2.0, SeqNo::from(301));
        assert!(out2.0 > out1.0);

        // Layer B (different encoder with far-apart seq range)
        let p3 = TimingHeader::new(SeqNo::from(10_000), MediaTime::from_90khz(90_000));
        let out3 = r.rewrite(&p3, true).unwrap();
        assert_eq!(
            out3.0,
            SeqNo::from(302),
            "First packet of new layer must continue from last + 1"
        );
        assert!(out3.0 > out2.0);

        // More packets from new layer — should remain monotonic
        let p4 = TimingHeader::new(SeqNo::from(10_001), MediaTime::from_90khz(93_000));
        let out4 = r.rewrite(&p4, false).unwrap();
        assert_eq!(out4.0, SeqNo::from(303));
        assert!(out4.0 > out3.0);
    }

    #[test]
    fn test_multiple_switches_stay_monotonic() {
        let mut r = RtpRewriter::new();

        let mut seq = 100;
        let mut ts = 30_000;

        // simulate rapid layer switches with large jumps
        for i in 0..5 {
            let p = TimingHeader::new(SeqNo::from(seq), MediaTime::from_90khz(ts));
            let is_switch = i % 2 == 1; // every other packet triggers a "switch"
            let out = r.rewrite(&p, is_switch).unwrap();

            if i > 0 {
                assert!(
                    out.0 > SeqNo::from(100 + (i as u64 - 1)),
                    "seq monotonic at iteration {i}"
                );
            }

            seq += 10_000; // jump far ahead in incoming seq space
            ts += 90_000;
        }
    }

    #[test]
    fn test_timestamp_continuity() {
        let mut r = RtpRewriter::new();

        let p1 = TimingHeader::new(SeqNo::from(500), MediaTime::from_90khz(0));
        let out1 = r.rewrite(&p1, false).unwrap();

        let p2 = TimingHeader::new(SeqNo::from(9_000), MediaTime::from_90khz(90_000));
        let out2 = r.rewrite(&p2, true).unwrap();
        assert!(out2.1 >= out1.1, "timestamp should not go backward");

        let p3 = TimingHeader::new(SeqNo::from(9_001), MediaTime::from_90khz(93_000));
        let out3 = r.rewrite(&p3, false).unwrap();
        assert!(
            out3.1 > out2.1,
            "timestamp should move forward after switch"
        );
    }

    #[test]
    fn reproduce_backward_seq_after_switch() {
        use super::*;
        // ramp the outgoing stream to a high sequence number similar to the log
        let mut r = RtpRewriter::new();

        // forward many packets to advance highest_forwarded_seq
        for i in 0..(20231u64 - 200u64) {
            let seq = SeqNo::from(200 + i);
            let ts = MediaTime::from_90khz(3_000 * (i + 1));
            let p = TimingHeader::new(seq, ts);
            let out = r.rewrite(&p, false).expect("should forward");
            // sanity: outgoing should equal incoming at start
            assert_eq!(out.0, seq);
        }

        // now highest_forwarded_seq should be 20230 (the last forwarded), next should be 20231
        // send one more normal packet to be explicit
        let last_in = TimingHeader::new(SeqNo::from(20230), MediaTime::from_90khz(756826681));
        let out_last = r.rewrite(&last_in, false).unwrap();
        assert_eq!(out_last.0, SeqNo::from(20230));

        // Now simulate a switch: incoming new layer packet has seq 19322 (lower on-wire),
        // like in your log. We expect the rewriter to produce next outgoing == last_out + 1 = 20231.
        let switching_pkt =
            TimingHeader::new(SeqNo::from(19322), MediaTime::from_90khz(1860279948));
        let out_switch = r
            .rewrite(&switching_pkt, true)
            .expect("switch should produce a forwarded seq");

        // If the rewriter is correct this must be last_out + 1 (20231). If it returns 19322, we reproduced the bug.
        assert_eq!(
            out_switch.0,
            SeqNo::from(20231),
            "after switch next outgoing seq must be last_out + 1 (no backward jump)"
        );
    }
}
