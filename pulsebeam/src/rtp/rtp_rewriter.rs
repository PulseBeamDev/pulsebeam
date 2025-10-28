use crate::rtp::PacketTiming;
use str0m::media::MediaTime;
use str0m::rtp::SeqNo;
use tokio::time::{Duration, Instant};

const VIDEO_CLOCK_RATE: u64 = 90_000;

/// Manages RTP header rewriting for an immediate, clean, and contiguous stream.
///
/// This implementation prioritizes immediate and smooth layer switching by ensuring the
/// rewritten timestamps reflect the passage of real wall-clock time. This prevents the
/// "big freeze" artifact where a player stalls waiting for a new frame.
#[derive(Debug)]
pub struct RtpRewriter {
    // === High-Water Marks for the entire outgoing session ===
    /// The highest sequence number ever forwarded.
    highest_seq_out: SeqNo,
    /// The highest timestamp ever forwarded.
    highest_ts_out: MediaTime,
    /// The wall-clock time when the packet with `highest_ts_out` was forwarded.
    last_forward_time: Instant,

    // === Per-Stream State (updated on each switch) ===
    /// The offset to apply to incoming sequence numbers for the current stream.
    seq_no_offset: u64,
    /// The offset to apply to incoming timestamps for the current stream.
    timestamp_offset: u64,
    /// The incoming sequence number that started the current stream. Used to reject stale packets.
    stream_start_seq_in: SeqNo,

    initialized: bool,
}

impl Default for RtpRewriter {
    fn default() -> Self {
        Self {
            highest_seq_out: SeqNo::from(0),
            highest_ts_out: MediaTime::from_90khz(0),
            last_forward_time: Instant::now(),
            seq_no_offset: 0,
            timestamp_offset: 0,
            stream_start_seq_in: SeqNo::from(0),
            initialized: false,
        }
    }
}

impl RtpRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rewrite(
        &mut self,
        packet: &impl PacketTiming,
        is_switch: bool,
    ) -> Option<(SeqNo, MediaTime)> {
        if is_switch || !self.initialized {
            self.start_new_stream(packet);
        }

        // --- Stream Validation ---
        // Drop any packet that is clearly from a previous stream.
        const MAX_REORDERING_WINDOW: u64 = 8192; // Generous window for reordering
        if *packet.seq_no() < *self.stream_start_seq_in {
            let distance = self.stream_start_seq_in.wrapping_sub(*packet.seq_no());
            if distance > MAX_REORDERING_WINDOW {
                tracing::warn!(
                    incoming_seq = *packet.seq_no(),
                    current_stream_start = *self.stream_start_seq_in,
                    "Dropping stale packet from a previous stream.",
                );
                return None;
            }
        }
        // --- End Validation ---

        // Apply the pre-calculated offsets for the current stream.
        let rewritten_seq = packet.seq_no().wrapping_add(self.seq_no_offset);

        let ts = packet.rtp_timestamp();
        let rewritten_ts = MediaTime::new(
            ts.numer().wrapping_add(self.timestamp_offset),
            ts.frequency(),
        );

        // Update the global high-water marks.
        let rewritten_seq_no: SeqNo = rewritten_seq.into();
        if rewritten_seq_no > self.highest_seq_out {
            self.highest_seq_out = rewritten_seq_no;
            self.highest_ts_out = rewritten_ts;
            self.last_forward_time = Instant::now();
        }

        Some((rewritten_seq_no, rewritten_ts))
    }

    /// Resets the offsets and markers for a new stream, anchored on the given packet.
    fn start_new_stream(&mut self, packet: &impl PacketTiming) {
        let now = Instant::now();
        let incoming_seq = packet.seq_no();
        let incoming_ts = packet.rtp_timestamp();

        // The target sequence number must be perfectly contiguous.
        let target_seq = if self.initialized {
            self.highest_seq_out.wrapping_add(1)
        } else {
            *incoming_seq
        };
        self.seq_no_offset = target_seq.wrapping_sub(*incoming_seq);

        // --- Real-Time Timestamp Logic ---
        // This is the critical part to prevent freezes. The timestamp for the new stream
        // must account for the real wall-clock time that has passed since the last frame.
        let target_ts = if self.initialized {
            let time_since_last = now.saturating_duration_since(self.last_forward_time);
            let time_gap_ticks = duration_to_ticks(time_since_last, VIDEO_CLOCK_RATE);

            // The new timestamp is the last one plus the real-time gap.
            self.highest_ts_out
                .rebase(incoming_ts.frequency())
                .numer()
                .wrapping_add(time_gap_ticks)
        } else {
            incoming_ts.numer()
        };
        self.timestamp_offset = target_ts.wrapping_sub(incoming_ts.numer());
        // --- End Real-Time Logic ---

        // Mark the beginning of this new stream to reject stale packets.
        self.stream_start_seq_in = incoming_seq;
        self.initialized = true;

        // The switch packet itself becomes the new high-water mark.
        let rewritten_seq_no: SeqNo = incoming_seq.wrapping_add(self.seq_no_offset).into();
        self.highest_seq_out = rewritten_seq_no;
        self.highest_ts_out = MediaTime::new(target_ts, incoming_ts.frequency());
        self.last_forward_time = now;

        tracing::info!(
            incoming_seq = *incoming_seq,
            outgoing_seq = *rewritten_seq_no,
            "RTP rewriter switched to new stream."
        );
    }
}

/// Converts a `Duration` into a number of ticks for a given clock rate.
fn duration_to_ticks(duration: Duration, clock_rate: u64) -> u64 {
    let total_nanos = duration.as_nanos() as u64;
    // Perform calculation with high precision to avoid truncation errors.
    (total_nanos * clock_rate) / 1_000_000_000
}

#[cfg(test)]
mod test {
    use str0m::{media::MediaTime, rtp::SeqNo};
    use tokio::time::{Duration, Instant};

    use super::*;
    use crate::rtp::TimingHeader;

    #[test]
    fn test_continuous_stream_after_switch() {
        let mut rewriter = RtpRewriter::new();
        let p1 = TimingHeader::new(SeqNo::from(300), MediaTime::from_90khz(3_000));
        rewriter.rewrite(&p1, false).unwrap();

        // Simulate a small delay for the switch
        std::thread::sleep(Duration::from_millis(10));

        let p2 = TimingHeader::new(SeqNo::from(1001), MediaTime::from_90khz(90_000));
        let (seq, ts) = rewriter.rewrite(&p2, true).unwrap();

        assert_eq!(seq, SeqNo::from(301));
        // Timestamp should be ~10ms (900 ticks) after the last one.
        assert!(
            ts.numer() > 3000 + 800 && ts.numer() < 3000 + 1200,
            "Timestamp should reflect wall-clock gap"
        );

        let p3 = TimingHeader::new(SeqNo::from(1002), MediaTime::from_90khz(93_000)); // 3000 ts delta
        let (seq3, ts3) = rewriter.rewrite(&p3, false).unwrap();
        assert_eq!(seq3, SeqNo::from(302));
        assert_eq!(
            ts3.numer() - ts.numer(),
            3000,
            "Should preserve internal delta"
        );
    }

    #[test]
    fn correctly_drops_stale_packet_after_switch() {
        let mut r = RtpRewriter::new();
        r.rewrite(
            &TimingHeader::new(SeqNo::from(100), MediaTime::from_90khz(1000)),
            false,
        );
        r.rewrite(
            &TimingHeader::new(SeqNo::from(101), MediaTime::from_90khz(2000)),
            false,
        );

        r.rewrite(
            &TimingHeader::new(SeqNo::from(50000), MediaTime::from_90khz(50000)),
            true,
        );
        assert_eq!(r.stream_start_seq_in, SeqNo::from(50000));

        let stale_packet = TimingHeader::new(SeqNo::from(102), MediaTime::from_90khz(3000));
        assert!(r.rewrite(&stale_packet, false).is_none());

        let next_b_packet = TimingHeader::new(SeqNo::from(50001), MediaTime::from_90khz(51000));
        let out = r.rewrite(&next_b_packet, false).unwrap();
        assert_eq!(out.0, SeqNo::from(103));
    }

    #[test]
    fn correctly_models_large_freeze_gap() {
        let mut r = RtpRewriter::new();
        let start_time = Instant::now();
        r.last_forward_time = start_time;

        r.rewrite(
            &TimingHeader::new(SeqNo::from(100), MediaTime::from_90khz(10_000)),
            false,
        );
        assert_eq!(r.highest_ts_out, MediaTime::from_90khz(10_000));

        // Simulate a 500ms delay for the new keyframe.
        let switch_time = start_time + Duration::from_millis(500);
        r.last_forward_time = switch_time; // Manually set for test predictability

        // The incoming timestamp doesn't matter for the gap, only the wall-clock does.
        let switch_packet = TimingHeader::new(SeqNo::from(2000), MediaTime::from_90khz(999_999));
        let (seq_switch, ts_switch) = r.rewrite(&switch_packet, true).unwrap();

        assert_eq!(seq_switch, SeqNo::from(101));

        let expected_gap_ticks = 90_000 / 2; // 500ms = 45_000 ticks
        let actual_ts = ts_switch.numer();
        let expected_ts = r.highest_ts_out.numer();

        // The new timestamp should be the old one plus the real-time gap.
        assert!(actual_ts >= expected_ts);
        assert!(ts_switch.numer() > 10_000 + 44_000 && ts_switch.numer() < 10_000 + 46_000);
    }
}
