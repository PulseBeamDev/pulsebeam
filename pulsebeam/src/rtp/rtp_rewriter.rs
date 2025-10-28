use crate::rtp::PacketTiming;
use str0m::media::MediaTime;
use str0m::rtp::SeqNo;
use tokio::time::{Duration, Instant};

/// Rewrites RTP headers for a single outgoing media stream that is sourced from an
/// upstream that may switch between different quality layers (e.g., simulcast).
///
/// The core responsibility is to ensure the outgoing stream is always perfectly
/// contiguous and correctly timed, making layer switches transparent to the client.
/// It addresses two critical production issues:
///
/// 1.  **Sequence Number Continuity:** A client's jitter buffer expects a monotonic,
///     gapless sequence number. A layer switch can cause a large, non-contiguous jump
///     (e.g., from 101 to 5000), which would be misinterpreted as a massive packet
///     loss event. This rewriter translates the incoming sequence numbers to a
///     perfectly contiguous outgoing sequence.
///
/// 2.  **Timestamp-induced Freezes:** The more subtle and critical problem is maintaining
///     correct timing. When switching layers, there is a non-trivial wall-clock delay
///     (RTT for a keyframe request + encoder delay). If we simply make the new stream's
///     timestamps contiguous with the old one, we create a temporal inconsistency: the
///     client receives a frame with a timestamp indicating it should be played
///     *immediately*, but in reality, 200-500ms have passed. The client's playout
///     buffer runs dry, causing a video freeze.
///
/// To solve this, the rewriter measures the actual wall-clock `Duration` between the
/// last packet of the old stream and the first packet of the new one (using the packet's
/// `arrival_timestamp` for accuracy). This duration is converted into RTP ticks and
/// injected as a gap in the rewritten timestamp sequence, ensuring the output stream
/// accurately reflects the passage of real time.
///
#[derive(Debug)]
pub struct RtpRewriter {
    // === High-Water Marks for the entire outgoing session ===
    /// The highest sequence number ever forwarded.
    highest_seq_out: SeqNo,
    /// The highest timestamp ever forwarded.
    highest_ts_out: MediaTime,
    /// The arrival time of the packet that set the `highest_ts_out`. This is our
    /// high-fidelity anchor for measuring real-time gaps.
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
            self.calculate_new_stream_offsets(packet);
        }

        const MAX_REORDERING_WINDOW: u64 = 8192;
        if *packet.seq_no() < *self.stream_start_seq_in {
            let distance = self.stream_start_seq_in.wrapping_sub(*packet.seq_no());
            if distance > MAX_REORDERING_WINDOW {
                return None;
            }
        }

        let rewritten_seq = packet.seq_no().wrapping_add(self.seq_no_offset);
        let ts = packet.rtp_timestamp();
        let rewritten_ts = MediaTime::new(
            ts.numer().wrapping_add(self.timestamp_offset),
            ts.frequency(),
        );

        let rewritten_seq_no: SeqNo = rewritten_seq.into();
        if !self.initialized || rewritten_seq_no > self.highest_seq_out {
            self.highest_seq_out = rewritten_seq_no;
            self.highest_ts_out = rewritten_ts;
            self.last_forward_time = packet.arrival_timestamp();
        }

        self.initialized = true;

        Some((rewritten_seq_no, rewritten_ts))
    }

    fn calculate_new_stream_offsets(&mut self, packet: &impl PacketTiming) {
        let now = packet.arrival_timestamp();
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

fn duration_to_ticks(duration: Duration, clock_rate: u64) -> u64 {
    (duration.as_nanos() * clock_rate as u128 / 1_000_000_000) as u64
}

#[cfg(test)]
mod test {
    use str0m::{media::MediaTime, rtp::SeqNo};
    use tokio::time::{Duration, Instant};

    use crate::rtp::TimingHeader;

    use super::*;

    #[test]
    fn test_continuous_stream_after_switch() {
        let mut rewriter = RtpRewriter::new();
        let start_time = Instant::now();

        let p1 = TimingHeader::new(SeqNo::from(300), MediaTime::from_90khz(3_000), start_time);
        rewriter.rewrite(&p1, false).unwrap();

        // Simulate a 10ms delay for the switch packet's arrival.
        let switch_time = start_time + Duration::from_millis(10);
        let p2 = TimingHeader::new(
            SeqNo::from(1001),
            MediaTime::from_90khz(90_000),
            switch_time,
        );
        let (seq, ts) = rewriter.rewrite(&p2, true).unwrap();

        assert_eq!(seq, SeqNo::from(301));
        let expected_ts = 3000 + duration_to_ticks(Duration::from_millis(10), 90000);
        assert_eq!(ts.numer(), expected_ts);
    }

    #[test]
    fn correctly_models_large_freeze_gap() {
        let mut r = RtpRewriter::new();
        let start_time = Instant::now();
        let p1 = TimingHeader::new(SeqNo::from(100), MediaTime::from_90khz(10_000), start_time);
        r.rewrite(&p1, false);

        // Simulate a 500ms delay for the new keyframe by creating an Instant in the future.
        let switch_arrival_time = start_time + Duration::from_millis(500);
        let switch_packet = TimingHeader::new(
            SeqNo::from(2000),
            MediaTime::from_90khz(999_999),
            switch_arrival_time,
        );
        let (seq_switch, ts_switch) = r.rewrite(&switch_packet, true).unwrap();

        assert_eq!(seq_switch, SeqNo::from(101));

        // The new timestamp should be the old one plus the exact 500ms real-time gap.
        let expected_ts = 10_000 + duration_to_ticks(Duration::from_millis(500), 90000);
        let actual_ts = ts_switch.numer();

        assert_eq!(actual_ts, expected_ts);
    }
}
