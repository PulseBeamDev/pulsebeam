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
}

impl Default for RtpRewriter {
    fn default() -> Self {
        Self {
            seq_no_offset: 0,
            timestamp_offset: 0,
            highest_forwarded_seq: SeqNo::new(),
            highest_forwarded_ts: MediaTime::from_90khz(0),
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
        let incoming_seq = packet.seq_no();

        if is_switch {
            self.reset(packet);
        }

        let new_seq_val = (*incoming_seq as i64 + self.seq_no_offset) as u64;
        let new_seq = SeqNo::from(new_seq_val);

        let new_ts_numer = packet.rtp_timestamp().numer() as i64 + self.timestamp_offset;
        let new_ts = MediaTime::new(new_ts_numer as u64, packet.rtp_timestamp().frequency());

        // // 5. Update the state for the next packet.
        // self.highest_forwarded_seq = Some(new_seq);
        // self.highest_forwarded_ts = Some(new_ts);
        // self.last_incoming_ts = Some(packet.rtp_timestamp());
        //
        // // 6. Cache the result for duplicate detection.
        // self.cache_packet(*incoming_seq, new_seq, new_ts);

        Some((new_seq, new_ts))
    }

    /// Resets the rewriter's state to seamlessly transition to a new stream.
    fn reset(&mut self, packet: &impl PacketTiming) {
        let target_seq_no = *self.highest_forwarded_seq + 1;
        self.seq_no_offset = target_seq_no.wrapping_sub(*packet.seq_no());

        let rtp_timestamp = packet.rtp_timestamp();
        let target_ts = self
            .highest_forwarded_ts
            .rebase(rtp_timestamp.frequency())
            .numer();
        let target_ts = MediaTime::new(target_ts, rtp_timestamp.frequency());

        // TODO: determine sample duration with a better approach?

        // let incoming_ts = packet.rtp_timestamp();
        //
        // let target_ts = if let (Some(last_fwd_ts), Some(last_inc_ts)) =
        //     (self.highest_forwarded_ts, self.last_incoming_ts)
        // {
        //     let duration = incoming_ts.saturating_sub(last_inc_ts);
        //     let last_fwd_ts_rebased = last_fwd_ts.rebase(duration.frequency());
        //     last_fwd_ts_rebased + duration
        // } else {
        //     incoming_ts
        // };
        //
        // self.timestamp_offset = (target_ts.numer() as i64) - (incoming_ts.numer() as i64);
        //
        // self.active_stream_start_seq = Some(packet.seq_no());
        //
        // tracing::info!(
        //     incoming_seq = *packet.seq_no(),
        //     target_seq = target_seq,
        //     seq_offset = self.seq_no_offset,
        //     ts_offset = self.timestamp_offset,
        //     "RTP rewriter reset for new stream"
        // );
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp::TimingHeader;

    #[test]
    fn test_get_offset() {
        let seq_nos = [(u64::MAX, 1), (1, u64::MAX), (5, 1), (1, 5), (1, 1)];
        for (original, target) in seq_nos.iter().copied() {
            let offset = target.wrapping_sub(original);
            assert_eq!(original.wrapping_add(offset), target);
        }
    }

    #[test]
    fn handles_initial_packet() {
        let mut rewriter = RtpRewriter::new();
        let p1 = TimingHeader::new(5000.into(), MediaTime::from_90khz(1000));

        let (s1, t1) = rewriter.rewrite(&p1, false).unwrap();

        assert_eq!(*s1, 5000);
        assert_eq!(t1, MediaTime::from_90khz(1000));
    }

    #[test]
    fn maintains_continuity() {
        let mut rewriter = RtpRewriter::new();
        let p1 = TimingHeader::new(100.into(), MediaTime::from_90khz(1000));
        let (s1, t1) = rewriter.rewrite(&p1, false).unwrap();

        let p2 = TimingHeader::new(101.into(), MediaTime::from_90khz(1030));
        let (s2, t2) = rewriter.rewrite(&p2, false).unwrap();

        assert_eq!(*s2, *s1 + 1);
        assert_eq!(t2, MediaTime::from_90khz(1030));
        assert_eq!(t2.saturating_sub(t1), MediaTime::from_90khz(30));
    }

    #[test]
    fn handles_signaled_switch() {
        let mut rewriter = RtpRewriter::new();

        let p1 = TimingHeader::new(100.into(), MediaTime::from_90khz(90000));
        rewriter.rewrite(&p1, false).unwrap();
        let p2 = TimingHeader::new(101.into(), MediaTime::from_90khz(92700));
        let (s2, t2) = rewriter.rewrite(&p2, false).unwrap();

        let p_switch = TimingHeader::new(5000.into(), MediaTime::from_90khz(95400));
        let (s_switch, t_switch) = rewriter.rewrite(&p_switch, true).unwrap();

        assert_eq!(*s_switch, *s2 + 1);
        // The time delta between rewritten timestamps must equal the delta between original timestamps.
        let expected_delta = p_switch.rtp_ts.saturating_sub(p2.rtp_ts);
        assert_eq!(t_switch.saturating_sub(t2), expected_delta);

        let p_next = TimingHeader::new(5001.into(), MediaTime::from_90khz(98100));
        let (s_next, t_next) = rewriter.rewrite(&p_next, false).unwrap();

        assert_eq!(*s_next, *s_switch + 1);
        let expected_delta2 = p_next.rtp_ts.saturating_sub(p_switch.rtp_ts);
        assert_eq!(t_next.saturating_sub(t_switch), expected_delta2);
    }

    #[test]
    fn handles_duplicate_packets() {
        let mut rewriter = RtpRewriter::new();

        let p1 = TimingHeader::new(100.into(), MediaTime::from_90khz(90000));
        let (s1, t1) = rewriter.rewrite(&p1, false).unwrap();

        let (s1_dup, t1_dup) = rewriter.rewrite(&p1, false).unwrap();

        assert_eq!(*s1_dup, *s1);
        assert_eq!(t1_dup, t1);
    }
}
