use pulsebeam_runtime::sync::Arc;
use pulsebeam_runtime::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::collections::VecDeque;
use std::ops::Deref;
use std::time::Duration;
use str0m::rtp::SeqNo;
use str0m::{bwe::Bitrate, media::MediaKind};
use tokio::time::Instant;

use crate::bitrate::{BitrateController, BitrateControllerConfig};
use crate::rtp::RtpPacket;

const SIMULCAST_LAYER_PAUSE_TIMEOUT: Duration = Duration::from_millis(250);
const STREAM_DEAD_TIMEOUT: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum StreamQuality {
    Bad = 0,
    Good = 1,
    Excellent = 2,
}

#[derive(Debug, Clone)]
pub struct StreamState(Arc<StreamStateInner>);

impl StreamState {
    pub fn new(inactive: bool, bitrate_bps: u64) -> Self {
        Self(Arc::new(StreamStateInner::new(inactive, bitrate_bps)))
    }

    #[cfg(test)]
    pub fn update_for_test(&self) -> StreamStateUpdater<'_> {
        StreamStateUpdater { state: &self.0 }
    }
}

impl Deref for StreamState {
    type Target = StreamStateInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<StreamStateInner> for StreamState {
    fn as_ref(&self) -> &StreamStateInner {
        &self.0
    }
}

#[derive(Debug)]
pub struct StreamStateInner {
    inactive: AtomicBool,
    bitrate_bps: AtomicU64,
    quality: AtomicU8,

    audio_envelope_bits: AtomicU32,
    silence_duration_ms: AtomicU64,
    normalized_volume_bits: AtomicU32,
}

impl StreamStateInner {
    pub fn new(inactive: bool, bitrate_bps: u64) -> Self {
        Self {
            inactive: AtomicBool::new(inactive),
            bitrate_bps: AtomicU64::new(bitrate_bps),
            quality: AtomicU8::new(StreamQuality::Good as u8),
            audio_envelope_bits: AtomicU32::new(0.0f32.to_bits()),
            silence_duration_ms: AtomicU64::new(0),
            normalized_volume_bits: AtomicU32::new(0.0f32.to_bits()),
        }
    }

    pub fn is_healthy(&self) -> bool {
        !self.is_inactive() && self.quality() != StreamQuality::Bad
    }

    pub fn is_inactive(&self) -> bool {
        self.inactive.load(Ordering::Relaxed)
    }

    pub fn bitrate_bps(&self) -> f64 {
        self.bitrate_bps.load(Ordering::Relaxed) as f64
    }

    pub fn audio_envelope(&self) -> f32 {
        f32::from_bits(self.audio_envelope_bits.load(Ordering::Relaxed))
    }

    pub fn silence_duration(&self) -> Duration {
        Duration::from_millis(self.silence_duration_ms.load(Ordering::Relaxed))
    }

    pub fn normalized_volume(&self) -> f32 {
        f32::from_bits(self.normalized_volume_bits.load(Ordering::Relaxed))
    }

    pub fn quality(&self) -> StreamQuality {
        match self.quality.load(Ordering::Relaxed) {
            0 => StreamQuality::Bad,
            2 => StreamQuality::Excellent,
            _ => StreamQuality::Good,
        }
    }
}

#[cfg(test)]
pub struct StreamStateUpdater<'a> {
    state: &'a StreamStateInner,
}

#[cfg(test)]
impl<'a> StreamStateUpdater<'a> {
    pub fn bitrate(self, bps: u64) -> Self {
        self.state.bitrate_bps.store(bps, Ordering::Relaxed);
        self
    }
    pub fn quality(self, q: StreamQuality) -> Self {
        self.state.quality.store(q as u8, Ordering::Relaxed);
        self
    }
    pub fn inactive(self, val: bool) -> Self {
        self.state.inactive.store(val, Ordering::Relaxed);
        self
    }
}

#[derive(Debug)]
pub struct StreamMonitor {
    shared_state: StreamState,

    stream_id: String,
    kind: MediaKind, // distinguish Audio/Video for scoring

    highest_seq: Option<SeqNo>,
    last_highest_seq: SeqNo,
    packets_received_this_tick: u64,
    smoothed_loss_ratio: f64,
    last_packet_at: Instant,
    bwe: BitrateEstimate,
    audio_monitor: Option<AudioMonitor>,

    current_quality: StreamQuality,
}

impl StreamMonitor {
    pub fn new(kind: MediaKind, stream_id: String, shared_state: StreamState) -> Self {
        let now = Instant::now();
        let audio_monitor = match kind {
            MediaKind::Audio => Some(AudioMonitor::new()),
            MediaKind::Video => None,
        };
        Self {
            stream_id,
            kind,
            shared_state,
            last_packet_at: now,
            highest_seq: None,
            last_highest_seq: 0.into(),
            packets_received_this_tick: 0,
            smoothed_loss_ratio: 0.0,
            audio_monitor,
            bwe: BitrateEstimate::new(now),
            current_quality: StreamQuality::Good,
        }
    }

    pub fn process_packet(&mut self, packet: &RtpPacket, size_bytes: usize) {
        self.last_packet_at = packet.arrival_ts;
        self.packets_received_this_tick += 1;
        self.bwe.record(size_bytes);

        let seq = packet.seq_no;
        match self.highest_seq {
            Some(current_highest) if seq <= current_highest => {}
            _ => {
                if self.highest_seq.is_none() {
                    self.last_highest_seq = seq;
                }
                self.highest_seq = Some(seq);
            }
        }

        if let Some(audio_monitor) = self.audio_monitor.as_mut() {
            let ext = &packet.ext_vals;
            audio_monitor.process_packet(
                packet.arrival_ts,
                ext.voice_activity.unwrap_or_default(),
                ext.audio_level.unwrap_or_default(),
            );
        }
    }

    pub fn shared_state(&self) -> &StreamState {
        &self.shared_state
    }

    pub fn poll(&mut self, now: Instant, is_any_sibling_active: bool) {
        self.bwe.poll(now);
        self.shared_state
            .bitrate_bps
            .store(self.bwe.estimate_bps() as u64, Ordering::Relaxed);

        if let Some(audio_monitor) = self.audio_monitor.as_mut() {
            audio_monitor.poll(now);
            let audio_metrics = audio_monitor.get_metrics(now);
            self.shared_state.audio_envelope_bits.store(
                audio_metrics.speech_intensity_envelope.to_bits(),
                Ordering::Relaxed,
            );
            self.shared_state
                .normalized_volume_bits
                .store(audio_metrics.normalized_volume.to_bits(), Ordering::Relaxed);
            self.shared_state.silence_duration_ms.store(
                audio_metrics.silence_duration.as_millis() as u64,
                Ordering::Relaxed,
            );
        }

        // Step A: Dual-Timer Inactivity (Simulcast Fast-Pause)
        let time_since_last_packet = now.saturating_duration_since(self.last_packet_at);
        let was_inactive = self.shared_state.is_inactive();

        if time_since_last_packet > SIMULCAST_LAYER_PAUSE_TIMEOUT && is_any_sibling_active {
            self.shared_state.inactive.store(true, Ordering::Relaxed);
            if self.current_quality != StreamQuality::Bad {
                self.current_quality = StreamQuality::Bad;
                self.shared_state
                    .quality
                    .store(StreamQuality::Bad as u8, Ordering::Relaxed);
            }
            self.shared_state.bitrate_bps.store(0, Ordering::Relaxed);
            return;
        }

        if time_since_last_packet > STREAM_DEAD_TIMEOUT {
            self.shared_state.inactive.store(true, Ordering::Relaxed);
            if !was_inactive {
                self.reset(now);
            }
            return;
        }

        self.shared_state.inactive.store(false, Ordering::Relaxed);

        // Step B: Calculate Interval Packet Loss
        let expected = self
            .highest_seq
            .map(|highest| (*highest).saturating_sub(*self.last_highest_seq))
            .unwrap_or(0);
        if let Some(highest) = self.highest_seq {
            self.last_highest_seq = highest;
        }

        let actual = self.packets_received_this_tick;
        self.packets_received_this_tick = 0;

        let interval_loss = if expected > 0 {
            expected.saturating_sub(actual) as f64 / expected as f64
        } else {
            0.0
        };

        // Step C: Asymmetric EWMA (Fast-Drop, Slow-Recover)
        let alpha = if interval_loss > self.smoothed_loss_ratio {
            0.50
        } else {
            0.05
        };
        self.smoothed_loss_ratio =
            (self.smoothed_loss_ratio * (1.0 - alpha)) + (interval_loss * alpha);

        // Step D: Threshold-Based Quality Evaluation with hysteresis.
        //
        // Downgrade thresholds are the same regardless of current state (react fast).
        // Upgrade thresholds are tighter than the downgrade thresholds to prevent flapping:
        let new_quality = match (self.kind, self.current_quality) {
            (MediaKind::Video, StreamQuality::Bad) => {
                if self.smoothed_loss_ratio <= 0.025 {
                    StreamQuality::Good
                } else {
                    StreamQuality::Bad
                }
            }
            (MediaKind::Video, StreamQuality::Good) => {
                if self.smoothed_loss_ratio >= 0.05 {
                    StreamQuality::Bad
                } else if self.smoothed_loss_ratio <= 0.005 {
                    StreamQuality::Excellent
                } else {
                    StreamQuality::Good
                }
            }
            (MediaKind::Video, StreamQuality::Excellent) => {
                if self.smoothed_loss_ratio >= 0.05 {
                    StreamQuality::Bad
                } else if self.smoothed_loss_ratio >= 0.02 {
                    StreamQuality::Good
                } else {
                    StreamQuality::Excellent
                }
            }
            (MediaKind::Audio, StreamQuality::Bad) => {
                if self.smoothed_loss_ratio <= 0.06 {
                    StreamQuality::Good
                } else {
                    StreamQuality::Bad
                }
            }
            (MediaKind::Audio, StreamQuality::Good) => {
                if self.smoothed_loss_ratio >= 0.10 {
                    StreamQuality::Bad
                } else if self.smoothed_loss_ratio <= 0.02 {
                    StreamQuality::Excellent
                } else {
                    StreamQuality::Good
                }
            }
            (MediaKind::Audio, StreamQuality::Excellent) => {
                if self.smoothed_loss_ratio >= 0.10 {
                    StreamQuality::Bad
                } else if self.smoothed_loss_ratio >= 0.06 {
                    StreamQuality::Good
                } else {
                    StreamQuality::Excellent
                }
            }
        };

        if new_quality != self.current_quality {
            tracing::info!(
                stream_id = %self.stream_id,
                "Stream quality transition: {:?} -> {:?} (smoothed_loss_ratio: {:.2}%, interval_loss: {:.2}%, expected: {}, actual: {}, bitrate: {})",
                self.current_quality,
                new_quality,
                self.smoothed_loss_ratio * 100.0,
                interval_loss * 100.0,
                expected,
                actual,
                Bitrate::from(self.bwe.estimate_bps()),
            );
            self.current_quality = new_quality;
            self.shared_state
                .quality
                .store(new_quality as u8, Ordering::Relaxed);
        }
    }

    fn reset(&mut self, now: Instant) {
        tracing::info!(
            stream_id = %self.stream_id,
            "Stream inactive, resetting all metrics. Quality was: {:?}", self.current_quality);
        self.highest_seq = None;
        self.last_highest_seq = 0.into();
        self.packets_received_this_tick = 0;
        self.smoothed_loss_ratio = 0.0;
        self.bwe = BitrateEstimate::new(now);
        self.current_quality = StreamQuality::Good;
        self.shared_state
            .quality
            .store(StreamQuality::Good as u8, Ordering::Relaxed);

        self.shared_state.bitrate_bps.store(0, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct Snapshot {
    ts: Instant,
    bytes: usize,
}

#[derive(Debug)]
pub struct BitrateEstimate {
    controller: BitrateController,
    last_update: Instant,
    accumulated_bytes: usize,

    history: VecDeque<Snapshot>,
    window_sum: usize,
    max_window_duration: Duration,
}

impl BitrateEstimate {
    pub fn new(now: Instant) -> Self {
        let config = BitrateControllerConfig {
            min_bitrate: Bitrate::kbps(10),
            max_bitrate: Bitrate::mbps(5),
            default_bitrate: Bitrate::kbps(100),
            ..Default::default()
        };

        Self {
            controller: BitrateController::new(config),
            last_update: now,
            accumulated_bytes: 0,
            history: VecDeque::new(),
            window_sum: 0,
            max_window_duration: Duration::from_secs(5),
        }
    }

    pub fn record(&mut self, packet_len: usize) {
        self.accumulated_bytes += packet_len;
    }

    pub fn poll(&mut self, now: Instant) {
        // Headroom strategy: use a small factor to allow for some growth while
        // avoiding over-allocation that leads to bufferbloat on congested links.
        const HEADROOM: f64 = 1.05;
        let elapsed = now.saturating_duration_since(self.last_update);
        if elapsed < Duration::from_millis(200) {
            return;
        }

        let snapshot = Snapshot {
            ts: now,
            bytes: self.accumulated_bytes,
        };
        self.window_sum += snapshot.bytes;
        self.history.push_back(snapshot);

        while let Some(front) = self.history.front() {
            if now.saturating_duration_since(front.ts) > self.max_window_duration {
                let removed = self.history.pop_front().unwrap();
                self.window_sum = self.window_sum.saturating_sub(removed.bytes);
            } else {
                break;
            }
        }

        let actual_window_duration = if let Some(front) = self.history.front() {
            now.saturating_duration_since(front.ts)
        } else {
            Duration::ZERO
        };

        // Safety: ensure we don't divide by zero at the very start
        let valid_duration = actual_window_duration.max(elapsed).as_secs_f64();

        // sliding window average
        let bps = if valid_duration > 0.001 {
            (self.window_sum as f64 * 8.0) / valid_duration * HEADROOM
        } else {
            0.0
        };

        self.controller.update(Bitrate::from(bps));
        self.last_update = now;
        self.accumulated_bytes = 0;
    }

    pub fn estimate_bps(&self) -> f64 {
        self.controller.current().as_f64()
    }
}

/// Tuning constants for the "Leaky Integrator"
const AUDIO_ATTACK_RATE: f32 = 0.2; // How fast we react to new speech (0.0-1.0)
const AUDIO_DECAY_RATE: f32 = 0.05; // How fast we fade out (keeps user in Top-N during pauses)

// Anything quieter than -50dB is considered background noise and clipped to 0.0.
const NOISE_THRESHOLD_DB: i8 = -50;
// The theoretical floor for silence in this integer scale (-127dB).
const SILENCE_DB_FLOOR: f32 = -127.0;

#[derive(Debug, Clone, Copy)]
pub struct AudioDerivedMetrics {
    /// A stable score (0.0-1.0) representing "Dominance".
    /// High during speech, decays slowly during pauses.
    /// USE THIS for sorting Top-N.
    pub speech_intensity_envelope: f32,

    /// The instantaneous volume (0.0-1.0), normalized and noise-gated.
    /// USE THIS for visualizers (green borders/audio bars).
    pub normalized_volume: f32,

    /// Time elapsed since the last "active" voice frame was detected.
    /// USE THIS for tie-breaking active speakers.
    pub silence_duration: Duration,
}

#[derive(Debug)]
pub struct AudioMonitor {
    // Internal State
    envelope: f32,
    last_packet_at: Instant,
    last_speech_at: Instant,
}

impl Default for AudioMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioMonitor {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            envelope: 0.0,
            last_packet_at: now,
            last_speech_at: now, // Initialize to now so we don't start with infinite silence
        }
    }

    /// Process audio level.
    ///
    /// * `vad_bit`: True if the encoder detects voice.
    /// * `level`: i8 dBov. 0 is Max, -30 is normal, -127 is silence.
    pub fn process_packet(&mut self, now: Instant, vad_bit: bool, level: i8) {
        // 1. Calculate time delta for frame-independent decay
        // (Assuming roughly 20ms packets, but handling jitter/loss)
        let dt_secs = now
            .saturating_duration_since(self.last_packet_at)
            .as_secs_f32();
        self.last_packet_at = now;

        // 2. Normalize Level
        // Range: -127 (Silence) -> 0 (Max).
        // We clip anything below NOISE_THRESHOLD_DB (-50) to 0.0.
        let raw_vol = if level < NOISE_THRESHOLD_DB {
            0.0
        } else {
            // Normalize linear range [-127, 0] to [0.0, 1.0]
            // Example: -30dB -> (-30 - (-127)) / 127 = 97/127 = ~0.76
            (level as f32 - SILENCE_DB_FLOOR) / (0.0 - SILENCE_DB_FLOOR)
        };

        // 3. Update "Last Speech" Timer
        // We require BOTH the VAD bit AND significant volume.
        let is_speaking = vad_bit && raw_vol > 0.0;

        if is_speaking {
            self.last_speech_at = now;

            // ATTACK: Rapidly increase envelope based on volume intensity
            // We add to the envelope, but clamp at 1.0.
            self.envelope += raw_vol * AUDIO_ATTACK_RATE;
        } else {
            // DECAY: Exponential decay based on time delta.
            // Normalize decay to work regardless of packet rate (target ~50Hz).
            let decay_factor = 1.0 - (AUDIO_DECAY_RATE * (dt_secs / 0.02));
            self.envelope *= decay_factor.max(0.0);
        }

        // Clamp envelope to 0.0 - 1.0
        self.envelope = self.envelope.clamp(0.0, 1.0);
    }

    /// Poll function to force decay if no packets are arriving
    /// (e.g., if the user went on mute or network died).
    pub fn poll(&mut self, now: Instant) {
        let dt_secs = now
            .saturating_duration_since(self.last_packet_at)
            .as_secs_f32();

        // If we haven't seen a packet in > 200ms, force decay
        if dt_secs > 0.2 {
            let decay_factor = 1.0 - (AUDIO_DECAY_RATE * (dt_secs / 0.02));
            self.envelope *= decay_factor.max(0.0);
            self.envelope = self.envelope.clamp(0.0, 1.0);
            self.last_packet_at = now; // Reset tick
        }
    }

    pub fn get_metrics(&self, now: Instant) -> AudioDerivedMetrics {
        AudioDerivedMetrics {
            speech_intensity_envelope: self.envelope,
            // Derive a simple volume for UI from the current envelope or raw input
            normalized_volume: self.envelope,
            silence_duration: now.saturating_duration_since(self.last_speech_at),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use more_asserts::{assert_gt, assert_le};
    use std::time::Duration;
    use str0m::media::{Frequency, MediaTime};
    use tokio::time::Instant;

    fn packet(seq: u64, arrival_ts: Instant) -> RtpPacket {
        RtpPacket {
            seq_no: seq.into(),
            rtp_ts: MediaTime::new(seq * 3000, Frequency::NINETY_KHZ),
            arrival_ts,
            ..Default::default()
        }
    }

    #[test]
    fn stream_monitor_fast_pause_marks_inactive_and_bad() {
        let shared = StreamState::new(false, 123_000);
        let mut monitor = StreamMonitor::new(MediaKind::Video, "v0".into(), shared.clone());
        let now = Instant::now();

        monitor.process_packet(&packet(1, now), 1200);
        monitor.poll(now, false);

        let paused_now = now + Duration::from_millis(300);
        monitor.poll(paused_now, true);

        assert!(shared.is_inactive());
        assert_eq!(shared.quality(), StreamQuality::Bad);
        assert_eq!(shared.bitrate_bps(), 0.0);
    }

    #[test]
    fn stream_monitor_dead_timeout_resets_metrics() {
        let shared = StreamState::new(false, 123_000);
        let mut monitor = StreamMonitor::new(MediaKind::Video, "v1".into(), shared.clone());
        let now = Instant::now();

        monitor.process_packet(&packet(1, now), 1200);
        monitor.process_packet(&packet(11, now + Duration::from_millis(1)), 1200);
        monitor.poll(now + Duration::from_millis(50), false);
        assert_eq!(shared.quality(), StreamQuality::Bad);
        assert!(monitor.smoothed_loss_ratio > 0.0);

        monitor.poll(now + Duration::from_millis(1200), false);

        assert!(shared.is_inactive());
        assert_eq!(shared.quality(), StreamQuality::Good);
        assert_eq!(shared.bitrate_bps(), 0.0);
        assert_eq!(monitor.highest_seq, None);
        assert_eq!(monitor.last_highest_seq, 0.into());
        assert_eq!(monitor.packets_received_this_tick, 0);
        assert_eq!(monitor.smoothed_loss_ratio, 0.0);
        assert_eq!(monitor.current_quality, StreamQuality::Good);
    }

    #[test]
    fn stream_monitor_ewma_is_fast_drop_slow_recover() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(MediaKind::Video, "v2".into(), shared);
        let now = Instant::now();

        monitor.process_packet(&packet(1, now), 1000);
        monitor.process_packet(&packet(11, now + Duration::from_millis(1)), 1000);
        monitor.poll(now + Duration::from_millis(20), false);

        let after_drop = monitor.smoothed_loss_ratio;
        assert!((after_drop - 0.4).abs() < 1e-9);

        for seq in 12..=21 {
            monitor.process_packet(
                &packet(seq, now + Duration::from_millis(40 + (seq - 12))),
                1000,
            );
        }
        monitor.poll(now + Duration::from_millis(100), false);

        let after_recover_tick = monitor.smoothed_loss_ratio;
        assert!((after_recover_tick - 0.38).abs() < 1e-9);
    }

    #[test]
    fn stream_monitor_hysteresis_prevents_flop() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(MediaKind::Video, "v4".into(), shared.clone());
        let now = Instant::now();

        // Drive quality Bad: 9 gaps out of 10 expected (loss=0.8, smoothed=0.4 >= 0.05)
        monitor.process_packet(&packet(1, now), 800);
        monitor.process_packet(&packet(11, now + Duration::from_millis(1)), 800);
        monitor.poll(now + Duration::from_millis(20), false);
        assert_eq!(shared.quality(), StreamQuality::Bad);
        assert!(
            monitor.smoothed_loss_ratio > 0.025,
            "smoothed={} should still be above the Bad-exit threshold",
            monitor.smoothed_loss_ratio
        );

        // One fully clean tick: smoothed decays from 0.4 * 0.95 = 0.38 — still above 0.025
        for seq in 12u64..=22 {
            monitor.process_packet(
                &packet(seq, now + Duration::from_millis(30 + seq - 12)),
                800,
            );
        }
        monitor.poll(now + Duration::from_millis(50), false);
        assert_eq!(
            shared.quality(),
            StreamQuality::Bad,
            "a single clean tick must not flip quality back to Good (hysteresis)"
        );

        // Sustain clean traffic until smoothed_loss_ratio falls below 0.025
        let mut tick_now = now + Duration::from_millis(100);
        let mut base_seq = 23u64;
        for _ in 0..100 {
            for seq in base_seq..(base_seq + 10) {
                monitor.process_packet(
                    &packet(seq, tick_now + Duration::from_millis(seq - base_seq)),
                    800,
                );
            }
            base_seq += 10;
            tick_now += Duration::from_millis(100);
            monitor.poll(tick_now, false);
            if shared.quality() == StreamQuality::Good {
                break;
            }
        }
        assert_eq!(
            shared.quality(),
            StreamQuality::Good,
            "quality must eventually recover to Good after sustained clean network"
        );
    }

    #[test]
    fn stream_monitor_thresholds_match_media_kind() {
        let now = Instant::now();

        let audio_shared = StreamState::new(false, 0);
        let mut audio = StreamMonitor::new(MediaKind::Audio, "a0".into(), audio_shared.clone());
        audio.process_packet(&packet(1, now), 200);
        audio.poll(now + Duration::from_millis(5), false);
        assert_eq!(audio_shared.quality(), StreamQuality::Excellent);

        audio.process_packet(&packet(2, now + Duration::from_millis(20)), 200);
        audio.process_packet(&packet(6, now + Duration::from_millis(21)), 200);
        audio.poll(now + Duration::from_millis(40), false);
        assert_eq!(audio_shared.quality(), StreamQuality::Bad);

        let video_shared = StreamState::new(false, 0);
        let mut video = StreamMonitor::new(MediaKind::Video, "v3".into(), video_shared.clone());
        video.process_packet(&packet(1, now), 800);
        video.poll(now + Duration::from_millis(5), false);
        assert_eq!(video_shared.quality(), StreamQuality::Excellent);

        video.process_packet(&packet(2, now + Duration::from_millis(20)), 800);
        video.process_packet(&packet(4, now + Duration::from_millis(21)), 800);
        video.poll(now + Duration::from_millis(40), false);
        assert_eq!(video_shared.quality(), StreamQuality::Bad);
    }

    struct StreamSimulator {
        estimator: BitrateEstimate,
        now: Instant,
    }

    impl StreamSimulator {
        fn new() -> Self {
            Self {
                estimator: BitrateEstimate::new(Instant::now()),
                now: Instant::now(),
            }
        }

        fn run_steady(&mut self, duration: Duration, target_bps: f64) {
            let tick_size = Duration::from_millis(100);
            let bytes_per_tick = (target_bps * 0.1 / 8.0) as usize;
            let end = self.now + duration;
            while self.now < end {
                self.now += tick_size;
                self.estimator.record(bytes_per_tick);
                self.estimator.poll(self.now);
            }
        }

        fn inject_keyframe(&mut self, size_bytes: usize) {
            self.estimator.record(size_bytes);
            self.now += Duration::from_millis(100);
            self.estimator.poll(self.now);
        }

        fn current(&self) -> f64 {
            self.estimator.estimate_bps()
        }
    }

    #[test]
    fn test_keyframe_rejection() {
        let mut sim = StreamSimulator::new();

        // 1. Warm up at 1Mbps
        sim.run_steady(Duration::from_secs(5), 1_000_000.0);
        let baseline = sim.current();

        println!("Baseline: {:.0} bps", baseline);
        assert_gt!(baseline, 1_200_000.0);
        assert_le!(baseline, 1_460_000.0);

        // 2. Inject Massive Keyframe
        // Normal 100ms = 12,500 bytes. Keyframe = 62,500 bytes (5x spike).
        sim.inject_keyframe(62_500);

        // 3. Run steady again
        sim.run_steady(Duration::from_millis(500), 1_000_000.0);

        let after_spike = sim.current();
        println!("After Spike: {:.0} bps", after_spike);

        // STABILITY CHECK:
        // The BitrateController should have allowed up to 10% deviation.
        // It might have risen by 1 quantization step (10kbps), but not more.
        assert_le!(
            (after_spike - baseline).abs(),
            baseline * 0.10,
            "Estimator flapped due to keyframe!"
        );
    }
}
