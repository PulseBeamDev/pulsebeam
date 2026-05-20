use pulsebeam_runtime::sync::Arc;
use pulsebeam_runtime::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::collections::VecDeque;
use std::ops::Deref;
use std::time::Duration;
use str0m::{bwe::Bitrate, media::MediaKind};
use tokio::time::Instant;

use crate::rtp::RtpPacket;

const SIMULCAST_LAYER_PAUSE_TIMEOUT: Duration = Duration::from_millis(1000);
const STREAM_DEAD_TIMEOUT: Duration = Duration::from_millis(3000);
const LOSS_MEASUREMENT_WINDOW: Duration = Duration::from_millis(500);
const WARMUP_LOSS_PENALTY: f64 = 0.15;

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

    window_start_ts: Instant,
    window_start_seq: u64,
    window_highest_seq: Option<u64>,
    window_actual_packets: u64,
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
            window_start_ts: now,
            window_start_seq: 0,
            window_highest_seq: None,
            window_actual_packets: 0,
            smoothed_loss_ratio: 0.0,
            audio_monitor,
            bwe: BitrateEstimate::new(now),
            current_quality: StreamQuality::Good,
        }
    }

    pub fn process_packet(&mut self, packet: &RtpPacket) {
        self.last_packet_at = packet.arrival_ts;
        self.bwe.record(packet);

        let seq = *packet.seq_no;
        if self.window_highest_seq.is_none() {
            self.window_highest_seq = Some(seq);
            self.window_start_seq = seq;
            self.window_start_ts = packet.arrival_ts;
        } else if seq > self.window_highest_seq.unwrap_or(0) {
            self.window_highest_seq = Some(seq);
        }
        self.window_actual_packets += 1;

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
        let bitrate_estimate = self.bwe.estimate_bps() as u64;
        tracing::debug!(
            stream_id = self.stream_id,
            "upstream bwe={}",
            Bitrate::from(bitrate_estimate)
        );
        self.shared_state
            .bitrate_bps
            .store(bitrate_estimate, Ordering::Relaxed);

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

        // Step A: Inactivity & Flap Prevention
        let time_since_last_packet = now.saturating_duration_since(self.last_packet_at);
        let was_inactive = self.shared_state.is_inactive();

        if time_since_last_packet > SIMULCAST_LAYER_PAUSE_TIMEOUT && is_any_sibling_active {
            if !was_inactive {
                self.shared_state.inactive.store(true, Ordering::Relaxed);
                self.smoothed_loss_ratio = WARMUP_LOSS_PENALTY;
                tracing::warn!(
                    stream_id = %self.stream_id,
                    "Simulcast layer paused while siblings active; applying warmup loss penalty and forcing Bad quality"
                );
                self.current_quality = StreamQuality::Bad;
                self.shared_state
                    .quality
                    .store(StreamQuality::Bad as u8, Ordering::Relaxed);
                self.shared_state.bitrate_bps.store(0, Ordering::Relaxed);
            }
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

        // Resuming from any form of inactivity: reset the measurement window so that
        // stale seq numbers don't produce a phantom loss spike on the first window.
        // smoothed_loss_ratio is intentionally NOT reset — the 0.15 simulcast-resume
        // warmup penalty must survive the transition.
        if was_inactive {
            self.window_highest_seq = None;
            self.window_start_seq = 0;
            self.window_actual_packets = 0;
            self.window_start_ts = now;
        }

        // Step B: Windowed Packet Loss Calculation
        if now.saturating_duration_since(self.window_start_ts) >= LOSS_MEASUREMENT_WINDOW {
            let expected = self
                .window_highest_seq
                .unwrap_or(0)
                .saturating_sub(self.window_start_seq);
            let actual = self.window_actual_packets;

            if expected > 0 {
                let interval_loss = expected.saturating_sub(actual) as f64 / expected as f64;
                let alpha = if interval_loss > self.smoothed_loss_ratio {
                    0.50
                } else {
                    0.20
                };
                self.smoothed_loss_ratio =
                    (self.smoothed_loss_ratio * (1.0 - alpha)) + (interval_loss * alpha);

                self.evaluate_quality_hysteresis(interval_loss, expected, actual);
            }

            self.window_start_ts = now;
            self.window_actual_packets = 0;
            if let Some(highest) = self.window_highest_seq {
                self.window_start_seq = highest;
            }
        }
    }

    fn evaluate_quality_hysteresis(&mut self, interval_loss: f64, expected: u64, actual: u64) {
        // Step C: Evaluate Quality Hysteresis
        let evaluated_quality = match (self.kind, self.current_quality) {
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
                } else if self.smoothed_loss_ratio >= 0.015 {
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

        let new_quality = evaluated_quality;

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
        self.window_highest_seq = None;
        self.window_start_seq = 0;
        self.window_actual_packets = 0;
        self.window_start_ts = now;
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
pub struct BitrateEstimate {
    tick_start: Option<Instant>,
    accumulated_bytes: usize,
    raw_ticks: VecDeque<f64>,
    sma1_ticks: VecDeque<f64>,
    max_window_ticks: usize,
    baseline_bps: f64,
    fast_trend_bps: f64,
}

impl BitrateEstimate {
    const HEADROOM: f64 = 1.05;
    const TICK_MS: f64 = 500.0;

    pub fn new(_now: Instant) -> Self {
        Self {
            tick_start: None,
            accumulated_bytes: 0,
            raw_ticks: VecDeque::with_capacity(6),
            sma1_ticks: VecDeque::with_capacity(6),
            max_window_ticks: 6, // 3s per stage (6s total triangular spread)
            baseline_bps: 0.0,
            fast_trend_bps: 0.0,
        }
    }

    pub fn record(&mut self, pkt: &RtpPacket) {
        // Drive timing from receiver-scheduled playout_time to ignore network jitter
        self.advance_time(pkt.playout_time);
        self.accumulated_bytes += pkt.header_len + pkt.payload.len();
    }

    pub fn poll(&mut self, current_time: Instant) {
        self.advance_time(current_time);
    }

    fn advance_time(&mut self, time: Instant) {
        let current_tick = *self.tick_start.get_or_insert(time);

        if time < current_tick + Duration::from_millis(Self::TICK_MS as u64) {
            return;
        }

        let elapsed = time.saturating_duration_since(current_tick);
        let ticks_passed = (elapsed.as_millis() / Self::TICK_MS as u128) as usize;

        let instant_bps = (self.accumulated_bytes as f64 * 8.0 * 1000.0) / Self::TICK_MS;
        self.push_tick(instant_bps);

        // Submit pure silence for missed ticks
        let empty_ticks = ticks_passed.saturating_sub(1).min(1000);
        for _ in 0..empty_ticks {
            self.push_tick(0.0);
        }

        self.accumulated_bytes = 0;
        self.tick_start = Some(
            current_tick + Duration::from_millis((ticks_passed as u64) * Self::TICK_MS as u64),
        );
    }

    fn push_tick(&mut self, bps: f64) {
        if self.raw_ticks.len() == self.max_window_ticks {
            self.raw_ticks.pop_front();
        }
        self.raw_ticks.push_back(bps);

        // Stage 1 SMA
        let sma1 = self.raw_ticks.iter().sum::<f64>() / self.raw_ticks.len() as f64;

        // Stage 2 SMA (Cascaded to form a triangular window, eliminating cliff drops)
        if self.sma1_ticks.len() == self.max_window_ticks {
            self.sma1_ticks.pop_front();
        }
        self.sma1_ticks.push_back(sma1);

        self.baseline_bps = self.sma1_ticks.iter().sum::<f64>() / self.sma1_ticks.len() as f64;

        // Fast trend detection: median of last 3 ticks (1.5s)
        // Ignores 1-tick I-frame spikes but instantly reacts to sustained step-ups
        let recent_len = self.raw_ticks.len();
        if recent_len >= 3 {
            let a = self.raw_ticks[recent_len - 1];
            let b = self.raw_ticks[recent_len - 2];
            let c = self.raw_ticks[recent_len - 3];
            self.fast_trend_bps = a.max(b.min(c)).min(b.max(c));
        } else {
            self.fast_trend_bps = 0.0;
        }
    }

    pub fn estimate_bps(&self) -> f64 {
        self.baseline_bps.max(self.fast_trend_bps) * Self::HEADROOM
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
