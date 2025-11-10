use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};
use str0m::bwe::Bitrate;
use str0m::media::{Frequency, MediaTime};
use str0m::rtp::SeqNo;
use tokio::time::Instant;

use crate::rtp::PacketTiming;

/// Defines the wall-clock duration without packets after which a stream is considered inactive.
const INACTIVE_TIMEOUT: Duration = Duration::from_secs(2);
/// The size of the circular buffer used for the sliding window packet loss calculation.
const LOSS_WINDOW_SIZE: usize = 256;
/// Number of bitrate samples to track for stability measurement
const BITRATE_HISTORY_SAMPLES: usize = 128;
const DELTA_DELTA_WINDOW_SIZE: usize = 128;
/// Minimum number of samples before making quality decisions
const MIN_SAMPLES_FOR_QUALITY: usize = 8; // ~2 seconds minimum

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamQuality {
    Bad = 0,
    Good = 1,
    Excellent = 2,
}

#[derive(Debug, Clone)]
pub struct StreamState {
    inactive: Arc<AtomicBool>,
    bitrate_bps: Arc<AtomicU64>,
    quality: Arc<AtomicU8>,
}

impl StreamState {
    pub fn new(inactive: bool, bitrate_bps: u64) -> Self {
        Self {
            inactive: Arc::new(AtomicBool::new(inactive)),
            bitrate_bps: Arc::new(AtomicU64::new(bitrate_bps)),
            quality: Arc::new(AtomicU8::new(StreamQuality::Good as u8)),
        }
    }

    pub fn is_inactive(&self) -> bool {
        self.inactive.load(Ordering::Relaxed)
    }

    pub fn bitrate_bps(&self) -> f64 {
        self.bitrate_bps.load(Ordering::Relaxed) as f64
    }

    pub fn quality(&self) -> StreamQuality {
        match self.quality.load(Ordering::Relaxed) {
            0 => StreamQuality::Bad,
            2 => StreamQuality::Excellent,
            _ => StreamQuality::Good,
        }
    }
}

#[derive(Debug)]
pub struct StreamMonitor {
    shared_state: StreamState,

    stream_id: String,
    manual_pause: bool,

    delta_delta: DeltaDeltaState,
    last_packet_at: Instant,
    bwe: BitrateEstimate,

    current_quality: StreamQuality,
}

impl StreamMonitor {
    pub fn new(stream_id: String, shared_state: StreamState) -> Self {
        let now = Instant::now();
        Self {
            stream_id,
            shared_state,
            manual_pause: true,
            last_packet_at: now,
            delta_delta: DeltaDeltaState::new(DELTA_DELTA_WINDOW_SIZE),
            bwe: BitrateEstimate::new(now),
            current_quality: StreamQuality::Good,
        }
    }

    pub fn process_packet(&mut self, packet: &impl PacketTiming, size_bytes: usize) {
        self.last_packet_at = packet.arrival_timestamp();
        self.bwe.record(size_bytes);

        self.delta_delta.update(packet);
    }

    pub fn poll(&mut self, now: Instant) {
        let was_inactive = self.shared_state.is_inactive();
        let is_inactive = self.determine_inactive_state(now);
        if is_inactive && !was_inactive {
            self.reset();
        }
        self.shared_state
            .inactive
            .store(is_inactive, Ordering::Relaxed);
        if is_inactive {
            return;
        }

        self.bwe.poll(now);
        self.shared_state
            .bitrate_bps
            .store(self.bwe.smoothed_bitrate_bps as u64, Ordering::Relaxed);

        let metrics: RawMetrics = (&self.delta_delta).into();
        let quality_score = metrics.quality_score();
        let new_quality = metrics.quality(quality_score);

        tracing::trace!(
            "stream_monitor={},{},{:3},{:3}",
            self.stream_id,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            metrics.dod_ewma,
            metrics.dod_abs_ewma
        );
        if new_quality != self.current_quality {
            tracing::info!(
                stream_id = %self.stream_id,
                "Stream quality transition: {:?} -> {:?} (score: {:.1}, loss: {:.2}%, delta_delta: {:.3}ms, delta_delta_abs: {:.3}ms, bitrate: {})",
                self.current_quality,
                new_quality,
                quality_score,
                metrics.packet_loss() * 100.0,
                metrics.dod_ewma,
                metrics.dod_abs_ewma,
                Bitrate::from(self.bwe.smoothed_bitrate_bps),
            );
            self.current_quality = new_quality;
            self.shared_state
                .quality
                .store(new_quality as u8, Ordering::Relaxed);
        }
    }

    fn reset(&mut self) {
        self.bwe.reset();
        self.current_quality = StreamQuality::Good;
        self.shared_state
            .quality
            .store(StreamQuality::Good as u8, Ordering::Relaxed);
    }

    pub fn set_manual_pause(&mut self, paused: bool) {
        self.manual_pause = paused;
    }

    fn determine_inactive_state(&self, now: Instant) -> bool {
        self.manual_pause || now.saturating_duration_since(self.last_packet_at) > INACTIVE_TIMEOUT
    }
}

#[derive(Debug)]
pub struct BitrateEstimate {
    bwe_last_update: Instant,
    bwe_interval_bytes: usize,
    bwe_bps_ewma: f64,

    smoothed_bitrate_bps: f64,
    history: Histogram,
}

impl BitrateEstimate {
    pub fn new(now: Instant) -> Self {
        Self {
            bwe_last_update: now,
            bwe_interval_bytes: 0,
            bwe_bps_ewma: 0.0,

            smoothed_bitrate_bps: 0.0,
            history: Histogram::new(BITRATE_HISTORY_SAMPLES),
        }
    }

    pub fn record(&mut self, packet_len: usize) {
        self.bwe_interval_bytes = self.bwe_interval_bytes.saturating_add(packet_len);
    }

    pub fn poll(&mut self, now: Instant) {
        const EWMA_ALPHA: f64 = 0.8;
        let elapsed = now.saturating_duration_since(self.bwe_last_update);

        let elapsed_secs = elapsed.as_secs_f64();
        if elapsed_secs > 0.0 && self.bwe_interval_bytes > 0 {
            let bps = (self.bwe_interval_bytes as f64 * 8.0) / elapsed_secs;

            if self.bwe_bps_ewma == 0.0 {
                self.bwe_bps_ewma = bps;
            } else {
                self.bwe_bps_ewma = (1.0 - EWMA_ALPHA) * self.bwe_bps_ewma + EWMA_ALPHA * bps;
            }
            self.history.push(bps);

            // P99 is too noisy + add headroom
            let p95 = 1.5 * self.history.percentile(0.95).unwrap_or_default();

            let bps = p95.max(self.bwe_bps_ewma);
            self.smoothed_bitrate_bps = bps;
            self.bwe_interval_bytes = 0;
            self.bwe_last_update = now;
        }
    }

    pub fn reset(&mut self) {
        self.history.reset();
    }
}

#[derive(Debug)]
pub struct Histogram {
    samples: Vec<u64>,
    value_counts: BTreeMap<u64, usize>,
    capacity: usize,
    index: usize,
    filled: bool,
    total_samples: usize,
}

impl Histogram {
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: vec![0; capacity],
            value_counts: BTreeMap::new(),
            capacity,
            index: 0,
            filled: false,
            total_samples: 0,
        }
    }

    pub fn push(&mut self, value: f64) {
        if self.capacity == 0 {
            return;
        }
        let new_val = value as u64;

        if self.filled {
            let old_val = self.samples[self.index];
            if let Some(count) = self.value_counts.get_mut(&old_val) {
                *count -= 1;
                if *count == 0 {
                    self.value_counts.remove(&old_val);
                }
            }
        } else {
            self.total_samples += 1;
            if self.total_samples == self.capacity {
                self.filled = true;
            }
        }
        self.samples[self.index] = new_val;
        *self.value_counts.entry(new_val).or_insert(0) += 1;
        self.index = (self.index + 1) % self.capacity;
    }

    pub fn percentile(&self, p: f64) -> Option<f64> {
        if self.total_samples < 2 || !(0.0..=1.0).contains(&p) {
            return None;
        }
        let target_rank = ((self.total_samples as f64 - 1.0) * p).round() as usize;
        let mut current_rank = 0;
        for (&value, &count) in &self.value_counts {
            if current_rank + count > target_rank {
                return Some(value as f64);
            }
            current_rank += count;
        }
        self.value_counts.last_key_value().map(|(&v, _)| v as f64)
    }

    pub fn coefficient_of_variation_percent(&self) -> f32 {
        if self.total_samples < 2 {
            return 0.0;
        }
        let samples_slice = if self.filled {
            &self.samples[..]
        } else {
            &self.samples[..self.total_samples]
        };
        let mean = samples_slice.iter().sum::<u64>() as f64 / self.total_samples as f64;
        if mean < 1.0 {
            return 0.0;
        }
        let variance = samples_slice
            .iter()
            .map(|&v| {
                let diff = v as f64 - mean;
                diff * diff
            })
            .sum::<f64>()
            / self.total_samples as f64;
        let std_dev = variance.sqrt();
        ((std_dev / mean) * 100.0) as f32
    }

    pub fn is_ready(&self) -> bool {
        self.total_samples >= MIN_SAMPLES_FOR_QUALITY
    }

    pub fn reset(&mut self) {
        self.samples.fill(0);
        self.value_counts.clear();
        self.index = 0;
        self.filled = false;
        self.total_samples = 0;
    }
}

struct RawMetrics {
    pub dod_ewma: f64,
    pub dod_abs_ewma: f64,
    pub packets_actual: u64,
    pub packets_expected: u64,
}

impl From<&DeltaDeltaState> for RawMetrics {
    fn from(value: &DeltaDeltaState) -> Self {
        Self {
            dod_ewma: value.dod_ewma,
            dod_abs_ewma: value.dod_abs_ewma,
            packets_actual: value.packets_actual,
            packets_expected: value.packets_expected,
        }
    }
}

impl RawMetrics {
    // --- Scoring & Quality Constants (Tunable) ---
    const QUALITY_MIN_PACKETS: u64 = 30;

    // --- Weights for combining scores (must sum to 1.0) ---
    /// Weight for packet loss, reflecting stream reliability.
    const WEIGHT_LOSS: f64 = 0.3;
    /// Weight for network instability (volatility), reflecting short-term jitter.
    const WEIGHT_INSTABILITY: f64 = 0.5;
    /// Weight for congestion trend, reflecting sustained delay increases (bufferbloat).
    const WEIGHT_CONGESTION: f64 = 0.2;

    // --- Normalization Thresholds (where the component score becomes 0) ---
    /// Packet loss percentage at which the loss score component drops to 0.
    const TERRIBLE_LOSS_PERCENT: f64 = 10.0;
    /// Instability (dod_abs_ewma) in ms at which its score component drops to 0.
    const TERRIBLE_INSTABILITY_MS: f64 = 5.0;
    /// Congestion trend (abs(dod_ewma)) in ms at which its score component drops to 0.
    const TERRIBLE_CONGESTION_TREND_MS: f64 = 1.5;

    // --- Final Score to Enum Mapping ---
    const QUALITY_SCORE_EXCELLENT_THRESHOLD: f64 = 90.0;
    const QUALITY_SCORE_GOOD_THRESHOLD: f64 = 60.0;

    fn packet_loss(&self) -> f64 {
        self.packets_expected.saturating_sub(self.packets_actual) as f64
            / self.packets_expected as f64
    }

    pub fn quality_score(&self) -> f64 {
        if self.packets_expected < Self::QUALITY_MIN_PACKETS {
            return 100.0;
        }

        // 1. Calculate the raw metric values.
        let loss_percentage = (1.0 - self.packet_loss()) * 100.0;
        let instability_ms = self.dod_abs_ewma;
        // We only care about the magnitude of the trend, not its direction.
        let congestion_trend_ms = self.dod_ewma.abs();

        // 2. Normalize each metric to a 0.0-100.0 score.
        let loss_score = {
            let scaled_loss = loss_percentage / Self::TERRIBLE_LOSS_PERCENT;
            (1.0 - scaled_loss).max(0.0) * 100.0
        };

        let instability_score = {
            let scaled_instability = instability_ms / Self::TERRIBLE_INSTABILITY_MS;
            (1.0 - scaled_instability).max(0.0) * 100.0
        };

        let congestion_score = {
            let scaled_congestion = congestion_trend_ms / Self::TERRIBLE_CONGESTION_TREND_MS;
            (1.0 - scaled_congestion).max(0.0) * 100.0
        };

        // 3. Combine the scores using the defined weights.
        let final_score = (loss_score * Self::WEIGHT_LOSS)
            + (instability_score * Self::WEIGHT_INSTABILITY)
            + (congestion_score * Self::WEIGHT_CONGESTION);

        final_score
    }

    /// Derives a `StreamQuality` enum from the numerical quality score.
    pub fn quality(&self, score: f64) -> StreamQuality {
        if score >= Self::QUALITY_SCORE_EXCELLENT_THRESHOLD {
            StreamQuality::Excellent
        } else if score >= Self::QUALITY_SCORE_GOOD_THRESHOLD {
            StreamQuality::Good
        } else {
            StreamQuality::Bad
        }
    }
}

#[derive(Clone, Debug)]
struct PacketStatus {
    seqno: SeqNo,
    arrival: Instant,
    rtp_ts: MediaTime,
}

/// Maps an input value to a specified range using an inverted logistic (sigmoid) function.
///
/// As the `value` increases, the output approaches the minimum of the range (0).
/// As the `value` decreases, the output approaches the `range_max`.
///
/// # Arguments
/// * `value` - The input value to map (e.g., dod_abs_ewma).
/// * `range_max` - The maximum value of the output range (e.g., 100.0).
/// * `k` - The steepness factor of the curve. Determines how quickly the output changes around the midpoint.
/// * `midpoint` - The input `value` at which the output will be exactly `range_max / 2`.
///
/// # Returns
/// The mapped value, which will be between 0.0 and `range_max`.
#[inline]
fn sigmoid(value: f64, range_max: f64, k: f64, midpoint: f64) -> f64 {
    range_max / (1.0 + (k * (value - midpoint)).exp())
}

#[derive(Debug)]
struct DeltaDeltaState {
    head: SeqNo,          // Next expected seq
    tail: SeqNo,          // Oldest seq in buffer
    frequency: Frequency, // Clock rate (e.g., 90000)
    last_rtp_ts: MediaTime,
    last_arrival: Instant,
    last_skew: f64,
    pub dod_ewma: f64,
    pub dod_abs_ewma: f64,
    packets_actual: u64,
    packets_expected: u64,
    buffer: Vec<Option<PacketStatus>>,
    initialized: bool,
}

impl DeltaDeltaState {
    pub fn new(cap: usize) -> Self {
        Self {
            head: 0.into(),
            tail: 0.into(),
            frequency: Frequency::NINETY_KHZ,
            last_rtp_ts: MediaTime::from_90khz(0),
            last_arrival: Instant::now(),
            last_skew: 0.0,
            dod_ewma: 0.0,
            dod_abs_ewma: 0.0,
            packets_actual: 0,
            packets_expected: 0,
            buffer: vec![None; cap],
            initialized: false,
        }
    }

    pub fn update<T: PacketTiming>(&mut self, packet: &T) {
        if !self.initialized {
            self.init(packet);
        }
        let seq = packet.seq_no();

        // Check if packet is older than tail using wrapping comparison
        let tail_val = *self.tail;
        let seq_val = *seq;
        let seq_offset = seq_val.wrapping_sub(tail_val);

        // If seq is more than half the u64 space behind tail, it's considered old
        if seq_offset > (u64::MAX / 2) {
            tracing::warn!(
                "{} is older than the current tail, {}, ignore it",
                seq,
                self.tail
            );
            return;
        }

        let rtp_ts = packet.rtp_timestamp();
        let arrival = packet.arrival_timestamp();
        let buffer_capacity = self.buffer.len() as u64;

        // Update head if this packet is beyond it
        let head_val = *self.head;
        let seq_ahead_of_head = seq_val.wrapping_sub(head_val);

        if seq_ahead_of_head < (u64::MAX / 2) || seq == self.head {
            self.head = seq.wrapping_add(1).into();
        }

        // Check if there's space in the buffer for this packet
        // Using wrapping arithmetic: offset < capacity means it's in range
        let offset_from_tail = seq_val.wrapping_sub(tail_val);
        if offset_from_tail >= buffer_capacity {
            // No space - need to slide the window
            // process_until is now optimized to only check each buffer slot once
            let new_tail = self.head.wrapping_sub(buffer_capacity);
            self.process_until(new_tail.into());
        }

        *self.packet_mut(seq) = Some(PacketStatus {
            seqno: seq,
            arrival,
            rtp_ts,
        });
        self.process_in_order();
    }

    fn init<T: PacketTiming>(&mut self, packet: &T) {
        let seq = packet.seq_no();
        let rtp_ts = packet.rtp_timestamp();
        let arrival = packet.arrival_timestamp();

        self.head = seq.wrapping_add(1).into();
        self.tail = seq;
        self.frequency = rtp_ts.frequency();
        self.last_arrival = arrival;
        self.initialized = true;
    }

    fn advance(&mut self, pkt: &PacketStatus) {
        const ALPHA: f64 = 0.1;
        let actual_ms = pkt.arrival.duration_since(self.last_arrival).as_secs_f64() * 1000.0;

        let expected_ms = (pkt.rtp_ts.numer().wrapping_sub(self.last_rtp_ts.numer()) as f64)
            * 1000.0
            / self.frequency.get() as f64;

        let skew = actual_ms - expected_ms;
        let dod = skew - self.last_skew;

        self.dod_ewma = ALPHA * dod + (1.0 - ALPHA) * self.dod_ewma;
        self.dod_abs_ewma = ALPHA * dod.abs() + (1.0 - ALPHA) * self.dod_abs_ewma;

        self.last_skew = skew;
        self.last_arrival = pkt.arrival;
        self.last_rtp_ts = pkt.rtp_ts;
        self.packets_actual += 1;
        self.packets_expected += 1;
    }

    fn process_in_order(&mut self) {
        // Process packets in sequence order starting from tail
        loop {
            let tail_val = *self.tail;
            let head_val = *self.head;

            // Check if we've caught up to head using wrapping arithmetic
            let distance = head_val.wrapping_sub(tail_val);
            if distance == 0 || distance > (u64::MAX / 2) {
                // Either we're at head, or head is somehow behind us (shouldn't happen)
                break;
            }

            // Try to get the packet at tail position
            let Some(pkt) = self.packet_mut(self.tail).take() else {
                // No packet at tail, can't continue processing in order
                break;
            };

            self.advance(&pkt);
            self.tail = self.tail.wrapping_add(1).into();
        }
    }

    fn process_until(&mut self, end: SeqNo) {
        let tail_val = *self.tail;
        let end_val = *end;

        // Calculate how many sequence numbers to process
        let count = end_val.wrapping_sub(tail_val);

        // If count is 0 or seems like we're going backwards, something is wrong
        if count == 0 || count > (u64::MAX / 2) {
            if count != 0 {
                tracing::warn!(
                    "process_until: end {} appears to be before tail {}",
                    end,
                    self.tail
                );
            }
            return;
        }

        let buffer_capacity = self.buffer.len() as u64;

        // If the jump is larger than our buffer, we only need to check
        // each buffer slot once. Limit iteration to buffer_capacity.
        // Any packets beyond that are guaranteed to be lost anyway.
        let iterations = count.min(buffer_capacity);

        if count > buffer_capacity {
            // We're skipping more packets than our buffer can hold
            // Count the definitely-lost packets (everything before our buffer range)
            let packets_definitely_lost = count - buffer_capacity;
            self.packets_expected += packets_definitely_lost;

            tracing::debug!(
                "process_until: large jump of {} packets, {} definitely lost, checking last {} slots",
                count,
                packets_definitely_lost,
                iterations
            );
        }

        // Only process the last buffer_capacity worth of packets
        // Start from (end - buffer_capacity) to end
        let start_checking = end_val.wrapping_sub(iterations);

        for i in 0..iterations {
            let current = start_checking.wrapping_add(i);
            let Some(pkt) = self.packet_mut(current.into()).take() else {
                self.packets_expected += 1;
                continue;
            };

            self.advance(&pkt);
        }
        self.tail = end;
    }

    fn as_index(&self, seq: SeqNo) -> usize {
        (*seq % self.buffer.len() as u64) as usize
    }

    fn packet(&mut self, seq: SeqNo) -> &Option<PacketStatus> {
        let index = self.as_index(seq);
        &mut self.buffer[index]
    }

    fn packet_mut(&mut self, seq: SeqNo) -> &mut Option<PacketStatus> {
        let index = self.as_index(seq);
        &mut self.buffer[index]
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;
    use tokio::time::Instant;

    const TEST_CAP: usize = 10;
    const TEST_FREQ: Frequency = Frequency::NINETY_KHZ;
    const MS_PER_PACKET: u64 = 20; // 20ms per packet
    const RTP_TS_PER_PACKET: u64 = (TEST_FREQ.get() as u64 * MS_PER_PACKET) / 1000; // 1800

    #[derive(Clone)]
    struct TestPacket {
        seq: SeqNo,
        rtp_ts: MediaTime,
        arrival: Instant,
    }

    impl PacketTiming for TestPacket {
        fn seq_no(&self) -> SeqNo {
            self.seq
        }
        fn rtp_timestamp(&self) -> MediaTime {
            self.rtp_ts
        }
        fn arrival_timestamp(&self) -> Instant {
            self.arrival
        }
    }

    // Helper to create a stream of test packets
    struct PacketFactory {
        start_time: Instant,
        start_seq: u64,
        start_rtp_ts: u64,
    }

    impl PacketFactory {
        fn new() -> Self {
            Self {
                start_time: Instant::now(),
                start_seq: 1,
                start_rtp_ts: 1000,
            }
        }

        fn with_start_seq(start_seq: u64) -> Self {
            Self {
                start_time: Instant::now(),
                start_seq,
                start_rtp_ts: 1000,
            }
        }

        // Creates a packet with a given sequence number and an optional jitter in arrival time
        fn at(&self, seq: u64, arrival_jitter_ms: i64) -> TestPacket {
            // Use wrapping arithmetic to handle overflow correctly
            let packets_since_start = seq.wrapping_sub(self.start_seq);
            let rtp_ts = self
                .start_rtp_ts
                .wrapping_add(packets_since_start.wrapping_mul(RTP_TS_PER_PACKET));
            let ideal_arrival_ms = packets_since_start.wrapping_mul(MS_PER_PACKET);

            let arrival_time = if arrival_jitter_ms >= 0 {
                self.start_time
                    + Duration::from_millis(ideal_arrival_ms.wrapping_add(arrival_jitter_ms as u64))
            } else {
                self.start_time + Duration::from_millis(ideal_arrival_ms)
                    - Duration::from_millis(arrival_jitter_ms.unsigned_abs())
            };

            TestPacket {
                seq: seq.into(),
                rtp_ts: MediaTime::new(rtp_ts, TEST_FREQ),
                arrival: arrival_time,
            }
        }
    }

    #[test]
    fn test_initialization() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let factory = PacketFactory::new();
        let pkt = factory.at(1, 0);

        monitor.update(&pkt);

        assert_eq!(
            monitor.tail,
            2.into(),
            "Tail should advance past the first packet"
        );
        assert_eq!(monitor.head, 2.into(), "Head should be next expected seq");
        assert_eq!(monitor.last_rtp_ts, pkt.rtp_timestamp());
        assert_eq!(monitor.last_arrival, pkt.arrival_timestamp());
    }

    #[test]
    fn test_in_order_processing() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let factory = PacketFactory::new();

        let pkt1 = factory.at(1, 0);
        monitor.update(&pkt1);
        assert_eq!(monitor.tail, 2.into());

        let pkt2 = factory.at(2, 0);
        monitor.update(&pkt2);

        assert_eq!(monitor.tail, 3.into(), "Tail should advance to 3");
        assert_eq!(monitor.head, 3.into(), "Head should advance to 3");
    }

    #[test]
    fn test_out_of_order_within_buffer() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let factory = PacketFactory::new();

        let pkt1 = factory.at(1, 0);
        let pkt3 = factory.at(3, 0);
        let pkt2 = factory.at(2, 0);

        monitor.update(&pkt1);
        assert_eq!(monitor.tail, 2.into());
        assert_eq!(monitor.head, 2.into());

        monitor.update(&pkt3); // Packet 3 arrives
        assert_eq!(monitor.tail, 2.into(), "Tail shouldn't move, waiting for 2");
        assert_eq!(monitor.head, 4.into(), "Head should jump to 4");
        assert!(
            monitor.packet(3.into()).is_some(),
            "Packet 3 should be in buffer"
        );

        monitor.update(&pkt2); // Packet 2 arrives, filling the gap
        assert_eq!(
            monitor.tail,
            4.into(),
            "Tail should advance to 4 after processing 2, 3"
        );
        assert_eq!(monitor.head, 4.into(), "Head should remain 4");
    }

    #[test]
    fn test_packet_loss() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let factory = PacketFactory::new();

        monitor.update(&factory.at(1, 0));
        assert_eq!(monitor.tail, 2.into(), "Tail is 2, waiting for packet 2");
        monitor.update(&factory.at(3, 0));
        assert_eq!(
            monitor.tail,
            2.into(),
            "Tail should still be 2, waiting for 2"
        );

        monitor.update(&factory.at(12, 0)); // This packet is outside the window [2, 2+10)

        // The head becomes 13. new_tail = 13 - 10 = 3.
        // process_until(3) is called. It processes from old_tail (2) up to 3 (exclusive).
        // It looks for packet 2, doesn't find it (loss), and sets tail to 3.
        // Then process_in_order() runs and immediately processes packet 3, advancing tail to 4.
        assert_eq!(monitor.head, 13.into());
        assert_eq!(
            monitor.tail,
            4.into(),
            "Tail should be at 4 after sliding past lost packet 2 and processing packet 3"
        );
    }

    #[test]
    fn test_buffer_wraparound_and_making_space() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP); // Capacity is 10
        let factory = PacketFactory::new();

        monitor.update(&factory.at(1, 0));
        assert_eq!(monitor.tail, 2.into());
        assert_eq!(monitor.head, 2.into());

        let pkt12 = factory.at(12, 0);
        monitor.update(&pkt12);

        assert_eq!(monitor.head, 13.into());
        assert_eq!(
            monitor.tail,
            3.into(),
            "Tail should be moved forward to make space"
        );
        assert!(
            monitor.packet(1.into()).is_none(),
            "Packet 1 should have been processed and removed"
        );
        assert!(
            monitor.packet(12.into()).is_some(),
            "Packet 12 should now be in the buffer"
        );
    }

    #[test]
    fn test_ignore_old_packet() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let factory = PacketFactory::new();

        monitor.update(&factory.at(5, 0));
        monitor.update(&factory.at(6, 0));

        assert_eq!(monitor.tail, 7.into());
        let before_last_arrival = monitor.last_arrival;

        // Send a packet older than the current tail
        monitor.update(&factory.at(4, 0));

        // State should not have changed
        assert_eq!(monitor.tail, 7.into(), "Old packet should not change tail");
        assert_eq!(
            monitor.last_arrival, before_last_arrival,
            "State should not be updated by an old packet"
        );
    }

    #[test]
    fn test_duplicate_packet_overwrite() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let factory = PacketFactory::new();

        // Send packets 1 and 3, creating a gap for packet 2.
        monitor.update(&factory.at(1, 0));
        monitor.update(&factory.at(3, 0));

        assert_eq!(monitor.tail, 2.into());
        assert!(monitor.packet(3.into()).is_some());

        // Send a "duplicate" of 3, but with a different arrival time.
        let pkt3_dup = factory.at(3, 10);
        monitor.update(&pkt3_dup);
        assert_eq!(
            monitor.packet(3.into()).as_ref().unwrap().arrival,
            pkt3_dup.arrival,
            "Duplicate should overwrite existing packet data"
        );

        // Now, fill the gap with packet 2.
        monitor.update(&factory.at(2, 0));

        assert_eq!(
            monitor.tail,
            4.into(),
            "Tail should have processed up to packet 3"
        );
        assert_eq!(
            monitor.last_arrival, pkt3_dup.arrival,
            "The monitor should have used the overwritten packet's data"
        );
    }

    #[test]
    fn test_sequence_number_wrap_around() {
        let mut monitor = DeltaDeltaState::new(TEST_CAP);
        let start_seq = u64::MAX - 5;
        let factory = PacketFactory::with_start_seq(start_seq);
        let mut current_seq = start_seq;

        // Initialize the monitor
        monitor.update(&factory.at(current_seq, 0));
        assert_eq!(monitor.tail, (current_seq.wrapping_add(1)).into());

        // Process several packets, wrapping around u64::MAX
        for _ in 0..15 {
            current_seq = current_seq.wrapping_add(1);
            monitor.update(&factory.at(current_seq, 0));
        }
        let expected_tail = start_seq.wrapping_add(16);
        let expected_head = start_seq.wrapping_add(16);
        assert_eq!(monitor.tail, expected_tail.into());
        assert_eq!(monitor.head, expected_head.into());
    }
}
