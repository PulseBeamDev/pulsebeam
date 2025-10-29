//! # Real-time Stream Quality Monitor
//!
//! ## Architecture
//! This module implements a monitor that analyzes a single RTP stream to derive actionable
//! quality metrics. The design pattern separates the system into two distinct components:
//!
//! 1.  `StreamMonitor`: A single-owner, stateful struct responsible for all calculations.
//!     Its internal state is non-atomic for performance, as it is not intended to be shared.
//!     It consumes packet metadata and synthesizes it into high-level metrics.
//!
//! 2.  `StreamState`: A lightweight, thread-safe struct holding the final, shared conclusions
//!     (e.g., quality and inactive status). It uses atomic types to allow for lock-free reads
//!     from multiple downstream consumers, providing a simple and performant API for decision-making.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use std::time::Duration;
use str0m::rtp::SeqNo;
use tokio::time::Instant;

use crate::rtp::PacketTiming;

/// Defines the wall-clock duration without packets after which a stream is considered inactive.
const INACTIVE_TIMEOUT: Duration = Duration::from_secs(2);
/// The size of the circular buffer used for the sliding window packet loss calculation.
const LOSS_WINDOW_SIZE: usize = 256;

// --- Public-Facing Shared State ---

/// Represents the measured quality of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamQuality {
    /// The stream is experiencing significant packet loss or jitter.
    Bad = 0,
    /// The stream is performing adequately.
    Good = 1,
    /// The stream has very low packet loss and jitter.
    Excellent = 2,
}

/// A lightweight, shared view of a stream layer's status.
/// Designed for safe, multi-threaded access by downstream packet forwarders.
#[derive(Debug, Clone)]
pub struct StreamState {
    /// If true, this stream is considered inactive, either manually or due to timeout.
    /// This state is separate from stream quality.
    inactive: Arc<AtomicBool>,
    /// Estimated bitrate of the stream in bits per second.
    bitrate_bps: Arc<AtomicU64>,
    /// The current assessed quality of the stream.
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

    /// Returns `true` if the stream is manually paused or has timed out due to inactivity.
    pub fn is_inactive(&self) -> bool {
        self.inactive.load(Ordering::Relaxed)
    }

    /// Returns the estimated bitrate in bits per second.
    pub fn bitrate_bps(&self) -> u64 {
        self.bitrate_bps.load(Ordering::Relaxed)
    }

    /// Returns the current quality of the stream.
    pub fn quality(&self) -> StreamQuality {
        match self.quality.load(Ordering::Relaxed) {
            0 => StreamQuality::Bad,
            2 => StreamQuality::Excellent,
            _ => StreamQuality::Good, // Default case
        }
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
#[derive(Debug, Clone, Copy)]
pub struct HealthThresholds {
    pub become_bad_loss_percent: f32,
    pub become_bad_jitter_ms: u32,
    pub become_good_loss_percent: f32,
    pub become_good_jitter_ms: u32,
    pub become_excellent_loss_percent: f32,
    pub become_excellent_jitter_ms: u32,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self {
            become_bad_loss_percent: 5.0,
            become_bad_jitter_ms: 30,
            become_good_loss_percent: 2.0,
            become_good_jitter_ms: 20,
            become_excellent_loss_percent: 1.0,
            become_excellent_jitter_ms: 15,
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

    // Derived metrics, calculated periodically in `poll`.
    raw_loss_percent: f32,
    raw_jitter_ms: u32,
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
            raw_loss_percent: 0.0,
            raw_jitter_ms: 0,
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

        // This is the core of the fix. If we transition from an active state to an
        // inactive one (due to packet timeout), we MUST reset all our metrics. This
        // prevents us from holding onto a stale "Excellent" quality for a stream
        // that has been throttled by the publisher and is no longer sending.
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

        // Update stream quality based on metrics
        let new_quality = self.determine_stream_quality();
        if new_quality != self.current_quality {
            self.current_quality = new_quality;
            self.shared_state
                .quality
                .store(new_quality as u8, Ordering::Relaxed);
        }
    }

    /// Resets the monitor to a clean state. Called when a stream becomes inactive.
    fn reset(&mut self) {
        // tracing::info!("Resetting stream monitor state due to inactivity");
        self.loss_window.reset();
        self.jitter_estimate = 0.0;
        self.jitter_last_arrival = None;
        self.jitter_last_rtp_time = None;
        self.raw_loss_percent = 0.0;
        self.raw_jitter_ms = 0;
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

    /// Assesses stream quality based on current loss and jitter metrics.
    fn determine_stream_quality(&self) -> StreamQuality {
        if !self.loss_window.is_ready() {
            return StreamQuality::Good; // Not enough data, assume Good.
        }

        match self.current_quality {
            StreamQuality::Excellent | StreamQuality::Good => {
                if self.raw_loss_percent > self.thresholds.become_bad_loss_percent
                    || self.raw_jitter_ms > self.thresholds.become_bad_jitter_ms
                {
                    StreamQuality::Bad
                } else if self.raw_loss_percent < self.thresholds.become_excellent_loss_percent
                    && self.raw_jitter_ms < self.thresholds.become_excellent_jitter_ms
                {
                    StreamQuality::Excellent
                } else {
                    self.current_quality // Remain in current state
                }
            }
            StreamQuality::Bad => {
                if self.raw_loss_percent < self.thresholds.become_good_loss_percent
                    && self.raw_jitter_ms < self.thresholds.become_good_jitter_ms
                {
                    StreamQuality::Good
                } else {
                    StreamQuality::Bad // Remain Bad
                }
            }
        }
    }

    // --- Internal Calculation Methods ---

    /// Updates derived metrics like bitrate and packet loss based on accumulated data.
    fn update_derived_metrics(&mut self, now: Instant) {
        // Update bitrate
        let elapsed = now.saturating_duration_since(self.bwe_last_update);
        if elapsed >= Duration::from_millis(500) {
            let elapsed_secs = elapsed.as_secs_f64();
            if elapsed_secs > 0.0 {
                let bps = (self.bwe_interval_bytes as f64 * 8.0) / elapsed_secs;
                self.shared_state
                    .bitrate_bps
                    .store(bps as u64, Ordering::Relaxed);
            }
            self.bwe_interval_bytes = 0;
            self.bwe_last_update = now;
        }

        // Update packet loss percentage
        self.raw_loss_percent = self.loss_window.calculate_loss_percent();
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
            become_good_loss_percent: 4.0,
            become_good_jitter_ms: 20,
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
        let actual_bps = state.bitrate_bps() as f64;
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
}
