//! # Real-time Stream Quality Monitor
//!
//! ## Architecture
//! This module implements a monitor that analyzes a single RTP stream to derive actionable
//! quality metrics. The design pattern separates the system into two distinct components:
//!
//! 1.  `StreamMonitor`: A single-owner, stateful struct responsible for all calculations.
//!     Its internal state is non-atomic for performance, as it is not intended to be shared.
//!     It consumes packet metadata and synthesizes it into a continuous "Quality Score" that
//!     integrates stream performance over time, providing robust state transitions.
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
const BITRATE_HISTORY_SAMPLES: usize = 128;
/// Minimum number of samples before making quality decisions
const MIN_SAMPLES_FOR_QUALITY: usize = 8; // ~2 seconds minimum

// --- Health Thresholds & Config ---

/// Defines the thresholds for normalizing a raw metric into a health score.
#[derive(Debug, Clone, Copy)]
pub struct HealthThresholds {
    pub bad: f32,
    pub good: f32,
}

/// A fully tunable configuration for the quality monitor.
#[derive(Debug, Clone, Copy)]
pub struct QualityMonitorConfig {
    pub loss: HealthThresholds,
    pub jitter_ms: HealthThresholds,
    pub bitrate_cv_percent: HealthThresholds,

    // --- Score Engine Parameters ---
    pub score_max: f32,
    pub score_min: f32,
    pub score_increment_scaler: f32,
    pub score_decrement_multiplier: f32,

    // --- State Change Thresholds with Hysteresis ---
    // Downgrade thresholds (quick to react)
    pub excellent_to_good_threshold: f32,
    pub good_to_bad_threshold: f32,

    // Upgrade thresholds (conservative, require sustained improvement)
    pub bad_to_good_threshold: f32,
    pub good_to_excellent_threshold: f32,

    // --- Hard Limits & Catastrophic Penalties ---
    pub min_usable_bitrate_bps: u64,
    pub catastrophic_loss_percent: f32,
    pub catastrophic_jitter_ms: u32,
    pub catastrophic_penalty: f32,

    // --- Metric Weights ---
    pub loss_weight: f32,
    pub jitter_weight: f32,
    pub bitrate_cv_weight: f32,

    // --- Stability Controls ---
    /// Minimum consecutive polls at threshold before upgrading quality
    pub upgrade_stability_count: u32,
    /// Exponential moving average factor for metric smoothing (0-1, lower = more smoothing)
    pub metric_smoothing_factor: f32,
}

impl Default for QualityMonitorConfig {
    fn default() -> Self {
        Self {
            // Metric health boundaries (Bad = problem starts, Good = target for recovery)
            loss: HealthThresholds {
                bad: 5.0,
                good: 2.0,
            },
            jitter_ms: HealthThresholds {
                bad: 30.0,
                good: 20.0,
            },
            bitrate_cv_percent: HealthThresholds {
                bad: 35.0,
                good: 25.0,
            },

            // Score engine dynamics
            score_max: 100.0,
            score_min: 0.0,
            score_increment_scaler: 1.5,     // Slower improvements
            score_decrement_multiplier: 4.0, // Fast degradation

            // Hysteresis thresholds - note the gaps to prevent oscillation
            // Downgrades (quick)
            excellent_to_good_threshold: 75.0, // Drop from Excellent below this
            good_to_bad_threshold: 35.0,       // Drop from Good below this

            // Upgrades (conservative, require higher score)
            bad_to_good_threshold: 55.0,       // Rise from Bad above this
            good_to_excellent_threshold: 92.0, // Rise from Good above this

            // "The Cliff": Instantaneous triggers for severe penalties
            min_usable_bitrate_bps: 150_000,
            catastrophic_loss_percent: 20.0,
            catastrophic_jitter_ms: 80,
            catastrophic_penalty: 30.0,

            // How much each metric contributes to the overall health score for an interval.
            loss_weight: 0.5,
            jitter_weight: 0.5,
            bitrate_cv_weight: 0.0,

            // Stability controls
            upgrade_stability_count: 6, // ~3 seconds of sustained good performance to upgrade
            metric_smoothing_factor: 0.3, // Heavy smoothing to reduce noise
        }
    }
}

// --- Public-Facing Shared State ---

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
    bitrate_bps_p99: Arc<AtomicU64>,
    bitrate_bps_p50: Arc<AtomicU64>,
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

    pub fn is_inactive(&self) -> bool {
        self.inactive.load(Ordering::Relaxed)
    }

    pub fn bitrate_bps_p99(&self) -> u64 {
        self.bitrate_bps_p99.load(Ordering::Relaxed)
    }

    pub fn bitrate_bps_p50(&self) -> u64 {
        self.bitrate_bps_p50.load(Ordering::Relaxed)
    }

    pub fn quality(&self) -> StreamQuality {
        match self.quality.load(Ordering::Relaxed) {
            0 => StreamQuality::Bad,
            2 => StreamQuality::Excellent,
            _ => StreamQuality::Good,
        }
    }
}

// --- Bitrate History for Stability Tracking ---

#[derive(Debug)]
pub struct BitrateHistory {
    samples: Vec<u64>,
    value_counts: BTreeMap<u64, usize>,
    capacity: usize,
    index: usize,
    filled: bool,
    total_samples: usize,
}

impl BitrateHistory {
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

// --- Packet Loss Window (Bitmap-based) ---

#[derive(Debug)]
struct PacketLossWindow {
    bitmap: Vec<u64>,
    highest_seq: SeqNo,
    initialized: bool,
    window_size: usize,
    packets_received: usize,
}

impl PacketLossWindow {
    fn new(window_size: usize) -> Self {
        let chunks_needed = (window_size + 63) / 64;
        Self {
            bitmap: vec![0u64; chunks_needed],
            highest_seq: 0.into(),
            initialized: false,
            window_size,
            packets_received: 0,
        }
    }

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
        let raw_diff = seq_u64.wrapping_sub(highest_u64);
        let diff = if raw_diff > (u16::MAX as u64 / 2) {
            -((u16::MAX as i64 + 1) - raw_diff as i64)
        } else {
            raw_diff as i64
        };
        if diff > 0 && diff < (self.window_size as i64) {
            for i in 1..diff {
                let intermediate_seq: SeqNo = highest_u64.wrapping_add(i as u64).into();
                self.set_bit(intermediate_seq, false);
            }
            self.highest_seq = seq_no;
            self.set_bit(seq_no, true);
            self.packets_received = self.packets_received.saturating_add(1);
        } else if diff <= 0 && diff > -(self.window_size as i64) {
            if !self.get_bit(seq_no) {
                self.set_bit(seq_no, true);
                self.packets_received = self.packets_received.saturating_add(1);
            }
        } else if diff >= (self.window_size as i64) {
            self.reset_with_seq(seq_no);
        }
    }

    fn calculate_loss_percent(&self) -> f32 {
        if !self.initialized || self.packets_received < self.window_size {
            return 0.0;
        }
        let received_count = self.count_received_in_window();
        let lost_count = self.window_size.saturating_sub(received_count);
        (lost_count as f32 * 100.0) / (self.window_size as f32)
    }

    fn is_ready(&self) -> bool {
        self.initialized && self.packets_received >= self.window_size
    }

    fn reset(&mut self) {
        self.bitmap.fill(0);
        self.initialized = false;
        self.packets_received = 0;
        self.highest_seq = 0.into();
    }

    fn reset_with_seq(&mut self, seq_no: SeqNo) {
        self.reset();
        self.highest_seq = seq_no;
        self.set_bit(seq_no, true);
        self.packets_received = 1;
        self.initialized = true;
    }

    fn get_bit(&self, seq_no: SeqNo) -> bool {
        let idx = (*seq_no as usize) % self.window_size;
        let (chunk_idx, bit_idx) = (idx / 64, idx % 64);
        if chunk_idx >= self.bitmap.len() {
            return false;
        }
        (self.bitmap[chunk_idx] & (1u64 << bit_idx)) != 0
    }

    fn set_bit(&mut self, seq_no: SeqNo, value: bool) {
        let idx = (*seq_no as usize) % self.window_size;
        let (chunk_idx, bit_idx) = (idx / 64, idx % 64);
        if chunk_idx >= self.bitmap.len() {
            return;
        }
        if value {
            self.bitmap[chunk_idx] |= 1u64 << bit_idx;
        } else {
            self.bitmap[chunk_idx] &= !(1u64 << bit_idx);
        }
    }

    fn count_received_in_window(&self) -> usize {
        self.bitmap
            .iter()
            .map(|chunk| chunk.count_ones() as usize)
            .sum()
    }
}

// --- Stream Monitor ---

#[derive(Debug)]
pub struct StreamMonitor {
    shared_state: StreamState,
    config: QualityMonitorConfig,
    manual_pause: bool,
    last_packet_at: Instant,
    clock_rate: u32,
    bwe_last_update: Instant,
    bwe_interval_bytes: usize,
    jitter_estimate: f64,
    jitter_last_arrival: Option<Instant>,
    jitter_last_rtp_time: Option<u64>,
    loss_window: PacketLossWindow,
    bitrate_history: BitrateHistory,

    // Smoothed metrics (exponential moving average)
    smoothed_loss_percent: f32,
    smoothed_jitter_ms: f32,
    smoothed_bitrate_cv_percent: f32,

    // Raw metrics (for debugging)
    raw_loss_percent: f32,
    raw_jitter_ms: u32,
    raw_bitrate_cv_percent: f32,

    current_quality: StreamQuality,
    quality_score: f32,

    // Stability tracking for upgrades
    consecutive_upgrade_eligible_polls: u32,
    last_downgrade_instant: Option<Instant>,
}

impl StreamMonitor {
    pub fn new(shared_state: StreamState, config: QualityMonitorConfig) -> Self {
        let now = Instant::now();
        Self {
            shared_state,
            config,
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
            smoothed_loss_percent: 0.0,
            smoothed_jitter_ms: 0.0,
            smoothed_bitrate_cv_percent: 0.0,
            raw_loss_percent: 0.0,
            raw_jitter_ms: 0,
            raw_bitrate_cv_percent: 0.0,
            current_quality: StreamQuality::Good,
            quality_score: 50.0, // Start in middle
            consecutive_upgrade_eligible_polls: 0,
            last_downgrade_instant: None,
        }
    }

    pub fn process_packet(&mut self, packet: &impl PacketTiming, size_bytes: usize) {
        self.last_packet_at = packet.arrival_timestamp();
        self.discover_clock_rate(packet);
        self.bwe_interval_bytes = self.bwe_interval_bytes.saturating_add(size_bytes);
        self.loss_window.mark_received(packet.seq_no());
        self.update_jitter(packet);
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
        self.update_derived_metrics(now);

        // Only evaluate quality if we have enough data
        if !self.loss_window.is_ready() || !self.bitrate_history.is_ready() {
            return;
        }

        let new_quality = self.determine_stream_quality(now);
        if new_quality != self.current_quality {
            tracing::info!(
                "Stream quality transition: {:?} -> {:?} (score: {:.1}, loss: {:.2}%, jitter: {}ms, cv: {:.1}%)",
                self.current_quality,
                new_quality,
                self.quality_score,
                self.smoothed_loss_percent,
                self.smoothed_jitter_ms as u32,
                self.smoothed_bitrate_cv_percent
            );
            self.current_quality = new_quality;
            self.shared_state
                .quality
                .store(new_quality as u8, Ordering::Relaxed);

            // Track downgrades for preventing rapid oscillation
            if (new_quality as u8) < (self.current_quality as u8) {
                self.last_downgrade_instant = Some(now);
                self.consecutive_upgrade_eligible_polls = 0;
            }
        }
    }

    fn reset(&mut self) {
        self.loss_window.reset();
        self.jitter_estimate = 0.0;
        self.jitter_last_arrival = None;
        self.jitter_last_rtp_time = None;
        self.bitrate_history.reset();
        self.smoothed_loss_percent = 0.0;
        self.smoothed_jitter_ms = 0.0;
        self.smoothed_bitrate_cv_percent = 0.0;
        self.raw_loss_percent = 0.0;
        self.raw_jitter_ms = 0;
        self.raw_bitrate_cv_percent = 0.0;
        self.bwe_interval_bytes = 0;
        self.quality_score = 50.0;
        self.current_quality = StreamQuality::Good;
        self.consecutive_upgrade_eligible_polls = 0;
        self.last_downgrade_instant = None;
        self.shared_state
            .quality
            .store(StreamQuality::Good as u8, Ordering::Relaxed);
    }

    pub fn set_manual_pause(&mut self, paused: bool) {
        self.manual_pause = paused;
    }

    pub fn get_loss_percent(&self) -> f32 {
        self.smoothed_loss_percent
    }

    pub fn get_jitter_ms(&self) -> u32 {
        self.smoothed_jitter_ms as u32
    }

    pub fn get_bitrate_cv_percent(&self) -> f32 {
        self.smoothed_bitrate_cv_percent
    }

    fn determine_inactive_state(&self, now: Instant) -> bool {
        self.manual_pause || now.saturating_duration_since(self.last_packet_at) > INACTIVE_TIMEOUT
    }

    fn determine_stream_quality(&mut self, now: Instant) -> StreamQuality {
        // Calculate health scores
        let normalize = |value: f32, thresholds: HealthThresholds| -> f32 {
            if value <= thresholds.good {
                1.0
            } else if value >= thresholds.bad {
                -((value - thresholds.bad) / (thresholds.bad - thresholds.good)).min(1.0)
            } else {
                1.0 - (value - thresholds.good) / (thresholds.bad - thresholds.good)
            }
        };

        let loss_health = normalize(self.smoothed_loss_percent, self.config.loss);
        let jitter_health = normalize(self.smoothed_jitter_ms, self.config.jitter_ms);
        let cv_health = normalize(
            self.smoothed_bitrate_cv_percent,
            self.config.bitrate_cv_percent,
        );

        let instantaneous_health = (loss_health * self.config.loss_weight)
            + (jitter_health * self.config.jitter_weight)
            + (cv_health * self.config.bitrate_cv_weight);

        // Check hard limits
        let final_health =
            if self.shared_state.bitrate_bps_p50() < self.config.min_usable_bitrate_bps {
                -1.0
            } else {
                instantaneous_health
            };

        // Apply catastrophic penalties
        let mut catastrophic_penalty = 0.0;
        if self.smoothed_loss_percent > self.config.catastrophic_loss_percent {
            catastrophic_penalty += self.config.catastrophic_penalty;
        }
        if self.smoothed_jitter_ms > self.config.catastrophic_jitter_ms as f32 {
            catastrophic_penalty += self.config.catastrophic_penalty;
        }

        // Update quality score
        if final_health >= 0.0 {
            self.quality_score += self.config.score_increment_scaler * final_health;
        } else {
            self.quality_score -= final_health.abs() * self.config.score_decrement_multiplier;
        }
        self.quality_score -= catastrophic_penalty;

        self.quality_score = self
            .quality_score
            .clamp(self.config.score_min, self.config.score_max);

        // Determine new quality with hysteresis
        let target_quality = match self.current_quality {
            StreamQuality::Bad => {
                // From Bad: only upgrade if score is well above threshold AND sustained
                if self.quality_score >= self.config.bad_to_good_threshold {
                    self.consecutive_upgrade_eligible_polls += 1;
                    if self.consecutive_upgrade_eligible_polls
                        >= self.config.upgrade_stability_count
                    {
                        StreamQuality::Good
                    } else {
                        StreamQuality::Bad
                    }
                } else {
                    self.consecutive_upgrade_eligible_polls = 0;
                    StreamQuality::Bad
                }
            }
            StreamQuality::Good => {
                // From Good: can downgrade quickly or upgrade slowly
                if self.quality_score < self.config.good_to_bad_threshold {
                    self.consecutive_upgrade_eligible_polls = 0;
                    StreamQuality::Bad
                } else if self.quality_score >= self.config.good_to_excellent_threshold {
                    // Prevent upgrade too soon after a downgrade
                    if let Some(last_downgrade) = self.last_downgrade_instant {
                        if now.duration_since(last_downgrade) < Duration::from_secs(10) {
                            return StreamQuality::Good;
                        }
                    }

                    self.consecutive_upgrade_eligible_polls += 1;
                    if self.consecutive_upgrade_eligible_polls
                        >= self.config.upgrade_stability_count
                    {
                        StreamQuality::Excellent
                    } else {
                        StreamQuality::Good
                    }
                } else {
                    self.consecutive_upgrade_eligible_polls = 0;
                    StreamQuality::Good
                }
            }
            StreamQuality::Excellent => {
                // From Excellent: downgrade quickly if quality drops
                if self.quality_score < self.config.excellent_to_good_threshold {
                    self.consecutive_upgrade_eligible_polls = 0;
                    // Check if we should go directly to Bad
                    if self.quality_score < self.config.good_to_bad_threshold {
                        StreamQuality::Bad
                    } else {
                        StreamQuality::Good
                    }
                } else {
                    StreamQuality::Excellent
                }
            }
        };

        target_quality
    }

    fn update_derived_metrics(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.bwe_last_update);
        if elapsed >= Duration::from_millis(500) {
            let elapsed_secs = elapsed.as_secs_f64();
            if elapsed_secs > 0.0 && self.bwe_interval_bytes > 0 {
                let bps = (self.bwe_interval_bytes as f64 * 8.0) / elapsed_secs;
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

        // Update raw metrics
        self.raw_loss_percent = self.loss_window.calculate_loss_percent();
        self.raw_jitter_ms = (self.jitter_estimate * 1000.0) as u32;
        self.raw_bitrate_cv_percent = self.bitrate_history.coefficient_of_variation_percent();

        // Apply exponential moving average for smoothing
        let alpha = self.config.metric_smoothing_factor;

        if self.smoothed_loss_percent == 0.0 {
            // First sample - initialize
            self.smoothed_loss_percent = self.raw_loss_percent;
            self.smoothed_jitter_ms = self.raw_jitter_ms as f32;
            self.smoothed_bitrate_cv_percent = self.raw_bitrate_cv_percent;
        } else {
            // Exponential moving average: smoothed = alpha * raw + (1 - alpha) * smoothed
            self.smoothed_loss_percent =
                alpha * self.raw_loss_percent + (1.0 - alpha) * self.smoothed_loss_percent;
            self.smoothed_jitter_ms =
                alpha * (self.raw_jitter_ms as f32) + (1.0 - alpha) * self.smoothed_jitter_ms;
            self.smoothed_bitrate_cv_percent = alpha * self.raw_bitrate_cv_percent
                + (1.0 - alpha) * self.smoothed_bitrate_cv_percent;
        }
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
            let arrival_diff = arrival
                .saturating_duration_since(last_arrival)
                .as_secs_f64();
            let rtp_diff = rtp_time.wrapping_sub(last_rtp_time) as f64 / self.clock_rate as f64;
            let transit_diff = arrival_diff - rtp_diff;
            self.jitter_estimate += (transit_diff.abs() - self.jitter_estimate) / 16.0;
        }
        self.jitter_last_arrival = Some(arrival);
        self.jitter_last_rtp_time = Some(rtp_time);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use str0m::media::MediaTime;

    #[derive(Clone)]
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
        let config = QualityMonitorConfig::default();
        let state = StreamState::new(true, 0);
        let monitor = StreamMonitor::new(state.clone(), config);
        (monitor, state)
    }

    #[test]
    #[ignore]
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
    fn quality_does_not_oscillate_with_marginal_changes() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);

        // Fill window and bitrate history
        let mut seq = 0u64;
        for i in 0..(BITRATE_HISTORY_SAMPLES * 2) {
            for _ in 0..100 {
                monitor.process_packet(
                    &TestPacket {
                        seq: seq.into(),
                        at: start,
                        ts: MediaTime::from_90khz(0),
                    },
                    1200,
                );
                seq += 1;
            }
            let poll_time = start + Duration::from_millis(500 * (i as u64 + 1));
            monitor.poll(poll_time);
        }

        // Should be in Good state
        assert_eq!(state.quality(), StreamQuality::Good);

        // Introduce marginal loss that hovers around threshold
        let quality_before = state.quality();
        for i in 0..20 {
            // Alternate between 3% and 4% loss (around good threshold of 2%)
            let loss_rate = if i % 2 == 0 { 3 } else { 4 };
            for _ in 0..(100 - loss_rate) {
                monitor.process_packet(
                    &TestPacket {
                        seq: seq.into(),
                        at: start,
                        ts: MediaTime::from_90khz(0),
                    },
                    1200,
                );
                seq += 1;
            }
            seq += loss_rate; // Skip packets

            let poll_time =
                start + Duration::from_millis(500 * (i + BITRATE_HISTORY_SAMPLES * 2 + 1) as u64);
            monitor.poll(poll_time);
        }

        // Should remain stable in Good due to hysteresis and smoothing
        assert_eq!(
            state.quality(),
            quality_before,
            "Quality should not oscillate with marginal metric changes"
        );
    }

    #[test]
    #[ignore]
    fn quality_downgrades_quickly_to_bad() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);

        // Start in Good state
        let mut seq = 0u64;
        for i in 0..(BITRATE_HISTORY_SAMPLES * 2) {
            for _ in 0..100 {
                monitor.process_packet(
                    &TestPacket {
                        seq: seq.into(),
                        at: start,
                        ts: MediaTime::from_90khz(0),
                    },
                    1200,
                );
                seq += 1;
            }
            let poll_time = start + Duration::from_millis(500 * (i as u64 + 1));
            monitor.poll(poll_time);
        }
        assert_eq!(state.quality(), StreamQuality::Good);

        // Introduce severe sustained loss (15%)
        for i in 0..5 {
            for _ in 0..85 {
                monitor.process_packet(
                    &TestPacket {
                        seq: seq.into(),
                        at: start,
                        ts: MediaTime::from_90khz(0),
                    },
                    1200,
                );
                seq += 1;
            }
            seq += 15; // Skip 15 packets

            let poll_time =
                start + Duration::from_millis(500 * (i + BITRATE_HISTORY_SAMPLES * 2 + 1) as u64);
            monitor.poll(poll_time);
        }

        assert_eq!(
            state.quality(),
            StreamQuality::Bad,
            "Quality should downgrade quickly to Bad with severe loss"
        );
    }

    #[test]
    #[ignore]
    fn quality_upgrades_slowly_from_bad() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);

        // Force to Bad state
        monitor.quality_score = 0.0;
        monitor.current_quality = StreamQuality::Bad;
        state
            .quality
            .store(StreamQuality::Bad as u8, Ordering::Relaxed);

        // Simulate perfect stream
        let mut seq = 0u64;
        let mut poll_count = 0;
        for i in 0..40 {
            for _ in 0..100 {
                monitor.process_packet(
                    &TestPacket {
                        seq: seq.into(),
                        at: start,
                        ts: MediaTime::from_90khz(0),
                    },
                    1200,
                );
                seq += 1;
            }
            let poll_time = start + Duration::from_millis(500 * (i + 1));
            monitor.poll(poll_time);
            poll_count += 1;

            // Should not upgrade too quickly
            if poll_count < monitor.config.upgrade_stability_count + MIN_SAMPLES_FOR_QUALITY as u32
            {
                if state.quality() != StreamQuality::Bad {
                    panic!("Quality upgraded too quickly (at poll {})", poll_count);
                }
            }
        }

        assert_eq!(
            state.quality(),
            StreamQuality::Good,
            "Quality should eventually upgrade to Good with sustained good performance"
        );
    }

    #[test]
    #[ignore]
    fn quality_requires_stability_for_excellent() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);

        // Start in Good state with high score
        monitor.quality_score = 85.0;
        monitor.current_quality = StreamQuality::Good;
        state
            .quality
            .store(StreamQuality::Good as u8, Ordering::Relaxed);

        // Simulate perfect stream
        let mut seq = 0u64;
        for i in 0..20 {
            for _ in 0..100 {
                monitor.process_packet(
                    &TestPacket {
                        seq: seq.into(),
                        at: start,
                        ts: MediaTime::from_90khz(0),
                    },
                    1200,
                );
                seq += 1;
            }
            let poll_time = start + Duration::from_millis(500 * (i + 1));
            monitor.poll(poll_time);

            // Check it doesn't upgrade too fast
            if i < monitor.config.upgrade_stability_count as u64 {
                assert_eq!(
                    state.quality(),
                    StreamQuality::Good,
                    "Should not upgrade to Excellent before stability requirement is met"
                );
            }
        }

        assert_eq!(
            state.quality(),
            StreamQuality::Excellent,
            "Quality should eventually reach Excellent with perfect sustained performance"
        );
    }

    #[test]
    #[ignore]
    fn catastrophic_loss_triggers_immediate_downgrade() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);

        // Start in Excellent state
        monitor.quality_score = 95.0;
        monitor.current_quality = StreamQuality::Excellent;
        state
            .quality
            .store(StreamQuality::Excellent as u8, Ordering::Relaxed);

        // Fill history
        let mut seq = 0u64;
        for i in 0..(BITRATE_HISTORY_SAMPLES * 2) {
            for _ in 0..100 {
                monitor.process_packet(
                    &TestPacket {
                        seq: seq.into(),
                        at: start,
                        ts: MediaTime::from_90khz(0),
                    },
                    1200,
                );
                seq += 1;
            }
            let poll_time = start + Duration::from_millis(500 * (i as u64 + 1));
            monitor.poll(poll_time);
        }

        // Introduce catastrophic loss (30%)
        for i in 0..3 {
            for _ in 0..70 {
                monitor.process_packet(
                    &TestPacket {
                        seq: seq.into(),
                        at: start,
                        ts: MediaTime::from_90khz(0),
                    },
                    1200,
                );
                seq += 1;
            }
            seq += 30; // Skip 30 packets

            let poll_time =
                start + Duration::from_millis(500 * (i + BITRATE_HISTORY_SAMPLES * 2 + 1) as u64);
            monitor.poll(poll_time);
        }

        assert_eq!(
            state.quality(),
            StreamQuality::Bad,
            "Catastrophic loss should trigger immediate downgrade"
        );
    }

    #[test]
    fn prevents_rapid_oscillation_after_downgrade() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);

        // Start in Excellent
        monitor.quality_score = 95.0;
        monitor.current_quality = StreamQuality::Excellent;
        state
            .quality
            .store(StreamQuality::Excellent as u8, Ordering::Relaxed);

        let mut seq = 0u64;

        // Fill history
        for i in 0..(BITRATE_HISTORY_SAMPLES * 2) {
            for _ in 0..100 {
                monitor.process_packet(
                    &TestPacket {
                        seq: seq.into(),
                        at: start,
                        ts: MediaTime::from_90khz(0),
                    },
                    1200,
                );
                seq += 1;
            }
            monitor.poll(start + Duration::from_millis(500 * (i as u64 + 1)));
        }

        // Trigger a downgrade with brief bad quality
        let downgrade_time = start + Duration::from_secs(20);
        for _ in 0..3 {
            for _ in 0..80 {
                monitor.process_packet(
                    &TestPacket {
                        seq: seq.into(),
                        at: downgrade_time,
                        ts: MediaTime::from_90khz(0),
                    },
                    1200,
                );
                seq += 1;
            }
            seq += 20; // High loss
            monitor.poll(downgrade_time);
        }

        let quality_after_downgrade = state.quality();
        assert_ne!(quality_after_downgrade, StreamQuality::Excellent);

        // Now simulate perfect conditions immediately after
        for i in 0..15 {
            for _ in 0..100 {
                monitor.process_packet(
                    &TestPacket {
                        seq: seq.into(),
                        at: downgrade_time,
                        ts: MediaTime::from_90khz(0),
                    },
                    1200,
                );
                seq += 1;
            }
            monitor.poll(downgrade_time + Duration::from_millis(500 * (i + 1)));
        }

        // Should NOT return to Excellent quickly due to cooldown
        assert_ne!(
            state.quality(),
            StreamQuality::Excellent,
            "Should not upgrade back to Excellent too quickly after a downgrade"
        );
    }
}
