use std::collections::BTreeMap;
use str0m::media::MediaTime;
use str0m::rtp::SeqNo;

/// Manages the state for rewriting RTP headers for a single forwarded stream.
///
/// This rewriter ensures that the RTP stream sent to a subscriber is clean and
/// contiguous, automatically detecting and smoothing out discontinuities caused by
/// simulcast layer switches or ingress packet loss. It handles out-of-order
/// packet arrival robustly.
#[derive(Debug)]
pub struct RtpRewriter {
    /// The offset to apply to the incoming sequence number.
    seq_no_offset: i64,
    /// The offset to apply to the incoming timestamp numerator.
    timestamp_offset: i64,

    /// The highest rewritten sequence number we've assigned.
    highest_rewritten_seq: Option<SeqNo>,
    /// The highest rewritten timestamp we've assigned.
    highest_rewritten_ts: Option<MediaTime>,

    /// The highest incoming sequence number we've seen from the current layer.
    highest_incoming_seq: Option<SeqNo>,
    /// The timestamp associated with the highest incoming sequence.
    highest_incoming_ts: Option<MediaTime>,

    /// Grace period after a switch: tolerate out-of-order packets without re-triggering reset.
    switch_grace_remaining: u32,

    /// Initial grace period packet count after a switch.
    switch_grace_period: u32,

    /// Cache of recently seen packets to detect duplicates and handle reordering.
    /// Maps incoming seq -> (rewritten seq, rewritten timestamp)
    seen_packets: BTreeMap<u64, (SeqNo, u32)>,

    /// Maximum number of packets to keep in the seen cache.
    max_cache_size: usize,

    /// Threshold for detecting a layer switch vs normal reordering.
    reorder_threshold: u64,
}

impl Default for RtpRewriter {
    fn default() -> Self {
        Self {
            seq_no_offset: 0,
            timestamp_offset: 0,
            highest_rewritten_seq: None,
            highest_rewritten_ts: None,
            highest_incoming_seq: None,
            highest_incoming_ts: None,
            switch_grace_remaining: 0,
            switch_grace_period: 50,
            seen_packets: BTreeMap::new(),
            max_cache_size: 200,
            reorder_threshold: 1000,
        }
    }
}

impl RtpRewriter {
    /// Creates a new, default `RtpRewriter`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new `RtpRewriter` with custom parameters.
    pub fn with_params(
        max_cache_size: usize,
        reorder_threshold: u64,
        switch_grace_period: u32,
    ) -> Self {
        Self {
            max_cache_size,
            reorder_threshold,
            switch_grace_period,
            ..Default::default()
        }
    }

    /// Rewrites the sequence number and timestamp of an RTP packet.
    /// Returns the new `SeqNo` and `u32` timestamp for the forwarded packet header.
    ///
    /// # Parameters
    /// - `packet`: The incoming RTP packet
    /// - `is_switch`: True if this packet is part of a keyframe marking a layer switch
    pub fn rewrite(
        &mut self,
        packet: &impl crate::rtp::PacketTiming,
        is_switch: bool,
    ) -> (SeqNo, u32) {
        let incoming_seq = packet.seq_no();
        let incoming_seq_val = *incoming_seq;

        // Check if we've already processed this packet (duplicate)
        if let Some(&(cached_seq, cached_ts)) = self.seen_packets.get(&incoming_seq_val) {
            tracing::trace!(
                incoming_seq = incoming_seq_val,
                "Duplicate packet detected, returning cached rewrite"
            );
            return (cached_seq, cached_ts);
        }

        // Decrement grace period if active
        if self.switch_grace_remaining > 0 {
            self.switch_grace_remaining -= 1;
        }

        // Determine if this packet triggers a reset
        let needs_reset = if is_switch {
            // Explicit switch signal - reset and start grace period
            tracing::debug!(
                incoming_seq = incoming_seq_val,
                "Layer switch signaled via is_switch flag"
            );
            self.switch_grace_remaining = self.switch_grace_period;
            true
        } else if self.switch_grace_remaining > 0 {
            // We're in grace period after a switch - be lenient with reordering
            false
        } else {
            // Normal heuristic-based detection
            self.should_reset(incoming_seq)
        };

        if needs_reset {
            self.reset(packet);
        }

        // Apply the calculated offsets
        let new_forwarded_val = (incoming_seq_val as i64 + self.seq_no_offset) as u64;
        let new_seq_no = SeqNo::from(new_forwarded_val);

        let new_timestamp_numer = packet.rtp_timestamp().numer() as i64 + self.timestamp_offset;
        let new_timestamp = new_timestamp_numer as u32;

        // Update highest rewritten if this is the highest we've assigned
        if let Some(highest_rew) = self.highest_rewritten_seq {
            if seq_is_newer(new_seq_no, highest_rew) {
                self.highest_rewritten_seq = Some(new_seq_no);
                self.highest_rewritten_ts = Some(MediaTime::new(
                    new_timestamp_numer as u64,
                    packet.rtp_timestamp().frequency(),
                ));
            }
        } else {
            self.highest_rewritten_seq = Some(new_seq_no);
            self.highest_rewritten_ts = Some(MediaTime::new(
                new_timestamp_numer as u64,
                packet.rtp_timestamp().frequency(),
            ));
        }

        // Update highest incoming only if this packet is actually newer in source space
        if let Some(highest_inc) = self.highest_incoming_seq {
            if seq_is_newer(incoming_seq, highest_inc) {
                self.highest_incoming_seq = Some(incoming_seq);
                self.highest_incoming_ts = Some(packet.rtp_timestamp());
            }
        } else {
            self.highest_incoming_seq = Some(incoming_seq);
            self.highest_incoming_ts = Some(packet.rtp_timestamp());
        }

        // Cache this packet's rewrite
        self.cache_packet(incoming_seq_val, new_seq_no, new_timestamp);

        (new_seq_no, new_timestamp)
    }

    /// Determines if a heuristic reset is needed (only used outside of switch).
    fn should_reset(&self, incoming_seq: SeqNo) -> bool {
        if self.highest_rewritten_seq.is_none() {
            return true;
        }

        let highest = match self.highest_incoming_seq {
            Some(h) => h,
            None => return true,
        };

        let is_newer = seq_is_newer(incoming_seq, highest);

        if is_newer {
            let forward_distance = seq_distance(highest, incoming_seq);
            if forward_distance > self.reorder_threshold {
                tracing::debug!(
                    incoming_seq = *incoming_seq,
                    highest_seq = *highest,
                    "Heuristic reset: large forward gap"
                );
                return true;
            }
        } else {
            let backward_distance = seq_distance(incoming_seq, highest);
            if backward_distance > self.reorder_threshold {
                tracing::debug!(
                    incoming_seq = *incoming_seq,
                    highest_seq = *highest,
                    "Heuristic reset: far in past"
                );
                return true;
            }
        }

        false
    }

    /// Resets the rewriter's state based on a new packet.
    fn reset(&mut self, packet: &impl crate::rtp::PacketTiming) {
        let target_seq = self
            .highest_rewritten_seq
            .map_or_else(|| *packet.seq_no(), |s| *s + 1);
        self.seq_no_offset = (target_seq as i64) - (*packet.seq_no() as i64);

        let new_incoming_ts = packet.rtp_timestamp();

        let target_ts = if let (Some(last_rew_ts), Some(last_in_ts)) =
            (self.highest_rewritten_ts, self.highest_incoming_ts)
        {
            let duration = new_incoming_ts.saturating_sub(last_in_ts);
            last_rew_ts + duration
        } else {
            new_incoming_ts
        };

        let target_ts_rebased = target_ts.rebase(new_incoming_ts.frequency());
        self.timestamp_offset =
            (target_ts_rebased.numer() as i64) - (new_incoming_ts.numer() as i64);

        self.highest_incoming_seq = Some(packet.seq_no());
        self.highest_incoming_ts = Some(packet.rtp_timestamp());
        self.seen_packets.clear();

        tracing::info!(
            target_seq = target_seq,
            incoming_seq = *packet.seq_no(),
            seq_offset = self.seq_no_offset,
            ts_offset = self.timestamp_offset,
            "RTP rewriter reset"
        );
    }

    fn cache_packet(&mut self, incoming_seq: u64, rewritten_seq: SeqNo, rewritten_ts: u32) {
        self.seen_packets
            .insert(incoming_seq, (rewritten_seq, rewritten_ts));

        while self.seen_packets.len() > self.max_cache_size {
            if let Some(&first_key) = self.seen_packets.keys().next() {
                self.seen_packets.remove(&first_key);
            }
        }
    }

    pub fn clear(&mut self) {
        self.seq_no_offset = 0;
        self.timestamp_offset = 0;
        self.highest_rewritten_seq = None;
        self.highest_rewritten_ts = None;
        self.highest_incoming_seq = None;
        self.highest_incoming_ts = None;
        self.switch_grace_remaining = 0;
        self.seen_packets.clear();
    }
}

/// Compares two sequence numbers accounting for rollover.
fn seq_is_newer(a: SeqNo, b: SeqNo) -> bool {
    let a_u16 = (*a & 0xFFFF) as u16;
    let b_u16 = (*b & 0xFFFF) as u16;
    let diff = a_u16.wrapping_sub(b_u16);
    diff > 0 && diff < 32768
}

/// Calculates the forward distance from `a` to `b` in sequence number space.
fn seq_distance(a: SeqNo, b: SeqNo) -> u64 {
    let a_u16 = (*a & 0xFFFF) as u16;
    let b_u16 = (*b & 0xFFFF) as u16;
    let diff = b_u16.wrapping_sub(a_u16);
    if diff < 32768 {
        diff as u64
    } else {
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

        let (s1, t1) = rewriter.rewrite(&p1, false);

        assert_eq!(*s1, 5000);
        assert_eq!(t1, 1000);
        assert_eq!(rewriter.seq_no_offset, 0);
        assert_eq!(rewriter.timestamp_offset, 0);
    }

    #[test]
    fn rewrite_maintains_continuity() {
        let mut rewriter = RtpRewriter::new();
        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(1000));
        let (s1, t1) = rewriter.rewrite(&p1, false);

        let p2 = TestPacket::new(101.into(), MediaTime::from_90khz(1030));
        let (s2, t2) = rewriter.rewrite(&p2, false);

        assert_eq!(*s2, *s1 + 1);
        assert_eq!(t2, t1 + 30);
    }

    #[test]
    fn rewrite_handles_out_of_order_packets() {
        let mut rewriter = RtpRewriter::new();

        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(90000));
        let (s1, t1) = rewriter.rewrite(&p1, false);

        let p2 = TestPacket::new(101.into(), MediaTime::from_90khz(92700));
        let (s2, t2) = rewriter.rewrite(&p2, false);

        let p4 = TestPacket::new(103.into(), MediaTime::from_90khz(98100));
        let (s4, t4) = rewriter.rewrite(&p4, false);

        // Now packet 102 arrives late
        let p3 = TestPacket::new(102.into(), MediaTime::from_90khz(95400));
        let (s3, t3) = rewriter.rewrite(&p3, false);

        // Should maintain original offsets, not trigger reset
        assert_eq!(*s3, *s2 + 1);
        assert_eq!(t3, t2 + 2700);
        assert_eq!(*s4, *s3 + 1);
    }

    #[test]
    fn rewrite_handles_signaled_switch_with_grace_period() {
        let mut rewriter = RtpRewriter::new();

        // Normal packets from layer 1
        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(90000));
        let (s1, _) = rewriter.rewrite(&p1, false);

        let p2 = TestPacket::new(101.into(), MediaTime::from_90khz(92700));
        let (s2, _) = rewriter.rewrite(&p2, false);

        // Layer switch: First packet from new layer with is_switch=true
        let p5000 = TestPacket::new(5000.into(), MediaTime::from_90khz(95400));
        let (s5000, _) = rewriter.rewrite(&p5000, true);
        assert_eq!(*s5000, *s2 + 1);

        // Out-of-order packets from the new layer arrive
        // Thanks to grace period, these won't trigger another reset
        let p5003 = TestPacket::new(5003.into(), MediaTime::from_90khz(103500));
        let (s5003, _) = rewriter.rewrite(&p5003, false);

        let p5001 = TestPacket::new(5001.into(), MediaTime::from_90khz(98100));
        let (s5001, _) = rewriter.rewrite(&p5001, false);

        let p5002 = TestPacket::new(5002.into(), MediaTime::from_90khz(100800));
        let (s5002, _) = rewriter.rewrite(&p5002, false);

        // All should maintain the same offset despite arriving out of order
        assert_eq!(*s5001, *s5000 + 1);
        assert_eq!(*s5002, *s5000 + 2);
        assert_eq!(*s5003, *s5000 + 3);
    }

    #[test]
    fn rewrite_grace_period_prevents_false_reset() {
        let mut rewriter = RtpRewriter::with_params(200, 100, 10);

        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(90000));
        rewriter.rewrite(&p1, false);

        let p2 = TestPacket::new(101.into(), MediaTime::from_90khz(92700));
        let (s2, _) = rewriter.rewrite(&p2, false);

        // Signaled switch with keyframe
        let p5000 = TestPacket::new(5000.into(), MediaTime::from_90khz(95400));
        let (s5000, _) = rewriter.rewrite(&p5000, true);
        assert_eq!(*s5000, *s2 + 1);

        // A packet arrives that's far ahead - normally would trigger reset
        // but grace period prevents it
        let p5150 = TestPacket::new(5150.into(), MediaTime::from_90khz(125400));
        let (s5150, _) = rewriter.rewrite(&p5150, false);

        // Should maintain continuity thanks to grace period
        assert_eq!(*s5150, *s5000 + 150);
    }

    #[test]
    fn rewrite_handles_duplicate_packets() {
        let mut rewriter = RtpRewriter::new();

        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(90000));
        let (s1, t1) = rewriter.rewrite(&p1, false);

        let (s1_dup, t1_dup) = rewriter.rewrite(&p1, false);

        assert_eq!(*s1_dup, *s1);
        assert_eq!(t1_dup, t1);
    }

    #[test]
    fn rewrite_heuristic_switch_still_works() {
        let mut rewriter = RtpRewriter::new();

        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(90000));
        rewriter.rewrite(&p1, false);

        let p2 = TestPacket::new(101.into(), MediaTime::from_90khz(92700));
        let (s2, t2) = rewriter.rewrite(&p2, false);

        // Large gap without is_switch flag - heuristic should catch it
        let p3_switch = TestPacket::new(8000.into(), MediaTime::from_90khz(95400));
        let (s3, t3) = rewriter.rewrite(&p3_switch, false);

        assert_eq!(*s3, *s2 + 1);
        let expected_t3 = t2.wrapping_add(2700);
        assert_eq!(t3, expected_t3);
    }

    #[test]
    fn test_seq_is_newer() {
        assert!(seq_is_newer(101.into(), 100.into()));
        assert!(!seq_is_newer(100.into(), 101.into()));
        assert!(!seq_is_newer(100.into(), 100.into()));

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
