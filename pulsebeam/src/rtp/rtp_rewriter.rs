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
    seen_packets: BTreeMap<u64, (SeqNo, MediaTime)>,
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
            max_cache_size: 100,
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
        let new_ts = MediaTime::new(new_ts_numer as u64, packet.rtp_timestamp().frequency());

        // 5. Update the state for the next packet.
        self.highest_forwarded_seq = Some(new_seq);
        self.highest_forwarded_ts = Some(new_ts);
        self.last_incoming_ts = Some(packet.rtp_timestamp());

        // 6. Cache the result for duplicate detection.
        self.cache_packet(*incoming_seq, new_seq, new_ts);

        Some((new_seq, new_ts))
    }

    /// Resets the rewriter's state to seamlessly transition to a new stream.
    fn reset(&mut self, packet: &impl PacketTiming) {
        let target_seq = self
            .highest_forwarded_seq
            .map_or(*packet.seq_no(), |s| *s + 1);
        self.seq_no_offset = (target_seq as i64) - (*packet.seq_no() as i64);

        let incoming_ts = packet.rtp_timestamp();

        let target_ts = if let (Some(last_fwd_ts), Some(last_inc_ts)) =
            (self.highest_forwarded_ts, self.last_incoming_ts)
        {
            let duration = incoming_ts.saturating_sub(last_inc_ts);
            let last_fwd_ts_rebased = last_fwd_ts.rebase(duration.frequency());
            last_fwd_ts_rebased + duration
        } else {
            incoming_ts
        };

        self.timestamp_offset = (target_ts.numer() as i64) - (incoming_ts.numer() as i64);

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
    fn cache_packet(&mut self, incoming_seq: u64, rewritten_seq: SeqNo, rewritten_ts: MediaTime) {
        self.seen_packets
            .insert(incoming_seq, (rewritten_seq, rewritten_ts));

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
    diff != 0 && diff < 32768
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp::TimingHeader;

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
