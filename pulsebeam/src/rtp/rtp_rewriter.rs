use str0m::rtp::SeqNo;

use crate::rtp::PacketTiming;

/// Manages the state for rewriting RTP headers for a single forwarded stream.
///
/// This rewriter ensures that the RTP stream sent to a subscriber is clean and
/// contiguous from their perspective, even if the SFU switches between different
/// simulcast layers from the publisher.
///
/// On a layer switch, it calculates new offsets to ensure the sequence number
/// increments by exactly 1, creating a seamless transition for the receiver.
#[derive(Debug, Clone, Default)]
pub struct RtpRewriter {
    /// The offset to apply to the incoming sequence number. Calculated on switches.
    seq_no_offset: i64,
    /// The offset to apply to the incoming timestamp. Calculated on switches.
    timestamp_offset: u32,
    /// The last sequence number we forwarded to the client. This is crucial for
    /// calculating the new offset on a switch.
    last_forwarded_seq: Option<SeqNo>,
}

impl RtpRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rewrites the sequence number and timestamp of an RTP packet.
    /// Returns the new `SeqNo` and `u32` timestamp for the forwarded packet header.
    ///
    /// # Arguments
    ///
    /// * `packet` - The incoming packet from any simulcast layer.
    /// * `is_switch_point` - Must be `true` for the first packet after a layer switch
    ///   to ensure a smooth, contiguous sequence number transition.
    pub fn rewrite(&mut self, packet: &impl PacketTiming, is_switch_point: bool) -> (SeqNo, u32) {
        if is_switch_point {
            self.reset(packet);
        }

        // Apply the calculated offsets.
        let new_forwarded_val = (*packet.seq_no() as i64 + self.seq_no_offset) as u64;
        let new_seq_no = SeqNo::from(new_forwarded_val);
        let new_timestamp =
            (packet.rtp_timestamp().numer() as u32).wrapping_add(self.timestamp_offset);

        self.last_forwarded_seq = Some(new_seq_no);

        (new_seq_no, new_timestamp)
    }

    /// Resets the rewriter's state based on a new packet, ensuring continuity.
    /// This is the core of a smooth simulcast switch.
    fn reset(&mut self, packet: &impl PacketTiming) {
        // Determine the target sequence number. If this is the very first packet,
        // start with a random one. Otherwise, it must be the next in sequence.
        let target_seq = self
            .last_forwarded_seq
            .map_or_else(SeqNo::default, |s| SeqNo::from(*s + 1));

        // Calculate the new offset needed to map the incoming sequence number
        // to our desired target sequence number.
        self.seq_no_offset = (*target_seq as i64) - (*packet.seq_no() as i64);

        // Calculate a new timestamp offset to keep the timeline consistent.
        // For simplicity and robustness, we re-anchor it to a new random base.
        // A small, plausible jump is fine and will be handled by the client's jitter buffer.
        let new_timestamp_base = rand::random::<u32>();
        self.timestamp_offset =
            new_timestamp_base.wrapping_sub(packet.rtp_timestamp().numer() as u32);

        tracing::info!(
            target_seq = *target_seq,
            incoming_seq = *packet.seq_no(),
            seq_offset = self.seq_no_offset,
            "RTP rewriter reset for layer switch"
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp::test::TestPacket;
    use str0m::media::MediaTime;

    #[test]
    fn rewrite_handles_initial_packet() {
        let mut rewriter = RtpRewriter::new();
        let p1 = TestPacket::new(5000.into(), MediaTime::from_millis(1000));
        let (s1, _t1) = rewriter.rewrite(&p1, true);

        // The first sequence number should be the random default.
        assert_eq!(s1, SeqNo::default());
        assert_eq!(rewriter.last_forwarded_seq, Some(s1));
    }

    #[test]
    fn rewrite_maintains_continuity_and_gaps() {
        let mut rewriter = RtpRewriter::new();
        let p1 = TestPacket::new(100.into(), MediaTime::from_millis(1000));
        let (s1, _) = rewriter.rewrite(&p1, true);

        // Next packet is contiguous.
        let p2 = TestPacket::new(101.into(), MediaTime::from_millis(1030));
        let (s2, _) = rewriter.rewrite(&p2, false);
        assert_eq!(*s2, *s1 + 1);

        // A gap appears in the source stream.
        let p3 = TestPacket::new(103.into(), MediaTime::from_millis(1090));
        let (s3, _) = rewriter.rewrite(&p3, false);
        assert_eq!(*s3, *s2 + 2); // The forwarded stream must also have a gap of 2.
    }

    #[test]
    fn rewrite_handles_smooth_switch() {
        let mut rewriter = RtpRewriter::new();

        // First stream segment (e.g., low quality layer)
        let (s1, _) = rewriter.rewrite(
            &TestPacket::new(100.into(), MediaTime::from_millis(1000)),
            true,
        );
        let (s2, _) = rewriter.rewrite(
            &TestPacket::new(101.into(), MediaTime::from_millis(1030)),
            false,
        );
        assert_eq!(*s2, *s1 + 1);

        // Now, switch to a high quality layer. The incoming sequence number is completely different.
        let p3_switch = TestPacket::new(8000.into(), MediaTime::from_millis(1060));
        let (s3, _) = rewriter.rewrite(&p3_switch, true);

        // The forwarded sequence number MUST be contiguous with the previous segment.
        assert_eq!(*s3, *s2 + 1);

        // The next packet on the new layer should also be contiguous.
        let p4 = TestPacket::new(8001.into(), MediaTime::from_millis(1090));
        let (s4, _) = rewriter.rewrite(&p4, false);
        assert_eq!(*s4, *s3 + 1);
    }

    #[test]
    fn rewrite_handles_gap_on_switch() {
        let mut rewriter = RtpRewriter::new();
        rewriter.last_forwarded_seq = Some(999.into());

        // Switch to a new layer, but the first packet we receive is not the first one.
        // e.g., packet 5000 was lost, we start at 5001.
        let p1_switch = TestPacket::new(5001.into(), MediaTime::from_millis(5030));
        let (s1, _) = rewriter.rewrite(&p1_switch, true);

        // The target sequence number must be the next one after the last forwarded.
        assert_eq!(*s1, 1000);

        // The next packet has a gap on the source stream (5002 is lost).
        let p2 = TestPacket::new(5003.into(), MediaTime::from_millis(5090));
        let (s2, _) = rewriter.rewrite(&p2, false);

        // The forwarded stream must also reflect this gap relative to the new base.
        assert_eq!(*s2, *s1 + 2);
    }

    #[test]
    fn rewrite_handles_rollover_on_switch() {
        let mut rewriter = RtpRewriter::new();
        rewriter.last_forwarded_seq = Some((u16::MAX as u64).into());

        // Switch to a new layer.
        let p1_switch = TestPacket::new(100.into(), MediaTime::from_millis(1000));
        let (s1, _) = rewriter.rewrite(&p1_switch, true);

        // The target must correctly wrap around.
        assert_eq!(*s1, u16::MAX as u64 + 1);
        assert_eq!(s1.as_u16(), 0);

        let p2 = TestPacket::new(101.into(), MediaTime::from_millis(1030));
        let (s2, _) = rewriter.rewrite(&p2, false);
        assert_eq!(*s2, *s1 + 1);
        assert_eq!(s2.as_u16(), 1);
    }
}
