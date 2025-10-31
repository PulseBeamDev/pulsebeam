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
const BITRATE_HISTORY_SAMPLES: usize = 16; // ~3-5 seconds of data

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

    // --- State Change Thresholds ---
    pub bad_score_threshold: f32,
    pub good_score_threshold: f32,
    pub excellent_score_threshold: f32,

    // --- Hard Limits & Catastrophic Penalties ---
    pub min_usable_bitrate_bps: u64,
    pub catastrophic_loss_percent: f32,
    pub catastrophic_jitter_ms: u32,
    pub catastrophic_penalty: f32,

    // --- Metric Weights ---
    pub loss_weight: f32,
    pub jitter_weight: f32,
    pub bitrate_cv_weight: f32,
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
            score_increment_scaler: 2.0, // A perfect interval adds 2.0 points to the score.
            score_decrement_multiplier: 4.0, // A very bad interval can subtract up to 4.0 points.

            // Score thresholds that determine the public StreamQuality
            bad_score_threshold: 40.0,
            good_score_threshold: 60.0,
            excellent_score_threshold: 90.0,

            // "The Cliff": Instantaneous triggers for severe penalties
            min_usable_bitrate_bps: 150_000,
            catastrophic_loss_percent: 20.0, // Over 20% loss is a disaster.
            catastrophic_jitter_ms: 80,      // Over 80ms jitter is a disaster.
            catastrophic_penalty: 30.0,      // Instantly subtract 30 points from the score.

            // How much each metric contributes to the overall health score for an interval.
            loss_weight: 0.5,
            jitter_weight: 0.3,
            bitrate_cv_weight: 0.2,
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
        self.filled
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
    raw_loss_percent: f32,
    raw_jitter_ms: u32,
    raw_bitrate_cv_percent: f32,
    current_quality: StreamQuality,
    quality_score: f32,
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
            raw_loss_percent: 0.0,
            raw_jitter_ms: 0,
            raw_bitrate_cv_percent: 0.0,
            current_quality: StreamQuality::Good,
            quality_score: config.good_score_threshold,
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
        let new_quality = self.determine_stream_quality();
        if new_quality != self.current_quality {
            tracing::debug!(
                "changed stream quality: {:?} -> {:?} (score: {:.1})",
                self.current_quality,
                new_quality,
                self.quality_score
            );
            self.current_quality = new_quality;
            self.shared_state
                .quality
                .store(new_quality as u8, Ordering::Relaxed);
        }
    }

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
        self.quality_score = self.config.good_score_threshold;
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

    fn determine_inactive_state(&self, now: Instant) -> bool {
        self.manual_pause || now.saturating_duration_since(self.last_packet_at) > INACTIVE_TIMEOUT
    }

    fn determine_stream_quality(&mut self) -> StreamQuality {
        if !self.loss_window.is_ready() || !self.bitrate_history.is_ready() {
            return StreamQuality::Good;
        }

        let normalize = |value: f32, thresholds: HealthThresholds| -> f32 {
            if value <= thresholds.good {
                1.0
            } else if value >= thresholds.bad {
                -((value - thresholds.bad) / (thresholds.bad - thresholds.good)).min(1.0)
            } else {
                1.0 - (value - thresholds.good) / (thresholds.bad - thresholds.good)
            }
        };

        let loss_health = normalize(self.raw_loss_percent, self.config.loss);
        let jitter_health = normalize(self.raw_jitter_ms as f32, self.config.jitter_ms);
        let cv_health = normalize(self.raw_bitrate_cv_percent, self.config.bitrate_cv_percent);

        let instantaneous_health = (loss_health * self.config.loss_weight)
            + (jitter_health * self.config.jitter_weight)
            + (cv_health * self.config.bitrate_cv_weight);

        let final_health =
            if self.shared_state.bitrate_bps_p50() < self.config.min_usable_bitrate_bps {
                -1.0
            } else {
                instantaneous_health
            };

        let mut catastrophic_penalty = 0.0;
        if self.raw_loss_percent > self.config.catastrophic_loss_percent {
            catastrophic_penalty += self.config.catastrophic_penalty;
        }
        if self.raw_jitter_ms > self.config.catastrophic_jitter_ms {
            catastrophic_penalty += self.config.catastrophic_penalty;
        }

        if final_health >= 0.0 {
            self.quality_score += self.config.score_increment_scaler * final_health;
        } else {
            self.quality_score -= final_health.abs() * self.config.score_decrement_multiplier;
        }
        self.quality_score -= catastrophic_penalty;

        self.quality_score = self
            .quality_score
            .clamp(self.config.score_min, self.config.score_max);

        if self.quality_score < self.config.bad_score_threshold {
            StreamQuality::Bad
        } else if self.quality_score >= self.config.excellent_score_threshold {
            StreamQuality::Excellent
        } else {
            StreamQuality::Good
        }
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
        self.raw_loss_percent = self.loss_window.calculate_loss_percent();
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
            let arrival_diff = arrival
                .saturating_duration_since(last_arrival)
                .as_secs_f64();
            let rtp_diff = rtp_time.wrapping_sub(last_rtp_time) as f64 / self.clock_rate as f64;
            let transit_diff = arrival_diff - rtp_diff;
            self.jitter_estimate += (transit_diff.abs() - self.jitter_estimate) / 16.0;
            self.raw_jitter_ms = (self.jitter_estimate * 1000.0) as u32;
        }
        self.jitter_last_arrival = Some(arrival);
        self.jitter_last_rtp_time = Some(rtp_time);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use str0m::media::MediaTime;

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
    fn quality_becomes_bad_after_sustained_packet_loss() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);

        // Fill window first
        for i in 0..(LOSS_WINDOW_SIZE as u64) {
            monitor.process_packet(
                &TestPacket {
                    seq: i.into(),
                    at: start,
                    ts: MediaTime::from_90khz(0),
                },
                1200,
            );
        }
        monitor.poll(start);
        assert_eq!(state.quality(), StreamQuality::Good);

        // Now, introduce sustained high loss
        let mut seq = LOSS_WINDOW_SIZE as u64;
        for i in 0..20 {
            // Poll for 10 seconds
            // 10% packet loss
            for _ in 0..90 {
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
            seq += 10; // Skip 10 packets

            let poll_time = start + Duration::from_millis(500 * (i + 1));
            monitor.poll(poll_time);
        }

        assert_eq!(
            state.quality(),
            StreamQuality::Bad,
            "Quality should be Bad after sustained high packet loss"
        );
    }

    #[test]
    fn quality_recovers_after_sustained_good_period() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);

        // Force quality to Bad first
        monitor.quality_score = 0.0;
        monitor.current_quality = StreamQuality::Bad;
        state
            .quality
            .store(StreamQuality::Bad as u8, Ordering::Relaxed);

        // Now simulate a perfect stream to recover
        let mut seq = 0;
        for i in 0..30 {
            // Poll for 15 seconds
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
            if state.quality() == StreamQuality::Good {
                break;
            }
        }

        assert_eq!(
            state.quality(),
            StreamQuality::Good,
            "Quality should recover to Good after a sustained period of no loss"
        );
    }

    #[test]
    fn quality_reaches_excellent_after_prolonged_perfect_conditions() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);

        // Start at Good
        monitor.quality_score = monitor.config.good_score_threshold;
        monitor.poll(start);
        assert_eq!(state.quality(), StreamQuality::Good);

        // Simulate a perfect stream
        let mut seq = 0;
        for i in 0..60 {
            // Poll for 30 seconds
            for _ in 0..100 {
                // Simulate ~1.92 Mbps bitrate
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
        }

        assert_eq!(
            state.quality(),
            StreamQuality::Excellent,
            "Quality should become Excellent after a long period of perfect conditions"
        );
    }

    #[test]
    fn catastrophic_loss_triggers_immediate_penalty() {
        let (mut monitor, state) = setup();
        let start = Instant::now();
        monitor.set_manual_pause(false);
        monitor.quality_score = 80.0; // Start with a high score
        monitor.poll(start);

        // Fill window with good packets
        for i in 0..(LOSS_WINDOW_SIZE as u64) {
            monitor.process_packet(
                &TestPacket {
                    seq: i.into(),
                    at: start,
                    ts: MediaTime::from_90khz(0),
                },
                1200,
            );
        }

        // Drop 30% of packets in one go
        let mut seq = LOSS_WINDOW_SIZE as u64;
        for _ in 0..((LOSS_WINDOW_SIZE as f32 * 0.7) as u64) {
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
        seq += (LOSS_WINDOW_SIZE as f32 * 0.3) as u64;

        monitor.poll(start + Duration::from_millis(500));

        assert!(
            monitor.quality_score < 50.0,
            "Score should drop significantly due to catastrophic penalty"
        );
        assert_eq!(
            state.quality(),
            StreamQuality::Bad,
            "Quality should drop to Bad immediately after catastrophic loss"
        );
    }
}
