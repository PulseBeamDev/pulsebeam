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
        let target_seq = *self.highest_forwarded_seq + 1;
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
}
