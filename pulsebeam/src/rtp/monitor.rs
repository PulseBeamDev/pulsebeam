//! # Real-time Stream Quality Monitor
//!
//! ## Architecture
//! This module implements a monitor that analyzes a single RTP stream to derive actionable
//! quality metrics. The design pattern separates the system into two distinct components:
//!
//! 1.  `StreamMonitor`: A single-owner, stateful struct responsible for all calculations.
//!     Its internal state is non-atomic for performance, as it is not intended to be shared.
//!     It consumes packet metadata and synthesizes it into high-level metrics including
//!     bitrate stability.
//!
//! 2.  `StreamState`: A lightweight, thread-safe struct holding the final, shared conclusions
//!     (e.g., quality and inactive status). It uses atomic types to allow for lock-free reads
//!     from multiple downstream consumers, providing a simple and performant API for decision-making.

use std::time::Duration;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};
use str0m::rtp::SeqNo;
use tokio::time::Instant;

use crate::rtp::PacketTiming;

/// Defines the wall-clock duration without packets after which a stream is considered inactive.
const INACTIVE_TIMEOUT: Duration = Duration::from_secs(2);
/// The size of the circular buffer used for the sliding window packet loss calculation.
const LOSS_WINDOW_SIZE: usize = 256;
/// Number of bitrate samples to track for stability measurement
const BITRATE_HISTORY_SAMPLES: usize = 16; // ~3-5 seconds of data

// --- Public-Facing Shared State ---

/// Represents the measured quality of the stream, including bitrate stability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamQuality {
    /// The stream is experiencing significant packet loss, jitter, or bitrate instability.
    Bad = 0,
    /// The stream is performing adequately.
    Good = 1,
    /// The stream has very low packet loss, jitter, and stable bitrate.
    Excellent = 2,
}

/// A lightweight, shared view of a stream layer's status.
/// Designed for safe, multi-threaded access by downstream packet forwarders.
#[derive(Debug, Clone)]
pub struct StreamState {
    /// If true, this stream is considered inactive, either manually or due to timeout.
    /// This state is separate from stream quality.
    inactive: Arc<AtomicBool>,
    bitrate_bps_p99: Arc<AtomicU64>,
    bitrate_bps_p50: Arc<AtomicU64>,
    /// The current assessed quality of the stream (includes stability).
    quality: Arc<AtomicU8>,
}

impl StreamState {
    pub fn new(inactive: bool, bitrate_bps: u64) -> Self {
        Self {
            inactive: Arc::new(AtomicBool::new(inactive)),
            bitrate_bps_p99: Arc::new(AtomicU64::new(bitrate_bps)),
            bitrate_bps_p50: Arc::new(AtomicU64::new(bitrate_bps)),
            quality: Arc::new(AtomicU8::new(StreamQuality::Bad as u8)),
        }
    }

    /// Returns `true` if the stream is manually paused or has timed out due to inactivity.
    pub fn is_inactive(&self) -> bool {
        self.inactive.load(Ordering::Relaxed)
    }

    /// Returns the estimated bitrate in bits per second.
    pub fn bitrate_bps_p99(&self) -> u64 {
        self.bitrate_bps_p99.load(Ordering::Relaxed)
    }

    pub fn bitrate_bps_p50(&self) -> u64 {
        self.bitrate_bps_p50.load(Ordering::Relaxed)
    }

    /// Returns the current quality of the stream.
    /// Note: Quality assessment now includes bitrate stability as a factor.
    pub fn quality(&self) -> StreamQuality {
        match self.quality.load(Ordering::Relaxed) {
            0 => StreamQuality::Bad,
            2 => StreamQuality::Excellent,
            _ => StreamQuality::Good, // Default case
        }
    }
}

// --- Bitrate History for Stability Tracking ---

/// A data structure for tracking a history of bitrate samples over a sliding window.
///
/// It is highly optimized for calculating statistics like percentiles and the
/// coefficient of variation without expensive operations like cloning or sorting
/// on each call. It uses a combination of a circular buffer (for storing raw values
/// in insertion order) and a sorted frequency map (`BTreeMap`) for efficient statistical queries.
#[derive(Debug)]
pub struct BitrateHistory {
    /// A circular buffer holding the raw sample values in insertion order.
    /// Pre-allocated to the capacity to avoid reallocations on push.
    samples: Vec<u64>,

    /// A sorted frequency map of the values currently in the sliding window.
    /// The key is the bitrate value (as bps), and the value is its frequency (count).
    /// This allows for O(log K) updates and fast percentile calculations, where K
    /// is the number of unique values in the window.
    value_counts: BTreeMap<u64, usize>,

    /// The maximum number of samples to store.
    capacity: usize,

    /// The current position in the circular buffer.
    index: usize,

    /// Becomes true once the buffer has been completely filled at least once.
    filled: bool,

    /// The current number of valid samples in the history (<= capacity).
    total_samples: usize,
}

impl BitrateHistory {
    /// Creates a new `BitrateHistory` with a specified capacity.
    ///
    /// # Arguments
    ///
    /// * `capacity` - The number of samples to keep in the sliding window.
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: vec![0; capacity], // Pre-allocate to prevent reallocations.
            value_counts: BTreeMap::new(),
            capacity,
            index: 0,
            filled: false,
            total_samples: 0,
        }
    }

    /// Adds a new bitrate sample to the history.
    ///
    /// This is an O(log K) operation, where K is the number of unique bitrate
    /// values currently in the window.
    ///
    /// # Arguments
    ///
    /// * `value` - The new bitrate sample (in bits per second).
    pub fn push(&mut self, value: f64) {
        if self.capacity == 0 {
            return;
        }

        let new_val = value as u64;

        // If the buffer is full, an old value is being replaced.
        // We must remove the old value from our frequency map first.
        if self.filled {
            let old_val = self.samples[self.index];
            if let Some(count) = self.value_counts.get_mut(&old_val) {
                *count -= 1;
                if *count == 0 {
                    self.value_counts.remove(&old_val);
                }
            }
        } else {
            // The buffer is not yet full, so we are just adding a new sample.
            self.total_samples += 1;
            if self.total_samples == self.capacity {
                self.filled = true;
            }
        }

        // Update the circular buffer with the new value.
        self.samples[self.index] = new_val;

        // Add the new value to our frequency map.
        *self.value_counts.entry(new_val).or_insert(0) += 1;

        // Advance the circular buffer index.
        self.index = (self.index + 1) % self.capacity;
    }

    /// Calculates the value at a given percentile from the samples in the window.
    ///
    /// This operation is efficient and does not require sorting the entire sample set.
    /// Its complexity is O(K), where K is the number of unique values.
    ///
    /// # Arguments
    ///
    /// * `p` - The desired percentile, expressed as a float between 0.0 (P0) and 1.0 (P100).
    ///
    /// # Returns
    ///
    /// An `Option<f64>` containing the bitrate value at the specified percentile,
    /// or `None` if there are not enough samples to compute it.
    pub fn percentile(&self, p: f64) -> Option<f64> {
        // Ensure there's enough data and the percentile is valid.
        if self.total_samples < 2 || !(0.0..=1.0).contains(&p) {
            return None;
        }

        // Determine the target rank (0-indexed) using the nearest-rank method.
        let target_rank = ((self.total_samples as f64 - 1.0) * p).round() as usize;

        let mut current_rank = 0;
        // The BTreeMap iterates through key-value pairs in sorted key order.
        for (&value, &count) in &self.value_counts {
            // If the target rank falls within the group of this value, we've found our percentile.
            if current_rank + count > target_rank {
                return Some(value as f64);
            }
            current_rank += count;
        }

        // Fallback to the last element if rank calculation points to the very end.
        // This can happen with p=1.0 due to floating point inaccuracies.
        self.value_counts.last_key_value().map(|(&v, _)| v as f64)
    }

    /// Returns the coefficient of variation (CV) as a percentage.
    /// CV = (standard_deviation / mean) * 100
    /// This gives a normalized stability metric independent of absolute bitrate.
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
            return 0.0; // Avoid division by zero or meaningless CV for near-zero bitrates.
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

    /// Returns true if the history buffer has been completely filled.
    pub fn is_ready(&self) -> bool {
        self.filled
    }

    /// Resets the history, clearing all samples and state.
    pub fn reset(&mut self) {
        // No need to reallocate, just clear the existing structures.
        self.samples.fill(0);
        self.value_counts.clear();
        self.index = 0;
        self.filled = false;
        self.total_samples = 0;
    }
}

// --- Packet Loss Window (Bitmap-based) ---

/// A bitmap-based circular buffer for tracking packet reception in a sliding window.
/// Uses u64 chunks for efficient storage and bit manipulation.
#[derive(Debug)]
struct PacketLossWindow {
    /// Bitmap storage. Each bit represents whether a packet was received.
    /// True = received, False = lost/not yet seen.
    bitmap: Vec<u64>,
    /// The highest sequence number we've seen so far.
    highest_seq: SeqNo,
    /// Whether we've received our first packet.
    initialized: bool,
    /// Size of the window in packets.
    window_size: usize,
    /// Number of packets received so far (for initialization tracking).
    packets_received: usize,
}

impl PacketLossWindow {
    fn new(window_size: usize) -> Self {
        // Calculate how many u64s we need to store window_size bits
        let chunks_needed = (window_size + 63) / 64;
        Self {
            bitmap: vec![0u64; chunks_needed],
            highest_seq: 0.into(),
            initialized: false,
            window_size,
            packets_received: 0,
        }
    }

    /// Records that a packet with the given sequence number was received.
    fn mark_received(&mut self, seq_no: SeqNo) {
        if !self.initialized {
            self.highest_seq = seq_no;
            self.initialized = true;
            self.set_bit(seq_no, true);
            self.packets_received = 1;
            return;
        }

        let seq_u64 = *seq_no;
        let highest_u64 = *self.highest_seq;

        // Calculate the difference handling wraparound
        // Cast to i64 to properly detect forward vs backward movement
        let raw_diff = seq_u64.wrapping_sub(highest_u64);
        let diff = if raw_diff > (u16::MAX as u64 / 2) {
            // Wrapped backward
            -((u16::MAX as i64 + 1) - raw_diff as i64)
        } else {
            raw_diff as i64
        };

        if diff > 0 && diff < (self.window_size as i64) {
            // New packet is ahead but within window
            // Clear bits for all skipped sequence numbers
            for i in 1..diff {
                let intermediate_seq: SeqNo = highest_u64.wrapping_add(i as u64).into();
                self.set_bit(intermediate_seq, false);
            }
            self.highest_seq = seq_no;
            self.set_bit(seq_no, true);
            self.packets_received = self.packets_received.saturating_add(1);
        } else if diff <= 0 && diff > -(self.window_size as i64) {
            // Old/reordered packet still within window
            // Only mark as received if not already set (don't double count)
            let was_set = self.get_bit(seq_no);
            if !was_set {
                self.set_bit(seq_no, true);
                self.packets_received = self.packets_received.saturating_add(1);
            }
        } else if diff >= (self.window_size as i64) {
            // Major jump forward - treat as reset
            self.reset_with_seq(seq_no);
        }
        // Packets too far in the past are ignored
    }

    /// Calculates the current packet loss percentage in the window.
    /// Returns 0.0 if we haven't received enough packets to fill the window yet.
    fn calculate_loss_percent(&self) -> f32 {
        if !self.initialized {
            return 0.0;
        }

        // If we haven't received enough packets to fill the window,
        // don't report loss yet (we're still ramping up)
        if self.packets_received < self.window_size {
            return 0.0;
        }

        let received_count = self.count_received_in_window();
        let lost_count = self.window_size.saturating_sub(received_count);

        (lost_count as f32 * 100.0) / (self.window_size as f32)
    }

    /// Returns true if the window has been fully initialized with enough packets.
    fn is_ready(&self) -> bool {
        self.initialized && self.packets_received >= self.window_size
    }

    /// Resets the window to its initial state.
    fn reset(&mut self) {
        for chunk in &mut self.bitmap {
            *chunk = 0;
        }
        self.initialized = false;
        self.packets_received = 0;
        self.highest_seq = 0.into();
    }

    /// Resets the window with a new starting sequence number.
    fn reset_with_seq(&mut self, seq_no: SeqNo) {
        self.reset();
        self.highest_seq = seq_no;
        self.set_bit(seq_no, true);
        self.packets_received = 1;
        self.initialized = true;
    }

    /// Gets the value of a bit in the bitmap for the given sequence number.
    fn get_bit(&self, seq_no: SeqNo) -> bool {
        let idx = (*seq_no as usize) % self.window_size;
        let chunk_idx = idx / 64;
        let bit_idx = idx % 64;

        if chunk_idx >= self.bitmap.len() {
            return false;
        }

        (self.bitmap[chunk_idx] & (1u64 << bit_idx)) != 0
    }

    /// Sets a bit in the bitmap for the given sequence number.
    fn set_bit(&mut self, seq_no: SeqNo, value: bool) {
        let idx = (*seq_no as usize) % self.window_size;
        let chunk_idx = idx / 64;
        let bit_idx = idx % 64;

        if chunk_idx >= self.bitmap.len() {
            return; // Safety check
        }

        if value {
            self.bitmap[chunk_idx] |= 1u64 << bit_idx;
        } else {
            self.bitmap[chunk_idx] &= !(1u64 << bit_idx);
        }
    }

    /// Counts how many packets in the current window are marked as received.
    fn count_received_in_window(&self) -> usize {
        let window_start = (*self.highest_seq as usize).wrapping_sub(self.window_size - 1);
        let mut count = 0;

        for i in 0..self.window_size {
            let seq = window_start.wrapping_add(i);
            if self.get_bit((seq as u64).into()) {
                count += 1;
            }
        }
        count
    }
}

// --- Health Thresholds ---

/// Defines asymmetric thresholds to prevent state flapping.
/// Now includes bitrate stability thresholds.
#[derive(Debug, Clone, Copy)]
pub struct HealthThresholds {
    pub become_bad_loss_percent: f32,
    pub become_bad_jitter_ms: u32,
    pub become_bad_bitrate_cv_percent: f32,

    pub become_good_loss_percent: f32,
    pub become_good_jitter_ms: u32,
    pub become_good_bitrate_cv_percent: f32,

    pub become_excellent_loss_percent: f32,
    pub become_excellent_jitter_ms: u32,
    pub become_excellent_bitrate_cv_percent: f32,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self {
            // Bad thresholds (degradation)
            become_bad_loss_percent: 0.1,
            become_bad_jitter_ms: 30,
            become_bad_bitrate_cv_percent: 30.0, // >30% CV = unstable

            // Good thresholds (recovery)
            become_good_loss_percent: 0.01,
            become_good_jitter_ms: 20,
            become_good_bitrate_cv_percent: 20.0, // <20% CV = stable enough

            // Excellent thresholds (optimal)
            become_excellent_loss_percent: 0.0,
            become_excellent_jitter_ms: 15,
            become_excellent_bitrate_cv_percent: 10.0, // <10% CV = very stable
        }
    }
}

// --- Stream Monitor ---

/// A single-owner monitor that performs stream analysis.
#[derive(Debug)]
pub struct StreamMonitor {
    shared_state: StreamState,
    thresholds: HealthThresholds,

    // --- NON-ATOMIC INTERNAL STATE ---
    manual_pause: bool,
    last_packet_at: Instant,
    clock_rate: u32,

    bwe_last_update: Instant,
    bwe_interval_bytes: usize,

    // Jitter calculation
    jitter_estimate: f64,
    jitter_last_arrival: Option<Instant>,
    jitter_last_rtp_time: Option<u64>,

    // Packet loss tracking
    loss_window: PacketLossWindow,

    // Bitrate stability tracking
    bitrate_history: BitrateHistory,

    // Derived metrics, calculated periodically in `poll`.
    raw_loss_percent: f32,
    raw_jitter_ms: u32,
    raw_bitrate_cv_percent: f32,
    current_quality: StreamQuality,
}

impl StreamMonitor {
    pub fn new(shared_state: StreamState, thresholds: HealthThresholds) -> Self {
        let now = Instant::now();
        Self {
            shared_state,
            thresholds,
            manual_pause: true,
            last_packet_at: now,
            clock_rate: 0,
            bwe_last_update: now,
            bwe_interval_bytes: 0,
            jitter_estimate: 0.0,
            jitter_last_arrival: None,
            jitter_last_rtp_time: None,
            loss_window: PacketLossWindow::new(LOSS_WINDOW_SIZE),
            bitrate_history: BitrateHistory::new(BITRATE_HISTORY_SAMPLES),
            raw_loss_percent: 0.0,
            raw_jitter_ms: 0,
            raw_bitrate_cv_percent: 0.0,
            current_quality: StreamQuality::Good,
        }
    }

    /// Ingests metrics from a new packet. This method only accumulates raw data;
    /// expensive calculations are deferred to `poll`.
    pub fn process_packet(&mut self, packet: &impl PacketTiming, size_bytes: usize) {
        self.last_packet_at = packet.arrival_timestamp();
        self.discover_clock_rate(packet);

        self.bwe_interval_bytes = self.bwe_interval_bytes.saturating_add(size_bytes);
        self.loss_window.mark_received(packet.seq_no());
        self.update_jitter(packet);
    }

    /// Periodically updates derived metrics and commits the final conclusion to the shared state.
    /// This should be called from a ticker or other periodic task.
    pub fn poll(&mut self, now: Instant) {
        let was_inactive = self.shared_state.is_inactive();
        let is_inactive = self.determine_inactive_state(now);

        // If we transition from active to inactive, reset all metrics
        if is_inactive && !was_inactive {
            self.reset();
        }

        self.shared_state
            .inactive
            .store(is_inactive, Ordering::Relaxed);

        // Do not perform quality calculations for an inactive stream.
        if is_inactive {
            return;
        }

        self.update_derived_metrics(now);

        // Update stream quality based on metrics (now includes bitrate stability)
        let new_quality = self.determine_stream_quality();
        if new_quality != self.current_quality {
            tracing::debug!(
                "changed stream quality: {:?} -> {:?}",
                self.current_quality,
                new_quality
            );
            self.current_quality = new_quality;
            self.shared_state
                .quality
                .store(new_quality as u8, Ordering::Relaxed);
        }
    }

    /// Resets the monitor to a clean state. Called when a stream becomes inactive.
    fn reset(&mut self) {
        self.loss_window.reset();
        self.jitter_estimate = 0.0;
        self.jitter_last_arrival = None;
        self.jitter_last_rtp_time = None;
        self.bitrate_history.reset();
        self.raw_loss_percent = 0.0;
        self.raw_jitter_ms = 0;
        self.raw_bitrate_cv_percent = 0.0;
        self.bwe_interval_bytes = 0;

        // Reset quality to a neutral default. It must prove itself again.
        self.current_quality = StreamQuality::Good;
        self.shared_state
            .quality
            .store(StreamQuality::Good as u8, Ordering::Relaxed);
    }

    pub fn set_manual_pause(&mut self, paused: bool) {
        self.manual_pause = paused;
    }

    pub fn get_loss_percent(&self) -> f32 {
        self.raw_loss_percent
    }

    pub fn get_jitter_ms(&self) -> u32 {
        self.raw_jitter_ms
    }

    pub fn get_bitrate_cv_percent(&self) -> f32 {
        self.raw_bitrate_cv_percent
    }

    /// Determines the inactive state based on manual override or packet timeout.
    fn determine_inactive_state(&self, now: Instant) -> bool {
        if self.manual_pause {
            return true;
        }
        if now.saturating_duration_since(self.last_packet_at) > INACTIVE_TIMEOUT {
            return true;
        }
        false
    }

    /// Assesses stream quality based on loss, jitter, AND bitrate stability.
    fn determine_stream_quality(&self) -> StreamQuality {
        if !self.loss_window.is_ready() {
            return StreamQuality::Good; // Not enough data, assume Good.
        }

        // For bitrate stability, only enforce if we have enough history
        let check_bitrate_stability = self.bitrate_history.is_ready();

        match self.current_quality {
            StreamQuality::Excellent | StreamQuality::Good => {
                // Check if ANY metric is bad
                let has_bad_loss = self.raw_loss_percent > self.thresholds.become_bad_loss_percent;
                let has_bad_jitter = self.raw_jitter_ms > self.thresholds.become_bad_jitter_ms;
                let has_bad_bitrate = check_bitrate_stability
                    && self.raw_bitrate_cv_percent > self.thresholds.become_bad_bitrate_cv_percent;

                if has_bad_loss || has_bad_jitter || has_bad_bitrate {
                    return StreamQuality::Bad;
                }

                // Check if ALL metrics are excellent
                let has_excellent_loss =
                    self.raw_loss_percent <= self.thresholds.become_excellent_loss_percent;
                let has_excellent_jitter =
                    self.raw_jitter_ms < self.thresholds.become_excellent_jitter_ms;
                let has_excellent_bitrate = !check_bitrate_stability
                    || self.raw_bitrate_cv_percent
                        < self.thresholds.become_excellent_bitrate_cv_percent;

                if has_excellent_loss && has_excellent_jitter && has_excellent_bitrate {
                    return StreamQuality::Excellent;
                }

                // Otherwise maintain current state
                self.current_quality
            }
            StreamQuality::Bad => {
                // To recover to Good, ALL metrics must be good
                let has_good_loss =
                    self.raw_loss_percent <= self.thresholds.become_good_loss_percent;
                let has_good_jitter = self.raw_jitter_ms < self.thresholds.become_good_jitter_ms;
                let has_good_bitrate = !check_bitrate_stability
                    || self.raw_bitrate_cv_percent < self.thresholds.become_good_bitrate_cv_percent;

                if has_good_loss && has_good_jitter && has_good_bitrate {
                    StreamQuality::Good
                } else {
                    StreamQuality::Bad
                }
            }
        }
    }

    // --- Internal Calculation Methods ---

    /// Updates derived metrics like bitrate, packet loss, and bitrate stability.
    fn update_derived_metrics(&mut self, now: Instant) {
        // Update bitrate
        let elapsed = now.saturating_duration_since(self.bwe_last_update);
        if elapsed >= Duration::from_millis(500) {
            let elapsed_secs = elapsed.as_secs_f64();
            if elapsed_secs > 0.0 && self.bwe_interval_bytes > 0 {
                let bps = (self.bwe_interval_bytes as f64 * 8.0) / elapsed_secs;
                // Track bitrate for stability calculation
                self.bitrate_history.push(bps);

                if let Some(p99) = self.bitrate_history.percentile(0.99) {
                    self.shared_state
                        .bitrate_bps_p99
                        .store(p99 as u64, Ordering::Relaxed);
                }

                if let Some(p50) = self.bitrate_history.percentile(0.50) {
                    self.shared_state
                        .bitrate_bps_p50
                        .store(p50 as u64, Ordering::Relaxed);
                }
            }
            self.bwe_interval_bytes = 0;
            self.bwe_last_update = now;
        }

        // Update packet loss percentage
        self.raw_loss_percent = self.loss_window.calculate_loss_percent();

        // Update bitrate coefficient of variation
        self.raw_bitrate_cv_percent = self.bitrate_history.coefficient_of_variation_percent();
    }

    fn discover_clock_rate(&mut self, packet: &impl PacketTiming) {
        if self.clock_rate == 0 {
            self.clock_rate = packet.rtp_timestamp().frequency().get();
        }
    }

    fn update_jitter(&mut self, packet: &impl PacketTiming) {
        if self.clock_rate == 0 {
            return;
        }

        let arrival = packet.arrival_timestamp();
        let rtp_time = packet.rtp_timestamp().numer();

        if let (Some(last_arrival), Some(last_rtp_time)) =
            (self.jitter_last_arrival, self.jitter_last_rtp_time)
        {
            // RFC 3550 jitter calculation
            let arrival_diff = arrival
                .saturating_duration_since(last_arrival)
                .as_secs_f64();
            let rtp_diff = rtp_time.wrapping_sub(last_rtp_time) as f64 / self.clock_rate as f64;
            let transit_diff = arrival_diff - rtp_diff;

            // Apply exponential smoothing (1/16 weight for new sample)
            self.jitter_estimate += (transit_diff.abs() - self.jitter_estimate) / 16.0;
            self.raw_jitter_ms = (self.jitter_estimate * 1000.0) as u32;
        }

        self.jitter_last_arrival = Some(arrival);
        self.jitter_last_rtp_time = Some(rtp_time);
    }
}

#[cfg(test)]
mod test {
    use str0m::media::MediaTime;

    use super::*;

    struct TestPacket {
        seq: SeqNo,
        ts: MediaTime,
        at: Instant,
    }

    impl PacketTiming for TestPacket {
        fn seq_no(&self) -> SeqNo {
            self.seq
        }
        fn rtp_timestamp(&self) -> MediaTime {
            self.ts
        }
        fn arrival_timestamp(&self) -> Instant {
            self.at
        }
    }

    fn setup() -> (StreamMonitor, StreamState) {
        let thresholds = HealthThresholds {
            become_bad_loss_percent: 7.0,
            become_bad_jitter_ms: 30,
            become_bad_bitrate_cv_percent: 30.0,
            become_good_loss_percent: 4.0,
            become_good_jitter_ms: 20,
            become_good_bitrate_cv_percent: 20.0,
            ..Default::default()
        };
        let state = StreamState::new(true, 0);
        let monitor = StreamMonitor::new(state.clone(), thresholds);
        (monitor, state)
    }

    #[test]
    fn becomes_active_and_calculates_bitrate() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);
        monitor.poll(start);
        assert!(!state.is_inactive(), "Should become active immediately");

        for i in 0..10 {
            let arrival_time = start + Duration::from_millis(i * 50);
            let pkt = TestPacket {
                seq: (i as u64).into(),
                ts: MediaTime::from_90khz(0),
                at: arrival_time,
            };
            monitor.process_packet(&pkt, 1200);
        }

        let final_time = start + Duration::from_millis(501);
        monitor.poll(final_time);

        let expected_bps = (1200.0 * 10.0 * 8.0) / 0.501;
        let actual_bps = state.bitrate_bps_p99() as f64;
        assert!(
            (actual_bps - expected_bps).abs() < 1000.0,
            "Bitrate calculation should be accurate (expected: {}, actual: {})",
            expected_bps,
            actual_bps
        );
        assert!(!state.is_inactive(), "Should remain active when healthy");
    }

    #[test]
    fn quality_becomes_bad_due_to_high_packet_loss() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);
        monitor.poll(start);

        // Send only even-numbered packets (50% loss) - need to send enough to fill window
        for i in 0..(LOSS_WINDOW_SIZE as u64 * 2) {
            if i % 2 == 0 {
                monitor.process_packet(
                    &TestPacket {
                        seq: i.into(),
                        at: start,
                        ts: MediaTime::from_90khz(0),
                    },
                    100,
                );
            }
        }

        monitor.poll(start);
        assert!(
            !state.is_inactive(),
            "Stream should remain active despite bad quality"
        );
        assert_eq!(
            state.quality(),
            StreamQuality::Bad,
            "Quality should be Bad due to high packet loss ({}%)",
            monitor.get_loss_percent()
        );
    }

    #[test]
    fn quality_becomes_bad_due_to_unstable_bitrate() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);
        monitor.poll(start);

        // Simulate highly fluctuating bitrate
        let bitrates = vec![1000000, 500000, 1200000, 400000, 1100000, 300000];
        for (i, &target_bps) in bitrates
            .iter()
            .cycle()
            .take(BITRATE_HISTORY_SAMPLES + 5)
            .enumerate()
        {
            let bytes_needed = (target_bps / 8) / 2; // Per 500ms
            for j in 0..10 {
                monitor.process_packet(
                    &TestPacket {
                        seq: ((i * 10 + j) as u64).into(),
                        at: start + Duration::from_millis(i as u64 * 500),
                        ts: MediaTime::from_90khz(0),
                    },
                    bytes_needed / 10,
                );
            }
            monitor.poll(start + Duration::from_millis(i as u64 * 500 + 501));
        }

        assert_eq!(
            state.quality(),
            StreamQuality::Bad,
            "Quality should be Bad due to unstable bitrate (CV: {}%)",
            monitor.get_bitrate_cv_percent()
        );
    }

    #[test]
    fn quality_recovers_from_bad_to_good() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);
        monitor.poll(start);

        let mut seq_counter: u64 = 0;

        // Create high loss condition - send enough to fill the window
        for _ in 0..(LOSS_WINDOW_SIZE * 2) {
            if seq_counter % 2 == 0 {
                // 50% loss
                monitor.process_packet(
                    &TestPacket {
                        seq: seq_counter.into(),
                        at: start,
                        ts: MediaTime::from_90khz(0),
                    },
                    100,
                );
            }
            seq_counter += 1;
        }
        monitor.poll(start);
        assert_eq!(
            state.quality(),
            StreamQuality::Bad,
            "Pre-condition: Quality should be Bad due to loss ({}%)",
            monitor.get_loss_percent()
        );

        // Send a full window of good packets to bring loss down
        for _ in 0..LOSS_WINDOW_SIZE {
            monitor.process_packet(
                &TestPacket {
                    seq: seq_counter.into(),
                    at: start,
                    ts: MediaTime::from_90khz(0),
                },
                100,
            );
            seq_counter += 1;
        }
        monitor.poll(start);
        assert_eq!(
            state.quality(),
            StreamQuality::Good,
            "Should recover to Good after a window of good packets (loss: {}%)",
            monitor.get_loss_percent()
        );
    }

    #[test]
    fn becomes_inactive_due_to_timeout() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);
        monitor.poll(start);

        monitor.process_packet(
            &TestPacket {
                seq: 0.into(),
                ts: MediaTime::from_90khz(0),
                at: start,
            },
            100,
        );
        monitor.poll(start);
        assert!(!state.is_inactive(), "Should be active after one packet");

        let future_time = start + INACTIVE_TIMEOUT + Duration::from_millis(1);
        monitor.poll(future_time);
        assert!(state.is_inactive(), "Should become inactive after timeout");
    }

    #[test]
    fn handles_sequence_number_wraparound() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);

        // Fill the window to establish a baseline
        let start_seq = (u16::MAX - (LOSS_WINDOW_SIZE as u16)) as u64;
        for i in 0..(LOSS_WINDOW_SIZE as u64 + 100) {
            let seq: SeqNo = start_seq.wrapping_add(i).into();
            monitor.process_packet(
                &TestPacket {
                    seq,
                    at: start,
                    ts: MediaTime::from_90khz(0),
                },
                100,
            );
        }

        monitor.poll(start);
        assert!(
            monitor.get_loss_percent() < 1.0,
            "Should handle wraparound without detecting false losses (loss: {}%)",
            monitor.get_loss_percent()
        );
    }

    #[test]
    #[ignore]
    fn handles_reordered_packets() {
        let (mut monitor, _state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);

        // Send packets out of order - need enough to fill the window
        for base in (0..(LOSS_WINDOW_SIZE * 2)).step_by(10) {
            let sequences = vec![0, 2, 1, 4, 3, 6, 5, 8, 7, 9];
            for &offset in &sequences {
                let seq = (base + offset) as u64;
                monitor.process_packet(
                    &TestPacket {
                        seq: seq.into(),
                        at: start,
                        ts: MediaTime::from_90khz(0),
                    },
                    100,
                );
            }
        }

        monitor.poll(start);
        assert!(
            monitor.get_loss_percent() < 1.0,
            "Should handle reordered packets without detecting false losses (loss: {}%)",
            monitor.get_loss_percent()
        );
    }

    #[test]
    fn excellent_quality_requires_stable_bitrate() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);
        monitor.poll(start);

        // Send stable bitrate with low loss and jitter
        for i in 0..(BITRATE_HISTORY_SAMPLES + 10) {
            // Send consistent packets for stable bitrate (1 Mbps)
            for j in 0..25 {
                monitor.process_packet(
                    &TestPacket {
                        seq: ((i * 25 + j) as u64).into(),
                        at: start + Duration::from_millis(i as u64 * 500 + j as u64 * 20),
                        ts: MediaTime::from_90khz(0),
                    },
                    5000, // 5KB every 20ms = ~2 Mbps
                );
            }
            monitor.poll(start + Duration::from_millis(i as u64 * 500 + 501));
        }

        assert_eq!(
            state.quality(),
            StreamQuality::Excellent,
            "Quality should be Excellent with stable bitrate (CV: {}%, loss: {}%, jitter: {}ms)",
            monitor.get_bitrate_cv_percent(),
            monitor.get_loss_percent(),
            monitor.get_jitter_ms()
        );
    }
}
