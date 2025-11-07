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
use str0m::bwe::Bitrate;
use str0m::media::{Frequency, MediaTime};
use str0m::rtp::SeqNo;
use tokio::time::Instant;

use crate::rtp::{PacketTiming, VIDEO_FREQUENCY};

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
    pub catastrophic_loss_percent: f32,
    pub catastrophic_jitter_ms: u32,
    pub catastrophic_penalty: f32,

    // --- Metric Weights ---
    pub loss_weight: f32,
    pub jitter_weight: f32,

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
                bad: 50.0,
                good: 30.0,
            },

            // Score engine dynamics
            score_max: 100.0,
            score_min: 0.0,
            score_increment_scaler: 3.0,     // Slower improvements
            score_decrement_multiplier: 4.0, // Fast degradation

            // Hysteresis thresholds - note the gaps to prevent oscillation
            // Downgrades (quick)
            excellent_to_good_threshold: 75.0, // Drop from Excellent below this
            good_to_bad_threshold: 40.0,       // Drop from Good below this

            // Upgrades (conservative, require higher score)
            bad_to_good_threshold: 60.0,       // Rise from Bad above this
            good_to_excellent_threshold: 88.0, // Rise from Good above this

            // "The Cliff": Instantaneous triggers for severe penalties
            catastrophic_loss_percent: 20.0,
            catastrophic_jitter_ms: 100,
            catastrophic_penalty: 30.0,

            // How much each metric contributes to the overall health score for an interval.
            loss_weight: 0.6,
            jitter_weight: 0.4,

            // Stability controls
            upgrade_stability_count: 10, // ~2 seconds of sustained good performance to upgrade
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
    bwe_bps_ewma: f64,
    jitter_estimate: f64,
    jitter_last_arrival: Option<Instant>,
    jitter_last_rtp_time: Option<u64>,
    loss_window: PacketLossWindow,
    bitrate_history: BitrateHistory,

    // Smoothed metrics (exponential moving average)
    smoothed_loss_percent: f32,
    smoothed_jitter_ms: f32,
    smoothed_bitrate_bps: f64,

    // Raw metrics (for debugging)
    raw_loss_percent: f32,
    raw_jitter_ms: u32,

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
            bwe_bps_ewma: 0.0,
            jitter_estimate: 0.0,
            jitter_last_arrival: None,
            jitter_last_rtp_time: None,
            loss_window: PacketLossWindow::new(LOSS_WINDOW_SIZE),
            bitrate_history: BitrateHistory::new(BITRATE_HISTORY_SAMPLES),
            smoothed_loss_percent: 0.0,
            smoothed_jitter_ms: 0.0,
            smoothed_bitrate_bps: 0.0,
            raw_loss_percent: 0.0,
            raw_jitter_ms: 0,
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
                "Stream quality transition: {:?} -> {:?} (score: {:.1}, loss: {:.2}%, jitter: {}ms, bitrate: {})",
                self.current_quality,
                new_quality,
                self.quality_score,
                self.smoothed_loss_percent,
                self.smoothed_jitter_ms as u32,
                Bitrate::from(self.smoothed_bitrate_bps),
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
        self.raw_jitter_ms = 0;
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

        let final_health =
            (loss_health * self.config.loss_weight) + (jitter_health * self.config.jitter_weight);

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
        const EWMA_ALPHA: f64 = 0.8;
        let elapsed = now.saturating_duration_since(self.bwe_last_update);

        // Short Window Estimate
        if elapsed >= Duration::from_millis(200) {
            let elapsed_secs = elapsed.as_secs_f64();
            if elapsed_secs > 0.0 && self.bwe_interval_bytes > 0 {
                let bps = (self.bwe_interval_bytes as f64 * 8.0) / elapsed_secs;

                if self.bwe_bps_ewma == 0.0 {
                    self.bwe_bps_ewma = bps;
                } else {
                    self.bwe_bps_ewma = (1.0 - EWMA_ALPHA) * self.bwe_bps_ewma + EWMA_ALPHA * bps;
                }
                self.bitrate_history.push(bps);

                // P99 is too noisy + add headroom
                let p95 = 1.5 * self.bitrate_history.percentile(0.95).unwrap_or_default();

                let bps = p95.max(self.bwe_bps_ewma);
                self.smoothed_bitrate_bps = bps;
                self.shared_state
                    .bitrate_bps
                    .store(bps as u64, Ordering::Relaxed);
            }
            self.bwe_interval_bytes = 0;
            self.bwe_last_update = now;
        }

        // Update raw metrics
        self.raw_loss_percent = self.loss_window.calculate_loss_percent();
        self.raw_jitter_ms = (self.jitter_estimate * 1000.0) as u32;

        // Apply exponential moving average for smoothing
        let alpha = self.config.metric_smoothing_factor;

        if self.smoothed_loss_percent == 0.0 {
            // First sample - initialize
            self.smoothed_loss_percent = self.raw_loss_percent;
            self.smoothed_jitter_ms = self.raw_jitter_ms as f32;
        } else {
            // Exponential moving average: smoothed = alpha * raw + (1 - alpha) * smoothed
            self.smoothed_loss_percent =
                alpha * self.raw_loss_percent + (1.0 - alpha) * self.smoothed_loss_percent;
            self.smoothed_jitter_ms =
                alpha * (self.raw_jitter_ms as f32) + (1.0 - alpha) * self.smoothed_jitter_ms;
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
            // If the RTP timestamp is the same as the last packet, it's part of the same frame burst.
            // Do not calculate jitter for these packets to avoid artificial spikes.
            if rtp_time == last_rtp_time {
                self.jitter_last_arrival = Some(arrival);
                return; // Skip jitter calculation
            }

            let arrival_diff = arrival
                .saturating_duration_since(last_arrival)
                .as_secs_f64();
            let rtp_diff = rtp_time.wrapping_sub(last_rtp_time) as f64 / self.clock_rate as f64;
            let transit_diff = arrival_diff - rtp_diff;

            // Use a slightly more responsive smoothing factor
            self.jitter_estimate += (transit_diff.abs() - self.jitter_estimate) / 8.0;
        }

        self.jitter_last_arrival = Some(arrival);
        self.jitter_last_rtp_time = Some(rtp_time);
    }
}

#[derive(Clone, Debug)]
struct PacketStatus {
    seqno: SeqNo,
    arrival: Instant,
    rtp_ts: MediaTime,
}

#[derive(Debug, Copy, Clone)]
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
}

impl DeltaDeltaState {
    fn init<T: PacketTiming>(packet: &T) -> Self {
        let seq = packet.seq_no();
        let rtp_ts = packet.rtp_timestamp();
        let now = packet.arrival_timestamp();

        Self {
            head: seq.wrapping_add(1).into(),
            tail: seq,
            frequency: rtp_ts.frequency(),
            last_rtp_ts: rtp_ts,
            last_arrival: now,
            last_skew: 0.0,
            dod_ewma: 0.0,
            dod_abs_ewma: 0.0,
            packets_actual: 0,
            packets_expected: 0,
        }
    }

    fn packet_loss(&self) -> f64 {
        self.packets_actual as f64 / self.packets_expected as f64
    }

    fn advance(&mut self, pkt: &PacketStatus, alpha: f64) {
        let actual_ms = pkt.arrival.duration_since(self.last_arrival).as_secs_f64() * 1000.0;

        let expected_ms = (pkt.rtp_ts.numer().wrapping_sub(self.last_rtp_ts.numer()) as f64)
            * 1000.0
            / self.frequency.get() as f64;

        let skew = actual_ms - expected_ms;
        let dod = skew - self.last_skew;

        self.dod_ewma = alpha * dod + (1.0 - alpha) * self.dod_ewma;
        self.dod_abs_ewma = alpha * dod.abs() + (1.0 - alpha) * self.dod_abs_ewma;

        self.last_skew = skew;
        self.last_arrival = pkt.arrival;
        self.last_rtp_ts = pkt.rtp_ts;
        self.packets_actual += 1;
        self.packets_expected += 1;
    }
}

#[derive(Debug)]
pub struct DeltaDeltaMonitor {
    buffer: Vec<Option<PacketStatus>>,
    state: Option<DeltaDeltaState>,
    alpha: f64,
}

impl DeltaDeltaMonitor {
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

    pub fn new(cap: usize) -> Self {
        Self {
            buffer: vec![None; cap],
            state: None,
            alpha: 0.1,
        }
    }

    pub fn quality_score(&self) -> f64 {
        let Some(state) = &self.state else {
            return 100.0;
        };

        if state.packets_expected < Self::QUALITY_MIN_PACKETS {
            return 100.0;
        }

        // 1. Calculate the raw metric values.
        let loss_percentage = (1.0 - state.packet_loss()) * 100.0;
        let instability_ms = state.dod_abs_ewma;
        // We only care about the magnitude of the trend, not its direction.
        let congestion_trend_ms = state.dod_ewma.abs();

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
    pub fn quality(&self) -> StreamQuality {
        let score = self.quality_score();

        if score >= Self::QUALITY_SCORE_EXCELLENT_THRESHOLD {
            StreamQuality::Excellent
        } else if score >= Self::QUALITY_SCORE_GOOD_THRESHOLD {
            StreamQuality::Good
        } else {
            StreamQuality::Bad
        }
    }

    pub fn update<T: PacketTiming>(&mut self, packet: &T) {
        let mut state = *self
            .state
            .get_or_insert_with(|| DeltaDeltaState::init(packet));

        let seq = packet.seq_no();
        if seq < state.tail {
            tracing::warn!(
                "{} is older than the current tail, {}, ignore it",
                seq,
                state.tail
            );
            return;
        }

        let rtp_ts = packet.rtp_timestamp();
        let arrival = packet.arrival_timestamp();

        if seq >= state.head {
            state.head = seq.wrapping_add(1).into();
        }

        // no space
        let tail = *state.tail;
        if !(tail <= *seq && *seq < tail + self.buffer.len() as u64) {
            let new_tail = state.head.wrapping_sub(self.buffer.len() as u64);
            self.process_until(&mut state, new_tail.into());
        }

        *self.packet_mut(seq) = Some(PacketStatus {
            seqno: seq,
            arrival,
            rtp_ts,
        });
        self.process_in_order(&mut state);
        self.state = Some(state);
    }

    fn process_in_order(&mut self, state: &mut DeltaDeltaState) {
        while state.tail < state.head
            && let Some(pkt) = self.packet_mut(state.tail).take()
        {
            state.advance(&pkt, self.alpha);
            state.tail = state.tail.wrapping_add(1).into();
        }
    }

    fn process_until(&mut self, state: &mut DeltaDeltaState, end: SeqNo) {
        assert!(state.tail < end);
        for current in *state.tail..*end {
            let Some(pkt) = self.packet_mut(current.into()).take() else {
                state.packets_expected += 1;
                continue;
            };

            state.advance(&pkt, self.alpha);
        }
        state.tail = end;
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
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::Instant;

    const TEST_CAP: usize = 10;
    const TEST_FREQ: Frequency = Frequency::NINETY_KHZ;
    const MS_PER_PACKET: u64 = 20; // 20ms per packet
    const RTP_TS_PER_PACKET: u64 = (TEST_FREQ.get() as u64 * MS_PER_PACKET) / 1000; // 1800
    //
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
    }

    impl PacketFactory {
        fn new() -> Self {
            Self {
                start_time: Instant::now(),
            }
        }

        // Creates a packet with a given sequence number and an optional jitter in arrival time
        fn at(&self, seq: u64, arrival_jitter_ms: i64) -> TestPacket {
            let rtp_ts = 1000 + (seq.wrapping_sub(1)).wrapping_mul(RTP_TS_PER_PACKET);
            let ideal_arrival_ms = (seq.wrapping_sub(1)).wrapping_mul(MS_PER_PACKET);

            let arrival_time = if arrival_jitter_ms >= 0 {
                self.start_time + Duration::from_millis(ideal_arrival_ms + arrival_jitter_ms as u64)
            } else {
                self.start_time + Duration::from_millis(ideal_arrival_ms)
                    - Duration::from_millis(arrival_jitter_ms.abs() as u64)
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
        let mut monitor = DeltaDeltaMonitor::new(TEST_CAP);
        let factory = PacketFactory::new();
        let pkt = factory.at(1, 0);

        monitor.update(&pkt);

        let state = monitor.state.expect("State should be initialized");
        // FIX: The first packet is immediately processed because it's at the tail.
        // So, the tail should advance past it.
        assert_eq!(
            state.tail,
            2.into(),
            "Tail should advance past the first packet"
        );
        assert_eq!(state.head, 2.into(), "Head should be next expected seq");
        assert_eq!(state.last_rtp_ts, pkt.rtp_timestamp());
        assert_eq!(state.last_arrival, pkt.arrival_timestamp());
    }

    #[test]
    fn test_in_order_processing() {
        let mut monitor = DeltaDeltaMonitor::new(TEST_CAP);
        let factory = PacketFactory::new();

        let pkt1 = factory.at(1, 0);
        monitor.update(&pkt1);
        // FIX: Tail is now 2, as packet 1 was immediately processed.
        assert_eq!(monitor.state.unwrap().tail, 2.into());

        let pkt2 = factory.at(2, 0);
        monitor.update(&pkt2);

        // FIX: With packet 2's arrival, the tail (at 2) can be processed, advancing tail to 3.
        let state = monitor.state.expect("State should exist");
        assert_eq!(state.tail, 3.into(), "Tail should advance to 3");
        assert_eq!(state.head, 3.into(), "Head should advance to 3");
    }

    #[test]
    fn test_out_of_order_within_buffer() {
        let mut monitor = DeltaDeltaMonitor::new(TEST_CAP);
        let factory = PacketFactory::new();

        let pkt1 = factory.at(1, 0);
        let pkt3 = factory.at(3, 0);
        let pkt2 = factory.at(2, 0);

        monitor.update(&pkt1);
        // FIX: Tail becomes 2 after processing packet 1.
        assert_eq!(monitor.state.unwrap().tail, 2.into());
        assert_eq!(monitor.state.unwrap().head, 2.into());

        monitor.update(&pkt3); // Packet 3 arrives
        // FIX: Tail is now waiting for packet 2, so it stays at 2.
        assert_eq!(
            monitor.state.unwrap().tail,
            2.into(),
            "Tail shouldn't move, waiting for 2"
        );
        assert_eq!(
            monitor.state.unwrap().head,
            4.into(),
            "Head should jump to 4"
        );
        assert!(
            monitor.packet(3.into()).is_some(),
            "Packet 3 should be in buffer"
        );

        monitor.update(&pkt2); // Packet 2 arrives, filling the gap
        // Now it can process 2, then 3.
        assert_eq!(
            monitor.state.unwrap().tail,
            4.into(),
            "Tail should advance to 4 after processing 2, 3"
        );
        assert_eq!(
            monitor.state.unwrap().head,
            4.into(),
            "Head should remain 4"
        );
    }

    #[test]
    fn test_packet_loss() {
        let mut monitor = DeltaDeltaMonitor::new(TEST_CAP);
        let factory = PacketFactory::new();

        monitor.update(&factory.at(1, 0));
        // FIX: Tail is 2 after processing packet 1. It is now waiting for packet 2.
        assert_eq!(
            monitor.state.unwrap().tail,
            2.into(),
            "Tail is 2, waiting for packet 2"
        );
        monitor.update(&factory.at(3, 0));
        assert_eq!(
            monitor.state.unwrap().tail,
            2.into(),
            "Tail should still be 2, waiting for 2"
        );

        monitor.update(&factory.at(12, 0)); // This packet is outside the window [2, 2+10)

        // The head becomes 13. new_tail = 13 - 10 = 3.
        // process_until(3) is called. It processes from old_tail (2) up to 3 (exclusive).
        // It looks for packet 2, doesn't find it (loss), and sets tail to 3.
        assert_eq!(
            monitor.state.unwrap().tail,
            3.into(),
            "Tail should slide past the lost packet"
        );
        assert_eq!(monitor.state.unwrap().head, 13.into());

        // Since tail is 3 and pkt 3 is in the buffer, it gets processed immediately.
        assert_eq!(
            monitor.state.unwrap().tail,
            4.into(),
            "Tail should process packet 3 which was waiting"
        );
    }

    #[test]
    fn test_buffer_wraparound_and_making_space() {
        let mut monitor = DeltaDeltaMonitor::new(TEST_CAP); // Capacity is 10
        let factory = PacketFactory::new();

        monitor.update(&factory.at(1, 0));
        // FIX: tail is 2 after processing the first packet. The valid window is [2, 12).
        assert_eq!(monitor.state.unwrap().tail, 2.into());
        assert_eq!(monitor.state.unwrap().head, 2.into());

        let pkt12 = factory.at(12, 0);
        monitor.update(&pkt12);

        let state = monitor.state.unwrap();
        // Head becomes 13. New window is [3, 13).
        assert_eq!(state.head, 13.into());
        // `process_until` moves tail from 2 to 3, skipping the lost packet 2.
        assert_eq!(
            state.tail,
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
        let mut monitor = DeltaDeltaMonitor::new(TEST_CAP);
        let factory = PacketFactory::new();

        monitor.update(&factory.at(5, 0));
        monitor.update(&factory.at(6, 0));

        // FIX: Pkt 5 is processed, tail becomes 6. Pkt 6 is processed, tail becomes 7.
        assert_eq!(monitor.state.unwrap().tail, 7.into());
        let state_before = monitor.state.unwrap();

        // Send a packet older than the current tail
        monitor.update(&factory.at(4, 0));

        // State should not have changed
        let state_after = monitor.state.unwrap();
        assert_eq!(
            state_after.tail,
            7.into(),
            "Old packet should not change tail"
        );
        assert_eq!(
            state_after.last_arrival, state_before.last_arrival,
            "State should not be updated by an old packet"
        );
    }

    #[test]
    fn test_duplicate_packet_overwrite() {
        let mut monitor = DeltaDeltaMonitor::new(TEST_CAP);
        let factory = PacketFactory::new();

        // Send packets 1 and 3, creating a gap for packet 2.
        monitor.update(&factory.at(1, 0));
        monitor.update(&factory.at(3, 0)); // Arrives 39ms after start

        // FIX: Tail is waiting at 2. Packet 3 is in the buffer.
        assert_eq!(monitor.state.unwrap().tail, 2.into());
        assert!(monitor.packet(3.into()).is_some());

        // Send a "duplicate" of 3, but with a different arrival time.
        // This simulates a network duplicate arriving later.
        let pkt3_dup = factory.at(3, 10); // Arrives 49ms after start
        monitor.update(&pkt3_dup);
        assert_eq!(
            monitor.packet(3.into()).as_ref().unwrap().arrival,
            pkt3_dup.arrival,
            "Duplicate should overwrite existing packet data"
        );

        // Now, fill the gap with packet 2. This will trigger processing.
        monitor.update(&factory.at(2, 0));

        // After processing, the last arrival time should be from packet 3 (the duplicate).
        let state = monitor.state.unwrap();
        assert_eq!(
            state.tail,
            4.into(),
            "Tail should have processed up to packet 3"
        );
        assert_eq!(
            state.last_arrival, pkt3_dup.arrival,
            "The monitor should have used the overwritten packet's data"
        );
    }

    #[test]
    fn test_sequence_number_wrap_around() {
        let mut monitor = DeltaDeltaMonitor::new(TEST_CAP);
        // FIX: Use a factory that won't overflow u64 math.
        let factory = PacketFactory::new();
        let mut current_seq = u64::MAX - 5;

        // Initialize the monitor
        monitor.update(&factory.at(current_seq, 0));
        assert_eq!(
            monitor.state.unwrap().tail,
            (current_seq.wrapping_add(1)).into()
        );

        // Process several packets, wrapping around u64::MAX
        for _ in 0..15 {
            current_seq = current_seq.wrapping_add(1);
            monitor.update(&factory.at(current_seq, 0));
        }

        let expected_tail = (u64::MAX - 5).wrapping_add(16);
        let expected_head = (u64::MAX - 5).wrapping_add(16);
        assert_eq!(monitor.state.unwrap().tail, expected_tail.into());
        assert_eq!(monitor.state.unwrap().head, expected_head.into());
    }
}
