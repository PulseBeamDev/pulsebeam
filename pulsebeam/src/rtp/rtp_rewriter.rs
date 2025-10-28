use crate::rtp::PacketTiming;
use str0m::media::MediaTime;
use str0m::rtp::SeqNo;
use tokio::time::{Duration, Instant};

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
            highest_ts_out: MediaTime::from_90khz(0), // Default, will be updated by first packet
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
        // First, calculate the offsets for the new stream if this is a switch point.
        if is_switch || !self.initialized {
            self.calculate_new_stream_offsets(packet);
        }

        // --- Stream Validation ---
        // Drop any packet that is clearly from a previous stream.
        const MAX_REORDERING_WINDOW: u64 = 8192; // Generous window for reordering
        if *packet.seq_no() < *self.stream_start_seq_in {
            let distance = self.stream_start_seq_in.wrapping_sub(*packet.seq_no());
            if distance > MAX_REORDERING_WINDOW {
                return None;
            }
        }

        // Apply the pre-calculated offsets for the current stream.
        let rewritten_seq = packet.seq_no().wrapping_add(self.seq_no_offset);
        let ts = packet.rtp_timestamp();
        let rewritten_ts = MediaTime::new(
            ts.numer().wrapping_add(self.timestamp_offset),
            ts.frequency(),
        );

        // --- State Update ---
        // This is the single source of truth for updating the high-water marks.
        let rewritten_seq_no: SeqNo = rewritten_seq.into();
        // The first packet always sets the initial state. Subsequent packets only
        // update it if they are newer than what we've already seen.
        if !self.initialized || rewritten_seq_no > self.highest_seq_out {
            self.highest_seq_out = rewritten_seq_no;
            self.highest_ts_out = rewritten_ts;
            self.last_forward_time = Instant::now();
        }

        // After the first packet, we are officially initialized.
        self.initialized = true;

        Some((rewritten_seq_no, rewritten_ts))
    }

    /// Calculates new offsets for a new stream, anchored on the given packet.
    /// This method does NOT modify the high-water mark state.
    fn calculate_new_stream_offsets(&mut self, packet: &impl PacketTiming) {
        let now = Instant::now();
        let incoming_seq = packet.seq_no();
        let incoming_ts = packet.rtp_timestamp();

        let target_seq = if self.initialized {
            self.highest_seq_out.wrapping_add(1)
        } else {
            *incoming_seq
        };
        self.seq_no_offset = target_seq.wrapping_sub(*incoming_seq);

        let target_ts = if self.initialized {
            let time_since_last = now.saturating_duration_since(self.last_forward_time);
            // Dynamically get the clock rate from the packet itself.
            let clock_rate = packet.rtp_timestamp().frequency().get() as u64;
            let time_gap_ticks = duration_to_ticks(time_since_last, clock_rate);
            self.highest_ts_out
                .rebase(incoming_ts.frequency())
                .numer()
                .wrapping_add(time_gap_ticks)
        } else {
            incoming_ts.numer()
        };
        self.timestamp_offset = target_ts.wrapping_sub(incoming_ts.numer());

        self.stream_start_seq_in = incoming_seq;
    }
}

/// Converts a `Duration` into a number of ticks for a given clock rate.
fn duration_to_ticks(duration: Duration, clock_rate: u64) -> u64 {
    (duration.as_nanos() as u64 * clock_rate) / 1_000_000_000
}

#[cfg(test)]
mod test {
    use str0m::{media::MediaTime, rtp::SeqNo};
    use tokio::time::Duration;

    use super::*;
    use crate::rtp::TimingHeader;

    #[test]
    fn test_continuous_stream_after_switch() {
        let mut rewriter = RtpRewriter::new();
        rewriter
            .rewrite(
                &TimingHeader::new(SeqNo::from(300), MediaTime::from_90khz(3_000)),
                false,
            )
            .unwrap();

        std::thread::sleep(Duration::from_millis(10));

        let (seq, ts) = rewriter
            .rewrite(
                &TimingHeader::new(SeqNo::from(1001), MediaTime::from_90khz(90_000)),
                true,
            )
            .unwrap();

        assert_eq!(seq, SeqNo::from(301));
        let expected_min_ts = 3000 + duration_to_ticks(Duration::from_millis(8), 90000);
        let expected_max_ts = 3000 + duration_to_ticks(Duration::from_millis(20), 90000);
        assert!(
            ts.numer() > expected_min_ts && ts.numer() < expected_max_ts,
            "Timestamp {} should reflect ~10ms wall-clock gap from 3000",
            ts.numer()
        );
    }

    #[test]
    fn correctly_models_large_freeze_gap() {
        let mut r = RtpRewriter::new();
        r.rewrite(
            &TimingHeader::new(SeqNo::from(100), MediaTime::from_90khz(10_000)),
            false,
        );

        // Simulate a 500ms delay for the new keyframe by actually sleeping.
        std::thread::sleep(Duration::from_millis(500));

        let switch_packet = TimingHeader::new(SeqNo::from(2000), MediaTime::from_90khz(999_999));
        let (seq_switch, ts_switch) = r.rewrite(&switch_packet, true).unwrap();

        assert_eq!(seq_switch, SeqNo::from(101));

        // The new timestamp should be the old one plus the real-time gap.
        // 500ms = 45,000 ticks. Allow a generous window for test runner variance.
        let expected_min_ts = 10_000 + duration_to_ticks(Duration::from_millis(480), 90000);
        let expected_max_ts = 10_000 + duration_to_ticks(Duration::from_millis(550), 90000);
        let actual_ts = ts_switch.numer();

        assert!(
            actual_ts > expected_min_ts && actual_ts < expected_max_ts,
            "Actual timestamp {} was not in the expected range [{} - {}]",
            actual_ts,
            expected_min_ts,
            expected_max_ts
        );
    }
}
