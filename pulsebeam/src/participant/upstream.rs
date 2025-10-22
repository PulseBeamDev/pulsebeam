use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::Instant;

// --- Constants ---

/// The smoothing factor for the jitter calculation, as recommended by RFC 3550, Appendix A.8.
/// A value of 1/16 is used to average out the jitter estimate over time.
const JITTER_SMOOTHING_FACTOR: f64 = 1.0 / 16.0;
/// Clamps the jitter estimate to a maximum value in seconds. This prevents a single,
/// large network spike from inflating the playout delay excessively.
const MAX_JITTER_SECONDS: f64 = 0.25;
/// Half the range of a 16-bit sequence number. Used for RFC 1982-style comparisons
/// to correctly handle wrap-around.
const SEQ_NUM_HALF_RANGE: u16 = 0x8000;

// --- Design Philosophy ---

/// A jitter buffer for a WebRTC SFU.
///
/// ## Why this Algorithm for an SFU?
///
/// An SFU's jitter buffer has a different job than a client's. It's a **"stream cleaner"**
/// that sits in the middle of a call. Its goal is to absorb upstream network chaos
/// (jitter, loss, reordering) from a publisher and forward a clean, well-paced stream
/// to all subscribers.
///
/// This algorithm is designed to balance three competing goals critical for an SFU:
///
/// 1.  **Low Latency:** In interactive conversation, delay is the most critical metric. The
///     SFU must add as little latency as possible. This algorithm is adaptive and
///     prioritizes keeping the buffering delay to a minimum.
///
/// 2.  **Robustness:** The internet is unreliable. The buffer must gracefully handle
///     packet loss and reordering to prevent stream stalls, which affect all subscribers.
///
/// 3.  **Server Efficiency:** This code runs for every single stream on the server. It
///     must be computationally cheap and have a predictable, bounded memory footprint
///     to allow the SFU to scale to thousands of streams.
///
/// This implementation is a pragmatic choice that favors simplicity and efficiency over
/// more complex academic solutions, making it a robust workhorse for a production system.
pub struct JitterBuffer<T: MediaPacket> {
    config: JitterBufferConfig,
    /// Stores packets ordered by their RTP sequence number.
    ///
    /// A `BTreeMap` is chosen for its simplicity and automatic sorting of keys. This
    /// handles packet reordering automatically. For the small number of packets
    /// typically held in a buffer, its `O(log N)` performance is more than sufficient
    /// and avoids the complexity of a custom ring buffer implementation.
    buffer: BTreeMap<u64, T>,
    /// The sequence number of the last packet released by `poll`.
    last_playout_seq: Option<u64>,
    /// The RTP timestamp of the last packet released by `poll`.
    last_playout_rtp_ts: Option<u32>,
    /// The adaptive target time for the next packet to be released. This is the core
    /// of the buffer's scheduling logic.
    playout_time: Instant,
    /// A running average of network jitter, calculated in seconds. This is the key
    /// input for the adaptive delay calculation.
    jitter_estimate: f64,
    /// The wall-clock time the last packet was received. Used for silence detection.
    last_arrival_time: Option<Instant>,
    /// The RTP timestamp of the last packet received. Used for jitter calculation.
    last_received_rtp_ts: Option<u32>,
}

/// Defines the contract for media packets stored in the JitterBuffer.
pub trait MediaPacket {
    fn sequence_number(&self) -> u64;
    fn rtp_timestamp(&self) -> u32;
    fn arrival_timestamp(&self) -> Instant;
}

/// The result of polling the jitter buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum PollResult<T> {
    PacketReady(T),
    WaitUntil(Instant),
    Empty,
}

/// Configuration for the JitterBuffer.
///
/// These parameters are the primary knobs for tuning the buffer's behavior, balancing
/// the core trade-offs between latency, robustness, and resource usage.
#[derive(Debug, Clone, Copy)]
pub struct JitterBufferConfig {
    /// The clock rate of the media source (e.g., 90000 for video, 48000 for Opus audio).
    pub clock_rate: u32,
    /// Max number of packets to hold. This is a critical safety valve for server
    /// efficiency, preventing a single stream from using unbounded memory (OOM).
    pub max_capacity: usize,
    /// A fixed minimum latency. Helps absorb small, consistent network delays without
    /// waiting for the adaptive estimator to react, contributing to a stable base latency.
    pub base_latency: Duration,
    /// The main tuning knob for the latency-vs-robustness trade-off. A higher value
    /// makes the buffer more conservative, adding more delay to handle high jitter at the
    /// cost of increased latency. A lower value prioritizes latency but risks more glitches
    /// on a choppy network.
    pub jitter_multiplier: f64,
    /// Another key latency-vs-robustness trade-off. This is how long we wait for a
    /// missing packet before giving up. A shorter duration minimizes stall time (low latency),
    /// while a longer duration can ride out short bursts of packet loss (robustness).
    pub loss_patience: Duration,
    /// If no packets are received for this duration, the buffer resets its state. This
    /// makes the buffer robust to changing network conditions. For example, if a user's
    /// connection changes during a long pause, this ensures the old (and now incorrect)
    /// jitter statistics are discarded.
    pub silence_threshold: Duration,
}

impl Default for JitterBufferConfig {
    fn default() -> Self {
        Self {
            clock_rate: 90000,
            max_capacity: 512,
            base_latency: Duration::from_millis(20),
            jitter_multiplier: 4.0,
            loss_patience: Duration::from_millis(40),
            silence_threshold: Duration::from_secs(2),
        }
    }
}

impl<T: MediaPacket> JitterBuffer<T> {
    pub fn new(config: JitterBufferConfig) -> Self {
        tracing::info!(?config, "jitter buffer initialized");
        Self {
            config,
            buffer: BTreeMap::new(),
            last_playout_seq: None,
            last_playout_rtp_ts: None,
            playout_time: Instant::now(),
            jitter_estimate: 0.0,
            last_arrival_time: None,
            last_received_rtp_ts: None,
        }
    }

    /// Adds a packet to the buffer.
    pub fn push(&mut self, packet: T) {
        let seq = packet.sequence_number();
        let arrival = packet.arrival_timestamp();
        tracing::trace!(sequence = seq, "packet pushed");

        if let Some(last_seq) = self.last_playout_seq {
            if !Self::is_seq_newer_than(last_seq, seq) {
                tracing::debug!(sequence = seq, last_seq, "dropping old/duplicate packet");
                metrics::counter!("jitterbuffer_dropped_packets_total").increment(1);
                return;
            }
        }

        // The silence detection check makes the buffer robust to changing network conditions.
        if let Some(last_arrival) = self.last_arrival_time {
            if arrival.saturating_duration_since(last_arrival) > self.config.silence_threshold {
                tracing::warn!(threshold = ?self.config.silence_threshold, "silence detected, resetting jitter buffer");
                metrics::counter!("jitterbuffer_resets_total").increment(1);
                self.reset_state();
            }
        }

        self.update_jitter(&packet);

        if self.buffer.is_empty() && self.last_playout_seq.is_none() {
            self.playout_time = arrival + self.current_delay();
            tracing::debug!(playout_time = ?self.playout_time, "initial playout time established");
        }

        self.buffer.insert(seq, packet);
        metrics::histogram!("jitterbuffer_buffer_occupancy_packets")
            .record(self.buffer.len() as f64);

        // Enforcing max_capacity is essential for server efficiency and stability.
        if self.buffer.len() > self.config.max_capacity {
            if let Some(first_key) = self.buffer.keys().next().copied() {
                self.buffer.remove(&first_key);
                tracing::warn!(seq = first_key, "buffer overflow, dropped oldest packet");
                metrics::counter!("jitterbuffer_dropped_packets_total").increment(1);
            }
        }
    }

    /// Attempts to retrieve the next packet for playout.
    pub fn poll(&mut self, now: Instant) -> PollResult<T> {
        let next_seq_to_play = self.last_playout_seq.map_or_else(
            || self.buffer.keys().next().copied(),
            |last_played| Some(last_played.wrapping_add(1)),
        );

        let Some(seq_to_play) = next_seq_to_play else {
            return PollResult::Empty;
        };

        // Case 1: The exact packet we want is in the buffer.
        if self.buffer.contains_key(&seq_to_play) {
            if now >= self.playout_time {
                let packet = self.buffer.remove(&seq_to_play).unwrap();
                self.advance_playout_time(packet.rtp_timestamp());
                self.last_playout_seq = Some(seq_to_play);
                self.last_playout_rtp_ts = Some(packet.rtp_timestamp());
                return PollResult::PacketReady(packet);
            } else {
                return PollResult::WaitUntil(self.playout_time);
            }
        }

        // Case 2: The packet is missing. We might need to skip it to remain robust against loss.
        // We only consider skipping if there are later packets buffered.
        if !self.buffer.is_empty() {
            // This check is the core of the low-latency goal: we give up on waiting for a
            // lost packet quickly to avoid stalling the stream.
            let overdue = now.saturating_duration_since(self.playout_time);
            if overdue > self.config.loss_patience {
                tracing::warn!(
                    seq_missing = seq_to_play,
                    "loss patience exceeded, skipping gap"
                );
                metrics::counter!("jitterbuffer_gap_skips_total").increment(1);
                self.skip_gap(now);
                return self.poll(now);
            }
        }

        // Case 3: Packet is missing, but we haven't waited long enough.
        PollResult::WaitUntil(self.playout_time + self.config.loss_patience)
    }

    fn reset_state(&mut self) {
        self.buffer.clear();
        self.jitter_estimate = 0.0;
        self.last_playout_seq = None;
        self.last_playout_rtp_ts = None;
        self.last_arrival_time = None;
        self.last_received_rtp_ts = None;
        tracing::debug!("jitter buffer state reset");
    }

    /// Updates the jitter estimate.
    ///
    /// This is the core of the adaptive behavior. The math is a simple IIR filter based on
    /// RFC 3550. Its simplicity is key to server efficiency, as it's computationally
    /// cheap and runs for every packet on every stream.
    fn update_jitter(&mut self, packet: &T) {
        let rtp_timestamp = packet.rtp_timestamp();
        if let (Some(last_arrival), Some(last_rtp)) =
            (self.last_arrival_time, self.last_received_rtp_ts)
        {
            let arrival_diff = packet
                .arrival_timestamp()
                .saturating_duration_since(last_arrival)
                .as_secs_f64();
            let rtp_diff =
                rtp_timestamp.wrapping_sub(last_rtp) as f64 / self.config.clock_rate as f64;

            let transit_diff = (arrival_diff - rtp_diff).abs();
            self.jitter_estimate += (transit_diff - self.jitter_estimate) * JITTER_SMOOTHING_FACTOR;
            self.jitter_estimate = self.jitter_estimate.clamp(0.0, MAX_JITTER_SECONDS);
        }

        metrics::histogram!("jitterbuffer_jitter_estimate_seconds").record(self.jitter_estimate);
        self.last_arrival_time = Some(packet.arrival_timestamp());
        self.last_received_rtp_ts = Some(rtp_timestamp);
    }

    /// Sets the playout time for the *next* packet after one has just been played.
    fn advance_playout_time(&mut self, rtp_ts_sent: u32) {
        if let Some(next_packet) = self.buffer.values().next() {
            let rtp_diff = next_packet.rtp_timestamp().wrapping_sub(rtp_ts_sent);
            let time_diff_secs = rtp_diff as f64 / self.config.clock_rate as f64;
            let time_diff = Duration::from_secs_f64(time_diff_secs);
            self.playout_time += time_diff;
        }
    }

    /// Skips over a gap of one or more missing packets.
    ///
    /// This function is the primary mechanism for robustness against packet loss. Its job
    /// is to recover from a stall. By setting `playout_time = now`, we ensure the next
    /// `poll` call can immediately release the next available packet, prioritizing stream
    /// continuity (liveness) over waiting for lost data.
    fn skip_gap(&mut self, now: Instant) {
        if let Some(next_packet) = self.buffer.values().next() {
            self.playout_time = now;
            self.last_playout_seq = self
                .last_playout_seq
                .map(|_| next_packet.sequence_number().wrapping_sub(1));
            tracing::trace!(
                next_seq = next_packet.sequence_number(),
                "skipped gap to next packet"
            );
        }
    }

    /// Calculates the total current buffering delay. This is adaptive, based on jitter.
    fn current_delay(&self) -> Duration {
        let jitter_delay_secs = self.jitter_estimate * self.config.jitter_multiplier;
        let clamped = jitter_delay_secs.min(MAX_JITTER_SECONDS);
        self.config.base_latency + Duration::from_secs_f64(clamped)
    }

    /// Compares two sequence numbers, correctly handling wrap-around (RFC 1982).
    #[inline]
    fn is_seq_newer_than(a: u64, b: u64) -> bool {
        let diff = b.wrapping_sub(a) as u16;
        diff != 0 && diff < SEQ_NUM_HALF_RANGE
    }
}

// All tests remain the same from the previous correct version.
#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;
    use tokio::time::Instant;
    use tracing_subscriber::fmt::Subscriber;

    // Simulates a 30fps video stream with a 90kHz clock. 90000 / 30 = 3000.
    const RTP_TICKS_PER_VIDEO_FRAME: u32 = 3000;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestPacket {
        seq: u64,
        rtp_ts: u32,
        arrival: Instant,
    }

    impl MediaPacket for TestPacket {
        fn sequence_number(&self) -> u64 {
            self.seq
        }
        fn rtp_timestamp(&self) -> u32 {
            self.rtp_ts
        }
        fn arrival_timestamp(&self) -> Instant {
            self.arrival
        }
    }

    fn make_packet(base_time: Instant, seq: u64, arrival_offset_ms: u64) -> TestPacket {
        TestPacket {
            seq,
            rtp_ts: (seq as u32) * RTP_TICKS_PER_VIDEO_FRAME,
            arrival: base_time + Duration::from_millis(arrival_offset_ms),
        }
    }

    fn setup_tracing() {
        let _ = Subscriber::builder()
            .with_max_level(tracing::Level::TRACE)
            .with_test_writer()
            .try_init();
    }

    #[test]
    fn test_push_and_poll_basic() {
        setup_tracing();
        let mut jb = JitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();
        let p1 = make_packet(now, 1, 0);
        jb.push(p1.clone());
        let playout_time = p1.arrival_timestamp() + jb.current_delay() + Duration::from_millis(1);
        let result = jb.poll(playout_time);
        assert!(matches!(result, PollResult::PacketReady(ref pkt) if pkt == &p1));
    }

    #[test]
    fn test_out_of_order_insertion() {
        setup_tracing();
        let mut jb = JitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();
        jb.push(make_packet(now, 2, 10));
        jb.push(make_packet(now, 1, 0));
        let playout_time = now + Duration::from_millis(50);
        let first = jb.poll(playout_time);
        assert!(matches!(first, PollResult::PacketReady(ref pkt) if pkt.sequence_number() == 1));
    }

    #[test]
    fn test_gap_skip_after_loss_patience() {
        setup_tracing();
        let mut config = JitterBufferConfig::default();
        config.loss_patience = Duration::from_millis(30);
        let mut jb = JitterBuffer::new(config);
        let now = Instant::now();
        jb.push(make_packet(now, 1, 0));
        jb.push(make_packet(now, 3, 30));
        let first_playout_time = now + jb.current_delay() + Duration::from_millis(1);
        let _ = jb.poll(first_playout_time);
        let gap_skip_time = jb.playout_time + config.loss_patience + Duration::from_millis(1);
        let second = jb.poll(gap_skip_time);
        assert!(matches!(second, PollResult::PacketReady(ref pkt) if pkt.sequence_number() == 3));
    }

    #[test]
    fn test_reset_on_silence() {
        setup_tracing();
        let mut config = JitterBufferConfig::default();
        config.silence_threshold = Duration::from_millis(50);
        let mut jb = JitterBuffer::new(config);
        let now = Instant::now();
        jb.push(make_packet(now, 1, 0));
        let p2 = make_packet(now, 100, 60);
        jb.push(p2.clone());
        assert_eq!(jb.buffer.len(), 1);
        assert_eq!(jb.buffer.values().next().unwrap(), &p2);
        assert!(jb.last_playout_seq.is_none());
    }

    #[test]
    fn test_buffer_overflow_drops_oldest() {
        setup_tracing();
        let mut config = JitterBufferConfig::default();
        config.max_capacity = 3;
        let mut jb = JitterBuffer::new(config);
        let now = Instant::now();
        for i in 0..5 {
            jb.push(make_packet(now, i, i * 5));
        }
        assert_eq!(jb.buffer.len(), 3);
        assert!(!jb.buffer.contains_key(&0));
        assert!(jb.buffer.contains_key(&4));
    }

    #[test]
    fn test_jitter_estimate_with_network_variation() {
        setup_tracing();
        let mut jb = JitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();
        let rtp_interval_secs = RTP_TICKS_PER_VIDEO_FRAME as f64 / 90000.0;
        jb.push(TestPacket {
            seq: 1,
            rtp_ts: 3000,
            arrival: now,
        });
        let p2_arrival = now + Duration::from_secs_f64(rtp_interval_secs);
        jb.push(TestPacket {
            seq: 2,
            rtp_ts: 6000,
            arrival: p2_arrival,
        });
        assert!(jb.jitter_estimate < 1e-9);
        let p3_arrival =
            now + Duration::from_secs_f64(2.0 * rtp_interval_secs) + Duration::from_millis(10);
        let last_jitter = jb.jitter_estimate;
        jb.push(TestPacket {
            seq: 3,
            rtp_ts: 9000,
            arrival: p3_arrival,
        });
        assert!(jb.jitter_estimate > last_jitter);
    }
}
