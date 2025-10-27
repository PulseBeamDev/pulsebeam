use crate::rtp::PacketTiming;
use std::collections::BTreeMap;
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
    seq_no_offset: i64,
    /// The offset applied to incoming timestamps to maintain correct timing.
    timestamp_offset: i64,

    /// The highest sequence number that has been forwarded to the subscriber.
    highest_forwarded_seq: Option<SeqNo>,
    /// The rewritten timestamp of the packet with `highest_forwarded_seq`.
    highest_forwarded_ts: Option<MediaTime>,
    /// The original timestamp of the last processed incoming packet. Used to
    /// calculate timestamp progression across a layer switch.
    last_incoming_ts: Option<MediaTime>,

    /// After a switch, this stores the sequence number of the first packet from
    /// the new stream. Any packet with an earlier sequence number is considered
    /// part of the old stream and is dropped.
    active_stream_start_seq: Option<SeqNo>,

    /// A small cache to detect and ignore duplicate packets.
    /// Maps incoming seq -> (rewritten seq, rewritten timestamp).
    seen_packets: BTreeMap<u64, (SeqNo, u32)>,
    /// The maximum number of packets to retain in the cache.
    max_cache_size: usize,
}

impl Default for RtpRewriter {
    fn default() -> Self {
        Self {
            seq_no_offset: 0,
            timestamp_offset: 0,
            highest_forwarded_seq: None,
            highest_forwarded_ts: None,
            last_incoming_ts: None,
            active_stream_start_seq: None,
            seen_packets: BTreeMap::new(),
            max_cache_size: 100, // Reduced default size, as it's only for duplicates.
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
    /// It returns `Some((SeqNo, u32))` with the new values if the packet should be
    /// forwarded, or `None` if the packet should be dropped (e.g., it's a
    /// lingering packet from an old stream after a switch).
    ///
    /// # Parameters
    /// - `packet`: The incoming RTP packet, providing sequence number and timestamp.
    /// - `is_switch`: Must be `true` if this packet is the first from a new layer,
    ///                signaling a stream switch.
    pub fn rewrite(&mut self, packet: &impl PacketTiming, is_switch: bool) -> Option<(SeqNo, u32)> {
        let incoming_seq = packet.seq_no();

        // 1. Check for and return cached result for duplicate packets.
        if let Some(&cached) = self.seen_packets.get(&*incoming_seq) {
            tracing::trace!(
                seq = *incoming_seq,
                "Duplicate packet, returning cached rewrite"
            );
            return Some(cached);
        }

        // 2. A reset is needed for the very first packet or an explicit switch.
        if self.highest_forwarded_seq.is_none() || is_switch {
            self.reset(packet);
        } else if let Some(start_seq) = self.active_stream_start_seq {
            // 3. After a switch, drop any lingering packets from the old stream.
            // These are identified by having a sequence number older than the
            // first packet of the new stream.
            if is_seq_older(incoming_seq, start_seq) {
                tracing::debug!(
                    seq = *incoming_seq,
                    start = *start_seq,
                    "Dropping lingering packet from old stream"
                );
                return None;
            }
        }

        // 4. Calculate the new sequence number and timestamp using the offsets.
        let new_seq_val = (*incoming_seq as i64 + self.seq_no_offset) as u64;
        let new_seq = SeqNo::from(new_seq_val);

        let new_ts_numer = packet.rtp_timestamp().numer() as i64 + self.timestamp_offset;
        let new_ts = new_ts_numer as u32;

        // 5. Update the state for the next packet.
        self.highest_forwarded_seq = Some(new_seq);
        self.highest_forwarded_ts = Some(MediaTime::new(
            new_ts_numer as u64,
            packet.rtp_timestamp().frequency(),
        ));
        self.last_incoming_ts = Some(packet.rtp_timestamp());

        // 6. Cache the result for duplicate detection.
        self.cache_packet(*incoming_seq, new_seq, new_ts);

        Some((new_seq, new_ts))
    }

    /// Resets the rewriter's state to seamlessly transition to a new stream.
    ///
    /// This is called for the first packet or any packet where `is_switch` is true.
    /// It calculates new offsets to ensure the forwarded sequence number and
    /// timestamp are contiguous with the previous packet, preserving timing.
    fn reset(&mut self, packet: &impl PacketTiming) {
        // The target sequence number should be one greater than the last forwarded one.
        // If this is the first packet ever, we start with its own sequence number.
        let target_seq = self
            .highest_forwarded_seq
            .map_or(*packet.seq_no(), |s| *s + 1);
        self.seq_no_offset = (target_seq as i64) - (*packet.seq_no() as i64);

        let incoming_ts = packet.rtp_timestamp();

        // The target timestamp should preserve the time delta from the last processed packet.
        // This ensures that a/v sync is maintained across layer switches.
        let target_ts = if let (Some(last_fwd_ts), Some(last_inc_ts)) =
            (self.highest_forwarded_ts, self.last_incoming_ts)
        {
            let duration = incoming_ts.saturating_sub(last_inc_ts);
            let last_fwd_ts_rebased = last_fwd_ts.rebase(duration.frequency());
            last_fwd_ts_rebased + duration
        } else {
            // If it's the first packet ever, there's no previous timestamp to reference.
            incoming_ts
        };

        self.timestamp_offset = (target_ts.numer() as i64) - (incoming_ts.numer() as i64);

        // Mark this packet's sequence number as the start of the new active stream.
        self.active_stream_start_seq = Some(packet.seq_no());

        tracing::info!(
            incoming_seq = *packet.seq_no(),
            target_seq = target_seq,
            seq_offset = self.seq_no_offset,
            ts_offset = self.timestamp_offset,
            "RTP rewriter reset for new stream"
        );
    }

    /// Adds a packet's rewrite result to the cache and prunes old entries.
    fn cache_packet(&mut self, incoming_seq: u64, rewritten_seq: SeqNo, rewritten_ts: u32) {
        self.seen_packets
            .insert(incoming_seq, (rewritten_seq, rewritten_ts));

        // Prune the cache if it exceeds the maximum size.
        if self.seen_packets.len() > self.max_cache_size {
            if let Some(&first_key) = self.seen_packets.keys().next() {
                self.seen_packets.remove(&first_key);
            }
        }
    }
}

/// Compares two sequence numbers, accounting for rollover, to see if `a` is older than `b`.
fn is_seq_older(a: SeqNo, b: SeqNo) -> bool {
    let a_u16 = *a as u16;
    let b_u16 = *b as u16;
    let diff = b_u16.wrapping_sub(a_u16);
    // `a` is older if `b` is ahead, within a reasonable window (half of the space).
    // A diff of 0 means they are the same, so not older.
    diff != 0 && diff < 32768
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp::test::TestPacket;

    #[test]
    fn handles_initial_packet() {
        let mut rewriter = RtpRewriter::new();
        let p1 = TestPacket::new(5000.into(), MediaTime::from_90khz(1000));

        let (s1, t1) = rewriter.rewrite(&p1, false).unwrap();

        assert_eq!(*s1, 5000);
        assert_eq!(t1, 1000);
        assert_eq!(rewriter.seq_no_offset, 0);
        assert_eq!(rewriter.timestamp_offset, 0);
    }

    #[test]
    fn maintains_continuity() {
        let mut rewriter = RtpRewriter::new();
        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(1000));
        let (s1, t1) = rewriter.rewrite(&p1, false).unwrap();

        let p2 = TestPacket::new(101.into(), MediaTime::from_90khz(1030));
        let (s2, t2) = rewriter.rewrite(&p2, false).unwrap();

        assert_eq!(*s2, *s1 + 1);
        assert_eq!(t2, t1 + 30);
    }

    #[test]
    fn handles_signaled_switch() {
        let mut rewriter = RtpRewriter::new();

        // Packets from the first layer
        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(90000));
        let (s1, t1) = rewriter.rewrite(&p1, false).unwrap();
        let p2 = TestPacket::new(101.into(), MediaTime::from_90khz(92700));
        let (s2, t2) = rewriter.rewrite(&p2, false).unwrap();

        assert_eq!(*s2, *s1 + 1);
        assert_eq!(t2, t1 + 2700);

        // A switch occurs. The new packet has a completely different sequence number.
        // `is_switch` is true.
        let p_switch = TestPacket::new(5000.into(), MediaTime::from_90khz(95400));
        let (s_switch, t_switch) = rewriter.rewrite(&p_switch, true).unwrap();

        // The rewritten sequence number must be contiguous.
        assert_eq!(*s_switch, *s2 + 1);
        // The rewritten timestamp must preserve the duration from the previous packet.
        let expected_t_switch = t2 + (95400 - 92700);
        assert_eq!(t_switch, expected_t_switch);

        // The next packet from the new layer should follow the rewritten sequence.
        let p_next = TestPacket::new(5001.into(), MediaTime::from_90khz(98100));
        let (s_next, t_next) = rewriter.rewrite(&p_next, false).unwrap();

        assert_eq!(*s_next, *s_switch + 1);
        let expected_t_next = t_switch + (98100 - 95400);
        assert_eq!(t_next, expected_t_next);
    }

    #[test]
    fn drops_lingering_packets_after_switch() {
        let mut rewriter = RtpRewriter::new();

        // Old layer packets
        let p101 = TestPacket::new(101.into(), MediaTime::from_90khz(92700));
        let (s101, _) = rewriter.rewrite(&p101, false).unwrap();

        // A switch to a new layer occurs
        let p5000 = TestPacket::new(5000.into(), MediaTime::from_90khz(95400));
        let (s5000, _) = rewriter.rewrite(&p5000, true).unwrap();
        assert_eq!(*s5000, *s101 + 1);

        // A packet from the new layer arrives and is processed
        let p5001 = TestPacket::new(5001.into(), MediaTime::from_90khz(98100));
        let (s5001, _) = rewriter.rewrite(&p5001, false).unwrap();
        assert_eq!(*s5001, *s5000 + 1);

        // Now, a late packet from the *old* layer arrives (e.g., seq 102).
        // Since a switch to the new stream (starting at seq 5000) has already
        // occurred, this packet should be dropped.
        let p102_late = TestPacket::new(102.into(), MediaTime::from_90khz(93000));
        let result = rewriter.rewrite(&p102_late, false);
        assert!(
            result.is_none(),
            "Late packet from old stream should be dropped"
        );
    }

    #[test]
    fn handles_duplicate_packets() {
        let mut rewriter = RtpRewriter::new();

        let p1 = TestPacket::new(100.into(), MediaTime::from_90khz(90000));
        let (s1, t1) = rewriter.rewrite(&p1, false).unwrap();

        // Send the same packet again
        let (s1_dup, t1_dup) = rewriter.rewrite(&p1, false).unwrap();

        // The output should be identical, and state should not have advanced.
        assert_eq!(*s1_dup, *s1);
        assert_eq!(t1_dup, t1);
        assert_eq!(rewriter.highest_forwarded_seq, Some(s1));
    }

    #[test]
    fn handles_rollover_in_switch() {
        let mut rewriter = RtpRewriter::new();

        // Packet just before sequence number rollover
        let p1 = TestPacket::new(65535.into(), MediaTime::from_90khz(1000));
        let (s1, _) = rewriter.rewrite(&p1, false).unwrap();
        assert_eq!(*s1, 65535);

        // A switch occurs with a new layer starting at a low sequence number
        let p_switch = TestPacket::new(5.into(), MediaTime::from_90khz(4000));
        let (s_switch, _) = rewriter.rewrite(&p_switch, true).unwrap();

        // The rewritten sequence number must wrap around correctly.
        assert_eq!(*s_switch, 65536); // This will wrap to 0 in a u16
        assert_eq!(*s_switch, *s1 + 1);
    }
}
