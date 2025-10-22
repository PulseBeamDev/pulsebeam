use str0m::media::MediaTime;
use str0m::rtp::SeqNo;

// The PacketTiming trait is assumed to be defined elsewhere in the crate,
// allowing the rewriter to work with any packet structure.
//
// pub trait PacketTiming {
//     fn seq_no(&self) -> SeqNo;
//     fn rtp_timestamp(&self) -> MediaTime;
// }

/// Manages the state for rewriting RTP headers for a single forwarded stream.
///
/// This rewriter ensures that the RTP stream sent to a subscriber is clean and
/// contiguous, automatically detecting and smoothing out discontinuities caused by
/// simulcast layer switches or ingress packet loss.
///
/// On a discontinuity, it calculates new offsets for the sequence number and
/// timestamp to ensure the forwarded stream increments smoothly, creating a
/// seamless transition for the receiver and preventing unnecessary NACK requests.
#[derive(Debug, Clone, Default)]
pub struct RtpRewriter {
    /// The offset to apply to the incoming sequence number.
    seq_no_offset: i64,
    /// The offset to apply to the incoming timestamp numerator.
    timestamp_offset: i64,

    /// The last sequence number we forwarded to the client.
    last_forwarded_seq: Option<SeqNo>,
    /// The last timestamp we forwarded to the client.
    last_forwarded_ts: Option<MediaTime>,

    /// The last sequence number received from the source. Used for gap detection.
    last_incoming_seq: Option<SeqNo>,
    /// The last timestamp received from the source. Used for duration calculation.
    last_incoming_ts: Option<MediaTime>,
}

impl RtpRewriter {
    /// Creates a new, default `RtpRewriter`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rewrites the sequence number and timestamp of an RTP packet.
    /// Returns the new `SeqNo` and `u32` timestamp for the forwarded packet header.
    ///
    /// This method automatically detects if a reset is needed based on sequence
    /// number discontinuities.
    pub fn rewrite(&mut self, packet: &impl crate::rtp::PacketTiming) -> (SeqNo, u32) {
        let incoming_seq = packet.seq_no();

        // Automatically trigger a reset if this is the first packet we've ever seen,
        // or if the incoming sequence number is not the expected next one. This
        // handles the initial packet, simulcast layer switches, and ingress loss.
        let is_discontiguous = self
            .last_incoming_seq
            .map_or(false, |last| *incoming_seq != *last + 1);

        if self.last_forwarded_seq.is_none() || is_discontiguous {
            self.reset(packet);
        }

        // Apply the calculated offsets.
        let new_forwarded_val = (*incoming_seq as i64 + self.seq_no_offset) as u64;
        let new_seq_no = SeqNo::from(new_forwarded_val);

        let new_timestamp_numer = packet.rtp_timestamp().numer() as i64 + self.timestamp_offset;
        let new_timestamp = new_timestamp_numer as u32;

        // Update state for the next packet.
        self.last_forwarded_seq = Some(new_seq_no);
        self.last_incoming_seq = Some(incoming_seq);
        self.last_incoming_ts = Some(packet.rtp_timestamp());
        self.last_forwarded_ts = Some(MediaTime::new(
            new_timestamp_numer as u64,
            packet.rtp_timestamp().frequency(),
        ));

        (new_seq_no, new_timestamp)
    }

    /// Resets the rewriter's state based on a new packet, ensuring continuity.
    /// This is the core of a smooth simulcast switch.
    fn reset(&mut self, packet: &impl crate::rtp::PacketTiming) {
        // 1. Calculate the sequence number offset.
        // The target is the next number after the last one we sent.
        // If it's the first packet ever, we start its sequence from its original value.
        let target_seq = self
            .last_forwarded_seq
            .map_or_else(|| *packet.seq_no(), |s| *s + 1);
        self.seq_no_offset = (target_seq as i64) - (*packet.seq_no() as i64);

        // 2. Calculate the timestamp offset using MediaTime's precise arithmetic.
        let new_incoming_ts = packet.rtp_timestamp();

        let target_ts = if let (Some(last_fwd_ts), Some(last_in_ts)) =
            (self.last_forwarded_ts, self.last_incoming_ts)
        {
            // Calculate how much time passed on the sender's clock.
            let duration = new_incoming_ts.saturating_sub(last_in_ts);
            // Add that duration to our forwarded timeline to keep it consistent.
            last_fwd_ts + duration
        } else {
            // This is the first packet. The forwarded timeline starts with its timestamp.
            new_incoming_ts
        };

        // The offset is the difference between where the timeline should be and where it is.
        // We must rebase the target timestamp to the new packet's clock rate before subtracting.
        let target_ts_rebased = target_ts.rebase(new_incoming_ts.frequency());
        self.timestamp_offset =
            (target_ts_rebased.numer() as i64) - (new_incoming_ts.numer() as i64);

        tracing::info!(
            target_seq = target_seq,
            incoming_seq = *packet.seq_no(),
            seq_offset = self.seq_no_offset,
            ts_offset = self.timestamp_offset,
            "RTP rewriter reset due to stream discontinuity"
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp::PacketTiming;
    use crate::rtp::test::TestPacket;
    use str0m::media::{Frequency, MediaTime};

    #[test]
    fn rewrite_handles_initial_packet() {
        let mut rewriter = RtpRewriter::new();
        let p1 = TestPacket::new(5000.into(), MediaTime::from_90khz(1000));

        let (s1, t1) = rewriter.rewrite(&p1);

        // The first packet should pass through unmodified (offsets are 0).
        assert_eq!(*s1, 5000);
        assert_eq!(t1, 1000);
        assert_eq!(rewriter.seq_no_offset, 0);
        assert_eq!(rewriter.timestamp_offset, 0);
    }

    #[test]
    fn rewrite_maintains_continuity() {
        let mut rewriter = RtpRewriter::new();
        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(1000));
        let (s1, t1) = rewriter.rewrite(&p1);

        // Next packet is contiguous.
        let p2 = TestPacket::new(101.into(), MediaTime::from_90khz(1030));
        let (s2, t2) = rewriter.rewrite(&p2);

        assert_eq!(*s2, *s1 + 1);
        assert_eq!(t2, t1 + 30);
    }

    #[test]
    fn rewrite_handles_smooth_switch() {
        let mut rewriter = RtpRewriter::new();

        // First stream segment (e.g., low quality layer)
        let (s1, _) = rewriter.rewrite(&TestPacket::new(100.into(), MediaTime::from_90khz(90000)));
        let (s2, t2_rewritten) =
            rewriter.rewrite(&TestPacket::new(101.into(), MediaTime::from_90khz(92700))); // 30ms later
        assert_eq!(*s2, 101);

        // Now, switch to a high quality layer. Incoming seq/ts are completely different.
        // This packet is 30ms after the previous one on the sender's timeline.
        let p3_switch = TestPacket::new(8000.into(), MediaTime::from_90khz(95400));
        let (s3, t3_rewritten) = rewriter.rewrite(&p3_switch);

        // The forwarded sequence number MUST be contiguous. This prevents NACKs.
        assert_eq!(*s3, *s2 + 1);

        // The forwarded timestamp must also be contiguous.
        let expected_t3 = t2_rewritten.wrapping_add(2700); // 30ms @ 90kHz
        assert_eq!(t3_rewritten, expected_t3);

        // The next packet on the new layer should also be contiguous.
        let p4 = TestPacket::new(8001.into(), MediaTime::from_90khz(98100));
        let (s4, t4_rewritten) = rewriter.rewrite(&p4);
        assert_eq!(*s4, *s3 + 1);
        assert_eq!(t4_rewritten, t3_rewritten.wrapping_add(2700));
    }

    #[test]
    fn rewrite_repairs_ingress_packet_loss() {
        let mut rewriter = RtpRewriter::new();
        let (s1, t1) = rewriter.rewrite(&TestPacket::new(100.into(), MediaTime::from_90khz(90000)));

        // Packet 101 is lost on the way to the SFU. We receive 102 next.
        // This is 60ms after packet 100.
        let p2 = TestPacket::new(102.into(), MediaTime::from_90khz(95400));
        let (s2, t2) = rewriter.rewrite(&p2);

        // The rewriter should hide the gap from the client.
        assert_eq!(*s2, *s1 + 1);

        // The timestamp should still reflect the real time passage.
        assert_eq!(t2, t1 + 5400);
    }

    #[test]
    fn rewrite_handles_rollover_on_switch() {
        let mut rewriter = RtpRewriter::new();
        // Manually set state to be just before a rollover.
        rewriter.last_forwarded_seq = Some((u16::MAX as u64).into());
        rewriter.last_incoming_seq = Some((1000 as u64).into()); // from some old layer

        // Switch to a new layer.
        let p1_switch = TestPacket::new(5000.into(), MediaTime::from_90khz(1000));
        let (s1, _) = rewriter.rewrite(&p1_switch);

        // The target must correctly wrap around. The u64 value continues, but the u16 wraps.
        assert_eq!(*s1, u16::MAX as u64 + 1);
        assert_eq!(s1.as_u16(), 0);

        let p2 = TestPacket::new(5001.into(), MediaTime::from_90khz(1030));
        let (s2, _) = rewriter.rewrite(&p2);
        assert_eq!(*s2, *s1 + 1);
        assert_eq!(s2.as_u16(), 1);
    }

    #[test]
    fn rewrite_handles_mixed_clock_rates() {
        let mut rewriter = RtpRewriter::new();

        // Start with a 48kHz audio packet.
        let p1 = TestPacket::new(
            200.into(),
            MediaTime::new(48000, Frequency::FORTY_EIGHT_KHZ),
        );
        let (_, t1) = rewriter.rewrite(&p1);
        assert_eq!(t1, 48000);

        // Now "switch" to a 90kHz video packet that is 100ms later.
        // 100ms in 48kHz is 4800 ticks.
        // 100ms in 90kHz is 9000 ticks.
        let p2 = TestPacket::new(
            9000.into(),
            MediaTime::new(
                p1.rtp_timestamp().numer() + 4800 + 9000, // some unrelated high number for video
                Frequency::NINETY_KHZ,
            ),
        );
        let (_, t2) = rewriter.rewrite(&p2);

        // The rewriter must calculate the duration correctly across frequencies.
        // The expected rewritten timestamp is the last one plus 100ms in the new clockrate.
        let expected_t2 = t1.wrapping_add(9000);
        assert_eq!(t2, expected_t2);
    }
}
