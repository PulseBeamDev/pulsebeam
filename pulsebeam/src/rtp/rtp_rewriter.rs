use std::collections::BTreeMap;
use str0m::media::MediaTime;
use str0m::rtp::SeqNo;

/// Manages the state for rewriting RTP headers for a single forwarded stream.
///
/// This rewriter ensures that the RTP stream sent to a subscriber is clean and
/// contiguous, automatically detecting and smoothing out discontinuities caused by
/// simulcast layer switches or ingress packet loss. It also handles out-of-order
/// packet arrival robustly.
#[derive(Debug, Clone)]
pub struct RtpRewriter {
    /// The offset to apply to the incoming sequence number.
    seq_no_offset: i64,
    /// The offset to apply to the incoming timestamp numerator.
    timestamp_offset: i64,

    /// The last sequence number we forwarded to the client.
    last_forwarded_seq: Option<SeqNo>,
    /// The last timestamp we forwarded to the client.
    last_forwarded_ts: Option<MediaTime>,

    /// The highest sequence number received from the source (for gap detection).
    highest_incoming_seq: Option<SeqNo>,
    /// The timestamp associated with the highest incoming sequence.
    highest_incoming_ts: Option<MediaTime>,

    /// Cache of recently seen packets to detect duplicates and severe reordering.
    /// Maps incoming seq -> (rewritten seq, rewritten timestamp)
    /// Limited size to prevent unbounded memory growth.
    seen_packets: BTreeMap<u64, (SeqNo, u32)>,

    /// Maximum number of packets to keep in the seen cache.
    max_cache_size: usize,

    /// Threshold for detecting a layer switch vs normal reordering.
    /// If a packet arrives more than this many sequence numbers behind the highest,
    /// we consider it either very late or from a previous layer.
    reorder_threshold: u64,
}

impl Default for RtpRewriter {
    fn default() -> Self {
        Self {
            seq_no_offset: 0,
            timestamp_offset: 0,
            last_forwarded_seq: None,
            last_forwarded_ts: None,
            highest_incoming_seq: None,
            highest_incoming_ts: None,
            seen_packets: BTreeMap::new(),
            max_cache_size: 100,
            reorder_threshold: 1000, // Tolerate up to ~1000 packets of reordering
        }
    }
}

impl RtpRewriter {
    /// Creates a new, default `RtpRewriter`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new `RtpRewriter` with custom parameters.
    pub fn with_params(max_cache_size: usize, reorder_threshold: u64) -> Self {
        Self {
            max_cache_size,
            reorder_threshold,
            ..Default::default()
        }
    }

    /// Rewrites the sequence number and timestamp of an RTP packet.
    /// Returns the new `SeqNo` and `u32` timestamp for the forwarded packet header.
    ///
    /// This method automatically detects if a reset is needed based on sequence
    /// number discontinuities, and handles out-of-order packets gracefully.
    pub fn rewrite(&mut self, packet: &impl crate::rtp::PacketTiming) -> (SeqNo, u32) {
        let incoming_seq = packet.seq_no();
        let incoming_seq_val = *incoming_seq;

        // Check if we've already processed this packet (duplicate or retransmission)
        if let Some(&(cached_seq, cached_ts)) = self.seen_packets.get(&incoming_seq_val) {
            tracing::trace!(
                incoming_seq = incoming_seq_val,
                "Duplicate packet detected, returning cached rewrite"
            );
            return (cached_seq, cached_ts);
        }

        // Determine if this packet triggers a reset (layer switch or initial packet)
        let needs_reset = self.should_reset(incoming_seq);

        if needs_reset {
            self.reset(packet);
            // After reset, highest_incoming is set to this packet in reset()
            // No need to update it again below
        } else {
            // Normal path: Update highest seen sequence number (accounting for rollover)
            if let Some(highest) = self.highest_incoming_seq {
                if seq_is_newer(incoming_seq, highest) {
                    self.highest_incoming_seq = Some(incoming_seq);
                    self.highest_incoming_ts = Some(packet.rtp_timestamp());
                }
            } else {
                // This shouldn't happen after init, but handle it
                self.highest_incoming_seq = Some(incoming_seq);
                self.highest_incoming_ts = Some(packet.rtp_timestamp());
            }
        }

        // Apply the calculated offsets.
        let new_forwarded_val = (incoming_seq_val as i64 + self.seq_no_offset) as u64;
        let new_seq_no = SeqNo::from(new_forwarded_val);

        let new_timestamp_numer = packet.rtp_timestamp().numer() as i64 + self.timestamp_offset;
        let new_timestamp = new_timestamp_numer as u32;

        // Update last forwarded if this is actually the newest packet we've sent
        if let Some(last_fwd) = self.last_forwarded_seq {
            if seq_is_newer(new_seq_no, last_fwd) {
                self.last_forwarded_seq = Some(new_seq_no);
                self.last_forwarded_ts = Some(MediaTime::new(
                    new_timestamp_numer as u64,
                    packet.rtp_timestamp().frequency(),
                ));
            }
        } else {
            self.last_forwarded_seq = Some(new_seq_no);
            self.last_forwarded_ts = Some(MediaTime::new(
                new_timestamp_numer as u64,
                packet.rtp_timestamp().frequency(),
            ));
        }

        // Cache this packet's rewrite
        self.cache_packet(incoming_seq_val, new_seq_no, new_timestamp);

        (new_seq_no, new_timestamp)
    }

    /// Determines if a reset is needed for the given incoming packet.
    fn should_reset(&self, incoming_seq: SeqNo) -> bool {
        // First packet ever
        if self.last_forwarded_seq.is_none() {
            return true;
        }

        let highest = match self.highest_incoming_seq {
            Some(h) => h,
            None => return true,
        };

        let incoming_val = *incoming_seq;
        let highest_val = *highest;

        // If this packet is newer than what we've seen, check if it's a big jump forward
        if seq_is_newer(incoming_seq, highest) {
            let forward_distance = seq_distance(highest, incoming_seq);

            if forward_distance > self.reorder_threshold {
                tracing::debug!(
                    incoming_seq = incoming_val,
                    highest_seq = highest_val,
                    forward_distance = forward_distance,
                    "Detected layer switch due to large forward gap"
                );
                return true;
            }
        } else {
            // Packet is older than highest. Check if it's TOO old (severe reordering or old layer)
            let backward_distance = seq_distance(incoming_seq, highest);

            if backward_distance > self.reorder_threshold {
                tracing::debug!(
                    incoming_seq = incoming_val,
                    highest_seq = highest_val,
                    backward_distance = backward_distance,
                    "Packet too far in the past, ignoring as old layer"
                );
                // Note: You might want to drop this packet instead of resetting
                // For now, we reset to handle potential edge cases
                return true;
            }
        }

        // Normal case: packet is within reasonable reordering distance
        false
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
            (self.last_forwarded_ts, self.highest_incoming_ts)
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

        // CRITICAL: Reset the highest_incoming tracking to THIS packet
        // This prevents every subsequent packet from looking like it's going backward
        self.highest_incoming_seq = Some(packet.seq_no());
        self.highest_incoming_ts = Some(packet.rtp_timestamp());

        // Clear the packet cache since we're starting a new layer/stream
        self.seen_packets.clear();

        tracing::info!(
            target_seq = target_seq,
            incoming_seq = *packet.seq_no(),
            seq_offset = self.seq_no_offset,
            ts_offset = self.timestamp_offset,
            "RTP rewriter reset due to stream discontinuity"
        );
    }

    /// Adds a packet to the seen cache, maintaining size limits.
    fn cache_packet(&mut self, incoming_seq: u64, rewritten_seq: SeqNo, rewritten_ts: u32) {
        self.seen_packets
            .insert(incoming_seq, (rewritten_seq, rewritten_ts));

        // Trim old entries if cache is too large
        while self.seen_packets.len() > self.max_cache_size {
            if let Some(&first_key) = self.seen_packets.keys().next() {
                self.seen_packets.remove(&first_key);
            }
        }
    }
}

/// Compares two sequence numbers accounting for rollover.
/// Returns true if `a` is newer than `b` in the RTP sequence space.
fn seq_is_newer(a: SeqNo, b: SeqNo) -> bool {
    let a_val = *a;
    let b_val = *b;

    // In the u16 space with rollover
    let a_u16 = (a_val & 0xFFFF) as u16;
    let b_u16 = (b_val & 0xFFFF) as u16;

    // Use wrapping subtraction to handle rollover
    let diff = a_u16.wrapping_sub(b_u16);

    // If diff is small (< 32768), a is ahead. If large (> 32768), b is ahead.
    diff < 32768 && diff != 0
}

/// Calculates the distance between two sequence numbers.
/// Returns the forward distance from `a` to `b` (how many steps forward is b from a).
fn seq_distance(a: SeqNo, b: SeqNo) -> u64 {
    let a_val = *a;
    let b_val = *b;

    let a_u16 = (a_val & 0xFFFF) as u16;
    let b_u16 = (b_val & 0xFFFF) as u16;

    // Forward distance with rollover handling
    let diff = b_u16.wrapping_sub(a_u16);

    if diff < 32768 {
        diff as u64
    } else {
        // Backward distance, return as large forward distance
        (65536u32 - diff as u32) as u64
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
        let p1 = TestPacket::new(5000.into(), MediaTime::from_90khz(1000));

        let (s1, t1) = rewriter.rewrite(&p1);

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

        let p2 = TestPacket::new(101.into(), MediaTime::from_90khz(1030));
        let (s2, t2) = rewriter.rewrite(&p2);

        assert_eq!(*s2, *s1 + 1);
        assert_eq!(t2, t1 + 30);
    }

    #[test]
    fn rewrite_handles_out_of_order_packets() {
        let mut rewriter = RtpRewriter::new();

        // Process packets in order first
        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(90000));
        let (s1, t1) = rewriter.rewrite(&p1);

        let p2 = TestPacket::new(101.into(), MediaTime::from_90khz(92700));
        let (s2, t2) = rewriter.rewrite(&p2);

        let p4 = TestPacket::new(103.into(), MediaTime::from_90khz(98100));
        let (s4, t4) = rewriter.rewrite(&p4);

        // Now packet 102 arrives late
        let p3 = TestPacket::new(102.into(), MediaTime::from_90khz(95400));
        let (s3, t3) = rewriter.rewrite(&p3);

        // Should maintain original offsets, not trigger reset
        assert_eq!(*s3, *s2 + 1);
        assert_eq!(t3, t2 + 2700);

        // Verify s4 is still ahead
        assert_eq!(*s4, *s3 + 1);
    }

    #[test]
    fn rewrite_handles_duplicate_packets() {
        let mut rewriter = RtpRewriter::new();

        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(90000));
        let (s1, t1) = rewriter.rewrite(&p1);

        // Process the same packet again
        let (s1_dup, t1_dup) = rewriter.rewrite(&p1);

        // Should return identical results
        assert_eq!(*s1_dup, *s1);
        assert_eq!(t1_dup, t1);
    }

    #[test]
    fn rewrite_handles_smooth_switch() {
        let mut rewriter = RtpRewriter::new();

        let (s1, _) = rewriter.rewrite(&TestPacket::new(100.into(), MediaTime::from_90khz(90000)));
        let (s2, t2_rewritten) =
            rewriter.rewrite(&TestPacket::new(101.into(), MediaTime::from_90khz(92700)));
        assert_eq!(*s2, 101);

        // Large gap triggers reset
        let p3_switch = TestPacket::new(8000.into(), MediaTime::from_90khz(95400));
        let (s3, t3_rewritten) = rewriter.rewrite(&p3_switch);

        assert_eq!(*s3, *s2 + 1);
        let expected_t3 = t2_rewritten.wrapping_add(2700);
        assert_eq!(t3_rewritten, expected_t3);
    }

    #[test]
    fn rewrite_no_reset_for_small_gaps() {
        let mut rewriter = RtpRewriter::with_params(100, 100);

        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(90000));
        let (s1, _) = rewriter.rewrite(&p1);

        // Skip ahead by 10 (within threshold)
        let p11 = TestPacket::new(110.into(), MediaTime::from_90khz(117000));
        let (s11, _) = rewriter.rewrite(&p11);
        assert_eq!(*s11, *s1 + 10);

        // Fill in the gap - should not trigger reset
        let p5 = TestPacket::new(105.into(), MediaTime::from_90khz(103500));
        let (s5, _) = rewriter.rewrite(&p5);

        // Should use same offset, resulting in seq 105
        assert_eq!(*s5, *s1 + 5);
    }

    #[test]
    fn test_seq_is_newer() {
        assert!(seq_is_newer(101.into(), 100.into()));
        assert!(!seq_is_newer(100.into(), 101.into()));

        // Rollover cases
        assert!(seq_is_newer(1.into(), 65535.into()));
        assert!(!seq_is_newer(65535.into(), 1.into()));
    }

    #[test]
    fn test_seq_distance() {
        assert_eq!(seq_distance(100.into(), 105.into()), 5);
        assert_eq!(seq_distance(65535.into(), 1.into()), 2);
        assert_eq!(seq_distance(100.into(), 100.into()), 0);
    }
}
