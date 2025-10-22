use str0m::rtp::SeqNo;

use crate::rtp::PacketTiming;

/// Manages the state for rewriting RTP headers for a single forwarded stream.
///
/// This rewriter ensures that the RTP stream sent to a subscriber is clean and
/// contiguous from their perspective. It mirrors the sequence number and timestamp
/// *deltas* from the original stream using robust `u64` sequence arithmetic.
#[derive(Debug, Clone)]
pub struct RtpRewriter {
    last_incoming_seq: SeqNo,
    last_forwarded_seq: SeqNo,
    timestamp_offset: u32,
}

impl RtpRewriter {
    /// Creates a new rewriter, initialized from the first packet of a stream.
    pub fn new(first_packet: &impl PacketTiming) -> Self {
        Self {
            last_incoming_seq: first_packet.seq_no(),
            last_forwarded_seq: SeqNo::default(),
            timestamp_offset: rand::random::<u32>(),
        }
    }

    /// Rewrites the sequence number and timestamp of an RTP packet.
    /// Returns the new `SeqNo` and `u32` timestamp for the forwarded packet header.
    pub fn rewrite(&mut self, packet: &impl PacketTiming) -> (SeqNo, u32) {
        // Calculate the jump in sequence numbers from the original stream.
        // Using the full u64 `SeqNo` correctly handles rollovers and large loss gaps.
        let seq_delta = *packet.seq_no() - *self.last_incoming_seq;
        let new_forwarded_val = *self.last_forwarded_seq + seq_delta;
        let new_seq_no = SeqNo::from(new_forwarded_val);

        // Apply the fixed offset to the timestamp.
        let new_timestamp = packet.rtp_timestamp().numer() as u32;
        let new_timestamp = new_timestamp.wrapping_add(self.timestamp_offset);

        // Update state for the next packet.
        self.last_incoming_seq = packet.seq_no();
        self.last_forwarded_seq = new_seq_no;

        (new_seq_no, new_timestamp)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp::test::TestPacket;
    use str0m::media::MediaTime;

    #[test]
    fn rewrite_maintains_contiguous_seq_no() {
        let p1 = TestPacket {
            seq_no: 100.into(),
            rtp_ts: MediaTime::from_millis(1000),
            server_ts: tokio::time::Instant::now(),
        };
        let mut rewriter = RtpRewriter::new(&p1);
        let (s1, _) = rewriter.rewrite(&p1);

        let p2 = TestPacket {
            seq_no: 101.into(),
            rtp_ts: MediaTime::from_millis(1030),
            server_ts: tokio::time::Instant::now(),
        };
        let (s2, _) = rewriter.rewrite(&p2);

        assert_eq!(*s2, *s1 + 1);
    }

    #[test]
    fn rewrite_handles_seq_no_gap() {
        let p1 = TestPacket {
            seq_no: 100.into(),
            rtp_ts: MediaTime::from_millis(1000),
            server_ts: tokio::time::Instant::now(),
        };
        let mut rewriter = RtpRewriter::new(&p1);
        let (s1, _) = rewriter.rewrite(&p1);

        let p2 = TestPacket {
            seq_no: 105.into(),
            rtp_ts: MediaTime::from_millis(1150),
            server_ts: tokio::time::Instant::now(),
        };
        let (s2, _) = rewriter.rewrite(&p2);

        assert_eq!(*s2, *s1 + 5);
    }

    #[test]
    fn rewrite_maintains_timestamp_delta() {
        let p1 = TestPacket {
            seq_no: 100.into(),
            rtp_ts: MediaTime::from_millis(1000),
            server_ts: tokio::time::Instant::now(),
        };
        let mut rewriter = RtpRewriter::new(&p1);
        let (_, t1) = rewriter.rewrite(&p1);

        let p2 = TestPacket {
            seq_no: 101.into(),
            rtp_ts: MediaTime::from_millis(1030),
            server_ts: tokio::time::Instant::now(),
        };
        let (_, t2) = rewriter.rewrite(&p2);

        let ts_delta = t2.wrapping_sub(t1);

        // The rebased delta should be ~30ms in a 90kHz clock.
        let expected_ts_delta = 30 * 90; // 30ms * 90kHz
        let actual_ts_delta = (p2
            .rtp_timestamp()
            .rebase(str0m::media::Frequency::NINETY_KHZ)
            .numer()
            - p1.rtp_timestamp()
                .rebase(str0m::media::Frequency::NINETY_KHZ)
                .numer()) as u32;

        assert_eq!(ts_delta, actual_ts_delta);
        assert!((ts_delta as i32 - expected_ts_delta as i32).abs() < 2);
    }

    #[test]
    fn rewrite_handles_rollover() {
        let p1 = TestPacket {
            seq_no: (u16::MAX as u64).into(),
            rtp_ts: MediaTime::from_millis(1000),
            server_ts: tokio::time::Instant::now(),
        };
        let mut rewriter = RtpRewriter::new(&p1);
        let (s1, _) = rewriter.rewrite(&p1);

        let p2 = TestPacket {
            seq_no: (u16::MAX as u64 + 1).into(),
            rtp_ts: MediaTime::from_millis(1030),
            server_ts: tokio::time::Instant::now(),
        };
        let (s2, _) = rewriter.rewrite(&p2);

        assert_eq!(*s2, *s1 + 1);
        assert_eq!(s2.as_u16(), s1.as_u16().wrapping_add(1));
    }
}
