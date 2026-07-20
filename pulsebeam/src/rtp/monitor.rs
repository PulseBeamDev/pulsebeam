use pulsebeam_runtime::sync::Arc;
use pulsebeam_runtime::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::collections::VecDeque;
use std::ops::Deref;
use std::time::Duration;
use str0m::bwe::Bitrate;
use tokio::time::Instant;

use crate::entity::TrackKind;
use crate::rtp::RtpPacket;

const SIMULCAST_LAYER_PAUSE_TIMEOUT: Duration = Duration::from_millis(1000);
const STREAM_DEAD_TIMEOUT: Duration = Duration::from_millis(3000);
const LOSS_MEASUREMENT_WINDOW: Duration = Duration::from_millis(500);
// An eligibility signal, not a per-packet alarm: small 500ms windows on a
// lossy WAN regularly contain one late/missing packet, and treating those
// as health transitions causes false layer churn and PLI storms.
const VIDEO_BAD_LOSS_THRESHOLD: f64 = 0.12;
const VIDEO_SEVERE_LOSS_THRESHOLD: f64 = 0.30;
const VIDEO_EXCELLENT_TO_GOOD_THRESHOLD: f64 = 0.05;
const VIDEO_BAD_TO_GOOD_THRESHOLD: f64 = 0.02;
// Durations, not packet-count thresholds, so a 5fps screen share and a
// 60fps camera both need persistent evidence, not one unlucky interval.
const VIDEO_DEGRADE_CONFIRMATION: Duration = Duration::from_secs(2);
const VIDEO_BAD_CONFIRMATION: Duration = Duration::from_secs(3);
const VIDEO_SEVERE_CONFIRMATION: Duration = Duration::from_secs(1);
const VIDEO_RECOVERY_CONFIRMATION: Duration = Duration::from_secs(3);
// Time-based so a single lost low-fps frame can't combine with another
// loss many seconds later, without imposing a packet-rate cutoff.
const VIDEO_EVIDENCE_MAX_GAP: Duration = Duration::from_secs(2);
// There's no jitter buffer: a packet that lands one window late is
// indistinguishable from a genuinely lost one, so `interval_loss` is exact
// only in how many packets it counts, not in what happened to them. With
// few expected packets that exactness doesn't help — a screen share at
// 2-5 fps can see just 1-2 packets per 500 ms window, so a single
// late/lost one swings interval_loss by 50-100%. Keep extending the
// window until it has gathered enough samples to be meaningful, capped so
// a persistently very-low-rate stream still gets evaluated eventually.
const MIN_LOSS_EVIDENCE_PACKETS: u64 = 5;
const MAX_LOSS_MEASUREMENT_WINDOW: Duration = Duration::from_secs(5);

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
    // Kept separate from the rolling ingress measurement: an inactive
    // encoding has no current rate, but reactivating it still costs its
    // nominal rate plus a keyframe.
    nominal_bitrate_bps: u64,
    bitrate_bps: AtomicU64,
    // Fast-reacting counterpart to `bitrate_bps` — see
    // `BitrateEstimate::demand_bps`. Used only to signal bandwidth demand
    // (BWE probing), never for admission/cost accounting.
    demand_bitrate_bps: AtomicU64,
    quality: AtomicU8,

    audio_envelope_bits: AtomicU32,
    silence_duration_ms: AtomicU64,
    normalized_volume_bits: AtomicU32,
}

impl StreamStateInner {
    pub fn new(inactive: bool, bitrate_bps: u64) -> Self {
        Self {
            inactive: AtomicBool::new(inactive),
            nominal_bitrate_bps: bitrate_bps,
            bitrate_bps: AtomicU64::new(bitrate_bps),
            demand_bitrate_bps: AtomicU64::new(bitrate_bps),
            quality: AtomicU8::new(StreamQuality::Good as u8),
            audio_envelope_bits: AtomicU32::new(0.0f32.to_bits()),
            silence_duration_ms: AtomicU64::new(0),
            normalized_volume_bits: AtomicU32::new(0.0f32.to_bits()),
        }
    }

    pub fn is_healthy(&self) -> bool {
        !self.is_inactive() && self.quality() != StreamQuality::Bad
    }

    /// Whether an encoding may be targeted by a keyframe-gated switch.
    ///
    /// Inactive isn't evidence of loss — publishers commonly pause an
    /// encoding until requested, so a known-good paused encoding stays
    /// eligible for one-PLI reactivation. Only sustained real loss (`Bad`)
    /// makes it ineligible.
    pub fn is_activation_candidate(&self) -> bool {
        self.quality() != StreamQuality::Bad
    }

    pub fn is_inactive(&self) -> bool {
        self.inactive.load(Ordering::Relaxed)
    }

    pub fn bitrate_bps(&self) -> f64 {
        self.bitrate_bps.load(Ordering::Relaxed) as f64
    }

    /// Fast-reacting bandwidth *demand* signal — for asking str0m to probe
    /// for more headroom, never for admission/cost accounting (that's
    /// `bitrate_bps`, deliberately more conservative). See
    /// `BitrateEstimate::demand_bps`.
    pub fn demand_bitrate_bps(&self) -> f64 {
        self.demand_bitrate_bps.load(Ordering::Relaxed) as f64
    }

    /// Configured bitrate envelope for this encoding, independent of whether
    /// it is currently producing packets.
    pub fn nominal_bitrate_bps(&self) -> f64 {
        self.nominal_bitrate_bps as f64
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
    pub fn demand_bitrate(self, bps: u64) -> Self {
        self.state.demand_bitrate_bps.store(bps, Ordering::Relaxed);
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
    nominal_bitrate_bps: u64,

    stream_id: String,
    kind: TrackKind, // distinguish Audio/Video for scoring

    window_start_ts: Instant,
    window_start_seq: u64,
    window_highest_seq: Option<u64>,
    window_actual_packets: u64,
    smoothed_loss_ratio: f64,
    last_packet_at: Instant,
    bwe: BitrateEstimate,
    audio_monitor: Option<AudioMonitor>,

    current_quality: StreamQuality,
    quality_transition_since: Option<Instant>,
    quality_transition_target: Option<StreamQuality>,
    quality_transition_last_evidence: Option<Instant>,
}

impl StreamMonitor {
    pub fn new(kind: TrackKind, stream_id: String, shared_state: StreamState) -> Self {
        let now = Instant::now();
        let nominal_bitrate_bps = shared_state.bitrate_bps() as u64;
        let audio_monitor = match kind {
            TrackKind::Audio => Some(AudioMonitor::new()),
            TrackKind::Video | TrackKind::Data => None,
        };
        // Audio has no simulcast layer to select between, so there's nothing
        // for a loss-driven quality signal to act on yet; stubbed Excellent
        // rather than run the (currently video-only) hysteresis machinery
        // against it. See `poll`.
        let current_quality = match kind {
            TrackKind::Audio => StreamQuality::Excellent,
            TrackKind::Video | TrackKind::Data => StreamQuality::Good,
        };
        shared_state
            .quality
            .store(current_quality as u8, Ordering::Relaxed);
        Self {
            stream_id,
            kind,
            shared_state,
            nominal_bitrate_bps,
            last_packet_at: now,
            window_start_ts: now,
            window_start_seq: 0,
            window_highest_seq: None,
            window_actual_packets: 0,
            smoothed_loss_ratio: 0.0,
            audio_monitor,
            bwe: BitrateEstimate::new(now),
            current_quality,
            quality_transition_since: None,
            quality_transition_target: None,
            quality_transition_last_evidence: None,
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
        // The ingress estimator can dip well below a video layer's nominal
        // rate between keyframes, but the next keyframe still costs the
        // full rate — never let a dip underprice it for allocation.
        let allocation_bitrate = if self.kind == TrackKind::Video {
            bitrate_estimate.max(self.nominal_bitrate_bps)
        } else if self.bwe.is_warm() {
            bitrate_estimate
        } else {
            bitrate_estimate.max(self.nominal_bitrate_bps)
        };
        // Same floor as `allocation_bitrate`, but from the fast/reactive
        // trend — this is what lets a live burst raise demand signaling
        // even while the conservative admission value hasn't confirmed it.
        let demand_estimate = self.bwe.demand_bps() as u64;
        let demand_bitrate = demand_estimate.max(self.nominal_bitrate_bps);
        tracing::debug!(
            stream_id = self.stream_id,
            "upstream bwe={}",
            Bitrate::from(allocation_bitrate)
        );
        self.shared_state
            .bitrate_bps
            .store(allocation_bitrate, Ordering::Relaxed);
        self.shared_state
            .demand_bitrate_bps
            .store(demand_bitrate, Ordering::Relaxed);

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
                tracing::debug!(
                    stream_id = %self.stream_id,
                    "Simulcast layer paused while siblings active; retaining its last loss classification for keyframe-gated reactivation"
                );
                self.quality_transition_since = None;
                self.quality_transition_target = None;
                self.quality_transition_last_evidence = None;
                self.shared_state.bitrate_bps.store(0, Ordering::Relaxed);
                self.shared_state
                    .demand_bitrate_bps
                    .store(0, Ordering::Relaxed);
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
        if was_inactive {
            self.window_highest_seq = None;
            self.window_start_seq = 0;
            self.window_actual_packets = 0;
            self.window_start_ts = now;
        }

        // Step B: Windowed Packet Loss Calculation. Audio has no simulcast
        // layer for a loss signal to act on yet, so it's stubbed Excellent
        // at construction and never evaluated here — see `new`.
        if self.kind != TrackKind::Video {
            return;
        }

        let window_elapsed = now.saturating_duration_since(self.window_start_ts);
        let expected = self
            .window_highest_seq
            .unwrap_or(0)
            .saturating_sub(self.window_start_seq);
        // Keep extending the window past LOSS_MEASUREMENT_WINDOW until
        // enough packets have been seen to make interval_loss meaningful,
        // capped by MAX_LOSS_MEASUREMENT_WINDOW. See that constant's doc
        // comment.
        let window_ready = window_elapsed >= LOSS_MEASUREMENT_WINDOW
            && (expected >= MIN_LOSS_EVIDENCE_PACKETS
                || window_elapsed >= MAX_LOSS_MEASUREMENT_WINDOW);

        if window_ready {
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

                self.evaluate_quality_hysteresis(now, interval_loss, expected, actual);
            }

            self.window_start_ts = now;
            self.window_actual_packets = 0;
            if let Some(highest) = self.window_highest_seq {
                self.window_start_seq = highest;
            }
        }
    }

    fn evaluate_quality_hysteresis(
        &mut self,
        now: Instant,
        interval_loss: f64,
        expected: u64,
        actual: u64,
    ) {
        // Only Bad makes a layer ineligible; don't let one 500ms loss
        // window withdraw it. Severe loss still acts immediately. Video
        // only — audio is stubbed Excellent in `new` and never reaches
        // here (see `poll`'s Step B kind gate).
        debug_assert_eq!(self.kind, TrackKind::Video);
        let new_quality = match self.current_quality {
            StreamQuality::Bad => {
                if self.smoothed_loss_ratio <= VIDEO_BAD_TO_GOOD_THRESHOLD {
                    StreamQuality::Good
                } else {
                    StreamQuality::Bad
                }
            }
            StreamQuality::Good => {
                if self.smoothed_loss_ratio >= VIDEO_BAD_LOSS_THRESHOLD
                    || interval_loss >= VIDEO_SEVERE_LOSS_THRESHOLD
                {
                    StreamQuality::Bad
                } else if self.smoothed_loss_ratio <= 0.005 {
                    StreamQuality::Excellent
                } else {
                    StreamQuality::Good
                }
            }
            StreamQuality::Excellent => {
                if self.smoothed_loss_ratio >= VIDEO_BAD_LOSS_THRESHOLD
                    || interval_loss >= VIDEO_SEVERE_LOSS_THRESHOLD
                {
                    StreamQuality::Bad
                } else if self.smoothed_loss_ratio >= VIDEO_EXCELLENT_TO_GOOD_THRESHOLD {
                    StreamQuality::Good
                } else {
                    StreamQuality::Excellent
                }
            }
        };

        if new_quality != self.current_quality {
            let confirmation = if new_quality > self.current_quality {
                VIDEO_RECOVERY_CONFIRMATION
            } else if interval_loss >= VIDEO_SEVERE_LOSS_THRESHOLD {
                VIDEO_SEVERE_CONFIRMATION
            } else if new_quality == StreamQuality::Bad {
                VIDEO_BAD_CONFIRMATION
            } else {
                VIDEO_DEGRADE_CONFIRMATION
            };
            // Evidence only accumulates while it supports the *same* target.
            // Otherwise alternating loss windows could accidentally combine
            // into a transition even though neither condition persisted.
            if self.quality_transition_target != Some(new_quality)
                || self
                    .quality_transition_last_evidence
                    .is_none_or(|last| now.saturating_duration_since(last) > VIDEO_EVIDENCE_MAX_GAP)
            {
                self.quality_transition_target = Some(new_quality);
                self.quality_transition_since = Some(now);
            }
            self.quality_transition_last_evidence = Some(now);
            let since = self
                .quality_transition_since
                .expect("transition start set above");
            if now.saturating_duration_since(since) < confirmation {
                return;
            }
        } else {
            self.quality_transition_since = None;
            self.quality_transition_target = None;
            self.quality_transition_last_evidence = None;
        }

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
            self.quality_transition_since = None;
            self.quality_transition_target = None;
            self.quality_transition_last_evidence = None;
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
        self.quality_transition_since = None;
        self.quality_transition_target = None;
        self.quality_transition_last_evidence = None;
        self.bwe = BitrateEstimate::new(now);
        // Audio stays stubbed Excellent even across a dead-stream reset —
        // see `new`.
        self.current_quality = match self.kind {
            TrackKind::Audio => StreamQuality::Excellent,
            TrackKind::Video | TrackKind::Data => StreamQuality::Good,
        };
        self.shared_state
            .quality
            .store(self.current_quality as u8, Ordering::Relaxed);

        self.shared_state.bitrate_bps.store(0, Ordering::Relaxed);
        self.shared_state
            .demand_bitrate_bps
            .store(0, Ordering::Relaxed);
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
    // Fast, reactive trend (median of last 3 raw ticks). Deliberately
    // trigger-happy — this is the *demand* signal (see `demand_bps`), used
    // to ask for more bandwidth, where a false-positive is cheap. It must
    // never feed admission math directly; see `admission_trend_bps`.
    fast_trend_bps: f64,
    // Slower, majority-confirmed trend over its own window (see
    // `admission_ticks`), decoupled from `raw_ticks`/`sma1_ticks` so it
    // doesn't disturb `baseline_bps`. This is the *admission* signal (see
    // `estimate_bps`) — a multi-second VBR/screen-share burst must not look
    // like a sustained cost increase until a real majority of the window
    // agrees.
    admission_ticks: VecDeque<f64>,
    admission_trend_bps: f64,
}

impl BitrateEstimate {
    const HEADROOM: f64 = 1.05;
    const TICK_MS: f64 = 500.0;
    // 3.5s: long enough that a multi-second burst (a few seconds of
    // screen-share/VBR activity) can't masquerade as a sustained cost
    // change, short enough to still confirm a genuine step-up well before
    // the 6s `baseline_bps` catches up.
    const ADMISSION_TREND_WINDOW_TICKS: usize = 7;

    pub fn new(_now: Instant) -> Self {
        Self {
            tick_start: None,
            accumulated_bytes: 0,
            raw_ticks: VecDeque::with_capacity(6),
            sma1_ticks: VecDeque::with_capacity(6),
            max_window_ticks: 6, // 3s per stage (6s total triangular spread)
            baseline_bps: 0.0,
            fast_trend_bps: 0.0,
            admission_ticks: VecDeque::with_capacity(Self::ADMISSION_TREND_WINDOW_TICKS),
            admission_trend_bps: 0.0,
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

        // Admission trend: median over a wider window, requiring a real
        // majority of the last 3.5s to agree before a rise counts.
        if self.admission_ticks.len() == Self::ADMISSION_TREND_WINDOW_TICKS {
            self.admission_ticks.pop_front();
        }
        self.admission_ticks.push_back(bps);
        let mut sorted: Vec<f64> = self.admission_ticks.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        self.admission_trend_bps = sorted[sorted.len() / 2];
    }

    /// Conservative, admission-facing estimate (backs `bitrate_bps()`): a
    /// burst must persist long enough to win a majority of
    /// `admission_trend_bps`'s window before it's trusted as real cost.
    pub fn estimate_bps(&self) -> f64 {
        self.baseline_bps.max(self.admission_trend_bps) * Self::HEADROOM
    }

    /// Fast, reactive estimate for signaling bandwidth *demand* (e.g.
    /// probing str0m for more headroom) — never for admission math. Safe to
    /// react quickly here: asking for more bandwidth is cheap, unlike
    /// charging another slot's shared pool on a false positive.
    pub fn demand_bps(&self) -> f64 {
        self.baseline_bps.max(self.fast_trend_bps) * Self::HEADROOM
    }

    fn is_warm(&self) -> bool {
        self.raw_ticks.len() == self.max_window_ticks
            && self.sma1_ticks.len() == self.max_window_ticks
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
    use more_asserts::{assert_ge, assert_le};
    use pulsebeam_testdata;
    use std::time::Duration;
    use str0m::media::{Frequency, MediaTime};
    use tokio::time::Instant;

    fn packet(seq: u64, arrival_ts: Instant) -> RtpPacket {
        RtpPacket {
            seq_no: seq.into(),
            rtp_ts: MediaTime::new(seq * 3000, Frequency::NINETY_KHZ),
            arrival_ts,
            playout_time: arrival_ts,
            ..Default::default()
        }
    }

    #[test]
    fn video_upstream_bitrate_never_falls_below_nominal_layer_rate() {
        let shared = StreamState::new(false, 1_250_000);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "high".into(), shared.clone());
        let now = Instant::now();

        // A partial initial window has too little traffic to characterize a
        // high simulcast layer. Its nominal rate remains the allocation cost.
        monitor.process_packet(&packet(1, now));
        monitor.poll(now + Duration::from_millis(600), false);
        assert_eq!(shared.bitrate_bps(), 1_250_000.0);
        assert!(!monitor.bwe.is_warm());

        // The same must remain true after the rolling estimator is warm but
        // happens to observe a low-VBR interval.
        monitor.bwe.raw_ticks.clear();
        monitor.bwe.sma1_ticks.clear();
        for _ in 0..monitor.bwe.max_window_ticks {
            monitor.bwe.raw_ticks.push_back(100_000.0);
            monitor.bwe.sma1_ticks.push_back(100_000.0);
        }
        monitor.bwe.baseline_bps = 100_000.0;
        monitor.bwe.fast_trend_bps = 100_000.0;
        monitor.poll(now + Duration::from_millis(700), false);
        assert!(monitor.bwe.is_warm());
        assert_eq!(shared.bitrate_bps(), 1_250_000.0);
    }

    #[test]
    fn stream_monitor_fast_pause_preserves_keyframe_reactivation_eligibility() {
        let shared = StreamState::new(false, 123_000);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "v0".into(), shared.clone());
        let now = Instant::now();

        monitor.process_packet(&packet(1, now));
        monitor.poll(now, false);

        let paused_now = now + Duration::from_millis(1100);
        monitor.poll(paused_now, true);

        assert!(shared.is_inactive());
        assert_eq!(shared.quality(), StreamQuality::Good);
        assert!(!shared.is_healthy());
        assert!(shared.is_activation_candidate());
        assert_eq!(shared.bitrate_bps(), 0.0);
        assert_eq!(monitor.smoothed_loss_ratio, 0.0);
    }

    #[test]
    fn stream_monitor_dead_timeout_resets_metrics() {
        let shared = StreamState::new(false, 123_000);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "v1".into(), shared.clone());
        let now = Instant::now();

        for window in 0..3u64 {
            let t = now + Duration::from_millis(window * 600);
            monitor.process_packet(&packet(1 + window * 10, t));
            monitor.process_packet(&packet(11 + window * 10, t + Duration::from_millis(1)));
            monitor.poll(t + Duration::from_millis(600), false);
        }
        assert_eq!(shared.quality(), StreamQuality::Bad);
        assert!(monitor.smoothed_loss_ratio > 0.0);

        monitor.poll(now + Duration::from_millis(5000), false);

        assert!(shared.is_inactive());
        assert_eq!(shared.quality(), StreamQuality::Good);
        assert_eq!(shared.bitrate_bps(), 0.0);
        assert_eq!(monitor.window_highest_seq, None);
        assert_eq!(monitor.window_start_seq, 0);
        assert_eq!(monitor.window_actual_packets, 0);
        assert_eq!(monitor.smoothed_loss_ratio, 0.0);
        assert_eq!(monitor.current_quality, StreamQuality::Good);
    }

    #[test]
    fn stream_monitor_ewma_is_fast_drop_slow_recover() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "v2".into(), shared);
        let now = Instant::now();

        monitor.process_packet(&packet(1, now));
        monitor.process_packet(&packet(11, now + Duration::from_millis(1)));
        monitor.poll(now + Duration::from_millis(600), false);

        let after_drop = monitor.smoothed_loss_ratio;
        assert!((after_drop - 0.4).abs() < 1e-9);

        for seq in 12..=21 {
            monitor.process_packet(&packet(seq, now + Duration::from_millis(700 + (seq - 12))));
        }
        monitor.poll(now + Duration::from_millis(1200), false);

        let after_recover_tick = monitor.smoothed_loss_ratio;
        assert!((after_recover_tick - 0.32).abs() < 1e-9);
    }

    #[test]
    fn stream_monitor_hysteresis_prevents_flop() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "v4".into(), shared.clone());
        let now = Instant::now();

        // Drive quality Bad with persistent severe loss, not one report.
        for window in 0..3u64 {
            let t = now + Duration::from_millis(window * 600);
            monitor.process_packet(&packet(1 + window * 10, t));
            monitor.process_packet(&packet(11 + window * 10, t + Duration::from_millis(1)));
            monitor.poll(t + Duration::from_millis(600), false);
        }
        assert_eq!(shared.quality(), StreamQuality::Bad);
        assert!(
            monitor.smoothed_loss_ratio > 0.025,
            "smoothed={} should still be above the Bad-exit threshold",
            monitor.smoothed_loss_ratio
        );

        // One fully clean window: smoothed decays from 0.4 * 0.8 = 0.32 — still above 0.025
        for seq in 32u64..=42 {
            monitor.process_packet(&packet(seq, now + Duration::from_millis(1900 + seq - 32)));
        }
        monitor.poll(now + Duration::from_millis(2400), false);
        assert_eq!(
            shared.quality(),
            StreamQuality::Bad,
            "a single clean window must not flip quality back to Good (hysteresis)"
        );

        // Sustain clean traffic until smoothed_loss_ratio falls below 0.025
        let mut tick_now = now + Duration::from_millis(2900);
        let mut base_seq = 43u64;
        for _ in 0..100 {
            for seq in base_seq..(base_seq + 10) {
                monitor.process_packet(&packet(
                    seq,
                    tick_now + Duration::from_millis(seq - base_seq),
                ));
            }
            base_seq += 10;
            tick_now += Duration::from_millis(600);
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
    fn stream_monitor_ewma_decay_prevents_instant_upgrade() {
        // After a high-loss window pushes quality to Bad, the asymmetric EWMA
        // (alpha_down=0.2) decays slowly. A single clean window is NOT enough to
        // bring smoothed_loss_ratio below the Bad→Good threshold (2.5% for video).
        // The EWMA's natural time-to-decay is the "consecutive windows" guard.
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "v5".into(), shared.clone());
        let now = Instant::now();

        // Drive Bad with three severe windows. The time confirmation prevents
        // a lone report from changing upstream eligibility.
        for window in 0..3u64 {
            let t = now + Duration::from_millis(window * 600);
            monitor.process_packet(&packet(1 + window * 10, t));
            monitor.process_packet(&packet(11 + window * 10, t + Duration::from_millis(1)));
            monitor.poll(t + Duration::from_millis(600), false);
        }
        assert_eq!(shared.quality(), StreamQuality::Bad);
        assert!(
            monitor.smoothed_loss_ratio >= VIDEO_BAD_LOSS_THRESHOLD,
            "persistent severe loss must leave a substantial EWMA penalty"
        );

        // One clean window: EWMA = 0.40 * 0.80 = 0.32 — still above 2.5%.
        for seq in 32u64..=42 {
            monitor.process_packet(&packet(seq, now + Duration::from_millis(1900)));
        }
        monitor.poll(now + Duration::from_millis(2400), false);
        assert_eq!(
            shared.quality(),
            StreamQuality::Bad,
            "one clean window must not immediately restore Good; smoothed={:.3}",
            monitor.smoothed_loss_ratio
        );

        // Sustain clean traffic until EWMA decays below 2.5% and quality upgrades.
        let mut t = now + Duration::from_millis(2900);
        let mut seq = 43u64;
        let mut recovered = false;
        for _ in 0..60 {
            for i in 0..10u64 {
                monitor.process_packet(&packet(seq + i, t + Duration::from_millis(i * 5)));
            }
            seq += 10;
            t += Duration::from_millis(600);
            monitor.poll(t, false);
            if shared.quality() == StreamQuality::Good {
                recovered = true;
                break;
            }
        }
        assert!(recovered, "quality must eventually recover via EWMA decay");
    }

    #[test]
    fn stream_monitor_severe_downgrade_is_time_confirmed() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "v6".into(), shared.clone());
        let now = Instant::now();

        monitor.current_quality = StreamQuality::Excellent;
        monitor
            .shared_state
            .quality
            .store(StreamQuality::Excellent as u8, Ordering::Relaxed);

        for window in 0..3u64 {
            let t = now + Duration::from_millis(window * 600);
            monitor.process_packet(&packet(1 + window * 10, t));
            monitor.process_packet(&packet(11 + window * 10, t + Duration::from_millis(1)));
            monitor.poll(t + Duration::from_millis(600), false);
        }

        assert_eq!(shared.quality(), StreamQuality::Bad);
    }

    /// A 2 fps screen share sees only one expected packet per 500 ms
    /// window. Without a minimum-sample gate, a single lost frame is a
    /// 1-packet window reading 100% interval_loss — noise, not evidence.
    /// The window must instead keep extending past `LOSS_MEASUREMENT_WINDOW`
    /// until it has gathered `MIN_LOSS_EVIDENCE_PACKETS`, so the ratio
    /// reflects real history instead of a single coin flip.
    #[test]
    fn low_fps_window_defers_evaluation_until_minimum_sample_size() {
        let shared = StreamState::new(false, 0);
        let mut monitor =
            StreamMonitor::new(TrackKind::Video, "screenshare".into(), shared.clone());
        let now = Instant::now();

        // Seed steady-state window bookkeeping directly: window_start_seq
        // is a carried-over boundary from a prior window (as in
        // production, every window after the very first), not a
        // freshly-received packet — avoids the unrelated first-window
        // accounting edge case this test isn't about.
        monitor.window_highest_seq = Some(100);
        monitor.window_start_seq = 100;
        monitor.window_start_ts = now;
        monitor.window_actual_packets = 0;

        // Frame 101 is lost; frame 102 arrives. 600ms later (past
        // LOSS_MEASUREMENT_WINDOW) only 2 packets are expected — below
        // MIN_LOSS_EVIDENCE_PACKETS, so the window must not evaluate yet.
        monitor.process_packet(&packet(102, now + Duration::from_millis(500)));
        monitor.poll(now + Duration::from_millis(600), false);
        assert_eq!(
            monitor.smoothed_loss_ratio, 0.0,
            "a 2-packet window was trusted as loss evidence"
        );

        // Frames 103-107 arrive cleanly. expected is now 7 (>= 5): enough
        // samples to finally evaluate — one real loss among 7 is real
        // evidence (~14%), but must not be misread as severe (30%+).
        let mut t = now + Duration::from_millis(600);
        for seq in 103..=107u64 {
            monitor.process_packet(&packet(seq, t));
            t += Duration::from_millis(500);
        }
        monitor.poll(t + Duration::from_millis(100), false);

        assert!(
            monitor.smoothed_loss_ratio > 0.0,
            "loss was never evaluated even once enough samples accumulated"
        );
        assert!(
            monitor.smoothed_loss_ratio < VIDEO_SEVERE_LOSS_THRESHOLD,
            "one real loss among 7 samples misread as severe: {}",
            monitor.smoothed_loss_ratio
        );
    }

    #[test]
    fn sparse_video_does_not_combine_separated_loss_observations() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "sparse".into(), shared.clone());
        let now = Instant::now();
        monitor.current_quality = StreamQuality::Excellent;
        monitor
            .shared_state
            .quality
            .store(StreamQuality::Excellent as u8, Ordering::Relaxed);

        // The first severe observation starts, but cannot complete, a
        // degradation candidate.
        monitor.process_packet(&packet(1, now));
        monitor.process_packet(&packet(11, now + Duration::from_millis(1)));
        monitor.poll(now + Duration::from_millis(600), false);

        // At this sparse cadence the next observation arrives after the
        // allowed evidence gap. It must start a new candidate instead of
        // completing the old one.
        let later = now + Duration::from_secs(4);
        monitor.process_packet(&packet(21, later));
        monitor.process_packet(&packet(31, later + Duration::from_millis(1)));
        monitor.poll(later + Duration::from_millis(600), false);
        assert_eq!(shared.quality(), StreamQuality::Excellent);
    }

    #[test]
    fn stream_monitor_thresholds_match_media_kind() {
        let now = Instant::now();

        let audio_shared = StreamState::new(false, 0);
        let mut audio = StreamMonitor::new(TrackKind::Audio, "a0".into(), audio_shared.clone());
        audio.process_packet(&packet(1, now));
        audio.process_packet(&packet(2, now + Duration::from_millis(1)));
        audio.poll(now + Duration::from_millis(600), false);
        audio.process_packet(&packet(3, now + Duration::from_millis(700)));
        audio.process_packet(&packet(4, now + Duration::from_millis(701)));
        audio.poll(now + Duration::from_millis(1200), false);
        audio.process_packet(&packet(5, now + Duration::from_millis(1300)));
        audio.process_packet(&packet(6, now + Duration::from_millis(1301)));
        audio.poll(now + Duration::from_millis(1800), false);
        assert_eq!(audio_shared.quality(), StreamQuality::Excellent);

        audio.process_packet(&packet(7, now + Duration::from_millis(1900)));
        audio.process_packet(&packet(11, now + Duration::from_millis(1901)));
        audio.poll(now + Duration::from_millis(2400), false);
        // Audio has no simulcast layer for a loss-driven quality signal to
        // act on yet, so it's stubbed Excellent and never evaluated here —
        // this lossy window must not move it.
        assert_eq!(audio_shared.quality(), StreamQuality::Excellent);

        let video_shared = StreamState::new(false, 0);
        let mut video = StreamMonitor::new(TrackKind::Video, "v3".into(), video_shared.clone());
        video.process_packet(&packet(1, now));
        video.process_packet(&packet(2, now + Duration::from_millis(1)));
        video.poll(now + Duration::from_millis(600), false);
        video.process_packet(&packet(3, now + Duration::from_millis(700)));
        video.process_packet(&packet(4, now + Duration::from_millis(701)));
        video.poll(now + Duration::from_millis(1200), false);
        video.process_packet(&packet(5, now + Duration::from_millis(1300)));
        video.process_packet(&packet(6, now + Duration::from_millis(1301)));
        video.poll(now + Duration::from_millis(1800), false);
        video.process_packet(&packet(7, now + Duration::from_millis(1900)));
        video.process_packet(&packet(8, now + Duration::from_millis(1901)));
        video.poll(now + Duration::from_millis(2400), false);
        // Four two-packet windows are insufficient to establish video
        // quality; retain the conservative initial state.
        assert_eq!(video_shared.quality(), StreamQuality::Good);

        video.process_packet(&packet(9, now + Duration::from_millis(2500)));
        video.process_packet(&packet(11, now + Duration::from_millis(2501)));
        video.poll(now + Duration::from_millis(3000), false);
        // A tiny two-packet sample is not evidence of upstream congestion.
        assert_eq!(video_shared.quality(), StreamQuality::Good);
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
                self.record_packet(bytes_per_tick);
                self.estimator.poll(self.now);
            }
        }

        fn inject_keyframe(&mut self, size_bytes: usize) {
            self.now += Duration::from_millis(500);
            self.estimator.poll(self.now);
            self.record_packet(size_bytes);
            self.now += Duration::from_millis(500);
            self.estimator.poll(self.now);
        }

        fn record_packet(&mut self, size_bytes: usize) {
            let mut pkt = RtpPacket::default();
            pkt.arrival_ts = self.now;
            pkt.playout_time = self.now;
            let payload_len = size_bytes.saturating_sub(pkt.header_len);
            pkt.payload = std::sync::Arc::from(vec![0; payload_len].as_slice());
            self.estimator.record(&pkt);
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
        assert_ge!(baseline, 1_050_000.0);
        assert_le!(baseline, 1_200_000.0);

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

    // ── Real-network scenario tests ──────────────────────────────────────────

    /// Fiber / clean LAN: zero loss for 2.4 s → reaches Excellent and stays.
    #[test]
    fn fiber_clean_network_reaches_and_stays_excellent() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "fiber".into(), shared.clone());
        let now = Instant::now();

        // 10 packets per 500-ms window; seq advances by 10 each window.
        // VIDEO_GOOD_TO_EXCELLENT_UPGRADE_WINDOWS = 4 consecutive windows needed.
        let mut seq = 1u64;
        let mut t = now;
        for _ in 0..6 {
            for i in 0..10u64 {
                monitor.process_packet(&packet(seq + i, t + Duration::from_millis(i * 5)));
            }
            seq += 10;
            t += Duration::from_millis(600);
            monitor.poll(t, false);
        }

        assert_eq!(
            shared.quality(),
            StreamQuality::Excellent,
            "clean network must reach Excellent"
        );

        // Two more clean windows — quality must not drop.
        for _ in 0..2 {
            for i in 0..10u64 {
                monitor.process_packet(&packet(seq + i, t + Duration::from_millis(i * 5)));
            }
            seq += 10;
            t += Duration::from_millis(600);
            monitor.poll(t, false);
        }
        assert_eq!(
            shared.quality(),
            StreamQuality::Excellent,
            "must stay Excellent"
        );
    }

    /// Regional WAN: ~2% sustained loss → stays Good, never reaches Bad.
    #[test]
    fn wan_low_loss_stays_good() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "wan".into(), shared.clone());
        let now = Instant::now();

        // 50 packets per window; skip 1 seq in the middle → ~2% loss.
        // expected = 49, actual = 49 (we send 49 but the gap at seq+25 shows 1 lost).
        let mut base_seq = 1u64;
        let mut t = now;
        for _ in 0..20 {
            for i in 0..50u64 {
                if i == 25 {
                    continue; // simulate 1 lost packet mid-window
                }
                monitor.process_packet(&packet(base_seq + i, t + Duration::from_millis(i * 2)));
            }
            base_seq += 50;
            t += Duration::from_millis(600);
            monitor.poll(t, false);

            assert_ne!(
                shared.quality(),
                StreamQuality::Bad,
                "2% loss must never reach Bad (window ending at t={t:?})"
            );
        }

        // It may be classified Good rather than Excellent, but it must remain
        // eligible and never create allocator churn.
        assert_eq!(shared.quality(), StreamQuality::Good);
    }

    #[test]
    fn isolated_loss_windows_do_not_flap_video_quality() {
        let shared = StreamState::new(false, 0);
        let mut monitor =
            StreamMonitor::new(TrackKind::Video, "isolated-loss".into(), shared.clone());
        let now = Instant::now();
        let mut base_seq = 1u64;
        let mut t = now;

        // Every window contains one missing packet out of 30: this is the
        // shape seen on a jittery path. It may be conservatively Good, but it
        // must remain eligible and never flap into Bad.
        for _ in 0..8 {
            for i in 0..30u64 {
                if i != 15 {
                    monitor.process_packet(&packet(base_seq + i, t + Duration::from_millis(i * 2)));
                }
            }
            base_seq += 30;
            t += Duration::from_millis(600);
            monitor.poll(t, false);
        }

        assert_eq!(shared.quality(), StreamQuality::Good);
        assert!(monitor.shared_state.is_healthy());
    }

    #[test]
    fn video_ordinary_loss_does_not_make_a_layer_ineligible() {
        let shared = StreamState::new(false, 0);
        let mut monitor =
            StreamMonitor::new(TrackKind::Video, "confirm-bad".into(), shared.clone());
        let now = Instant::now();

        // Repeated ~11% intervals are degraded, but below the high-confidence
        // Bad threshold and must not remove a layer from allocation.
        for i in 0..10u64 {
            if i != 5 {
                monitor.process_packet(&packet(1 + i, now + Duration::from_millis(i * 10)));
            }
        }
        monitor.poll(now + Duration::from_millis(600), false);
        // The initial window establishes the sequence baseline, so there is
        // not yet a loss measurement to act on.
        assert_eq!(shared.quality(), StreamQuality::Good);

        // More ordinary-loss windows still keep the layer eligible.
        let next = now + Duration::from_millis(600);
        for i in 0..10u64 {
            if i != 5 {
                monitor.process_packet(&packet(11 + i, next + Duration::from_millis(i * 10)));
            }
        }
        monitor.poll(next + Duration::from_millis(600), false);
        assert_eq!(shared.quality(), StreamQuality::Good);

        let third = next + Duration::from_millis(600);
        for i in 0..10u64 {
            if i != 5 {
                monitor.process_packet(&packet(21 + i, third + Duration::from_millis(i * 10)));
            }
        }
        monitor.poll(third + Duration::from_millis(600), false);
        assert_eq!(shared.quality(), StreamQuality::Good);
    }

    /// Cross-region WAN: 20% sustained loss → Bad after confirmation, then
    /// recovers to Good after sustained clean traffic.
    #[test]
    fn cross_region_high_loss_detects_bad_then_recovers() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "xr".into(), shared.clone());
        let now = Instant::now();

        // 10 packets per window; drop two interior packets → ~20% loss.
        let mut base_seq = 1u64;
        let mut t = now;

        // The first window establishes the sequence baseline. Three measured
        // windows confirm the high-confidence ordinary-loss Bad transition.
        for _ in 0..8 {
            for i in 0..10u64 {
                if i == 3 || i == 5 {
                    continue;
                }
                monitor.process_packet(&packet(base_seq + i, t + Duration::from_millis(i * 10)));
            }
            base_seq += 10;
            t += Duration::from_millis(600);
            monitor.poll(t, false);
        }
        assert_eq!(
            shared.quality(),
            StreamQuality::Bad,
            "must detect Bad quickly"
        );

        // Recover: send clean windows until Good is restored.
        let mut recovered = false;
        for _ in 0..60 {
            for i in 0..10u64 {
                monitor.process_packet(&packet(base_seq + i, t + Duration::from_millis(i * 10)));
            }
            base_seq += 10;
            t += Duration::from_millis(600);
            monitor.poll(t, false);
            if shared.quality() == StreamQuality::Good {
                recovered = true;
                break;
            }
        }
        assert!(
            recovered,
            "quality must recover to Good after sustained clean traffic"
        );
    }

    /// screen-share idle: windows with expected==0 must NOT change quality.
    #[test]
    fn cbr_idle_window_does_not_change_quality() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "cbr".into(), shared.clone());
        let now = Instant::now();

        // Reach Excellent with 6 clean windows.
        let mut seq = 1u64;
        let mut t = now;
        for _ in 0..6 {
            for i in 0..10u64 {
                monitor.process_packet(&packet(seq + i, t + Duration::from_millis(i * 5)));
            }
            seq += 10;
            t += Duration::from_millis(600);
            monitor.poll(t, false);
        }
        assert_eq!(shared.quality(), StreamQuality::Excellent);

        // Idle: no packets for 2 consecutive 500-ms windows (simulate screen-share freeze).
        // Expected = 0 in both windows → quality must be preserved.
        for _ in 0..2 {
            t += Duration::from_millis(600);
            monitor.poll(t, false);
            assert_eq!(
                shared.quality(),
                StreamQuality::Excellent,
                "idle window must not degrade quality"
            );
        }

        // Resume with clean packets.
        for i in 0..10u64 {
            monitor.process_packet(&packet(seq + i, t + Duration::from_millis(i * 5)));
        }
        seq += 10;
        t += Duration::from_millis(600);
        monitor.poll(t, false);
        assert_eq!(
            shared.quality(),
            StreamQuality::Excellent,
            "quality must survive idle + resume"
        );
    }

    /// Simulcast resume: a large seq gap during a pause must NOT produce phantom
    /// loss. A pause is not loss evidence and must preserve reactivation
    /// eligibility for the layer.
    #[test]
    fn no_phantom_loss_on_simulcast_resume() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "sr".into(), shared.clone());
        let now = Instant::now();

        // Establish stream at seq 1–10.
        for seq in 1u64..=10 {
            monitor.process_packet(&packet(seq, now + Duration::from_millis(seq * 5)));
        }
        monitor.poll(now + Duration::from_millis(600), false);

        // Simulcast pause: no packet for > 1 s while a sibling is active.
        let paused_at = now + Duration::from_millis(1100);
        monitor.poll(paused_at, true);
        assert!(shared.is_inactive());
        let smoothed_after_pause = monitor.smoothed_loss_ratio;
        assert_eq!(smoothed_after_pause, 0.0, "pause must not manufacture loss");
        assert!(shared.is_activation_candidate());

        // Resume with a large seq gap (encoder advanced by 990 during the pause).
        let resumed_at = paused_at + Duration::from_millis(50);
        monitor.process_packet(&packet(1000, resumed_at));
        monitor.poll(resumed_at + Duration::from_millis(10), false);

        assert!(
            !shared.is_inactive(),
            "must be active after first packet arrives"
        );
        assert!(
            monitor.smoothed_loss_ratio <= 0.20,
            "seq gap during pause must not spike smoothed_loss_ratio; got {:.3}",
            monitor.smoothed_loss_ratio
        );
    }

    /// Anti-oscillation: alternating clean / lossy windows must keep quality
    /// stable — never flipping Good→Bad→Good on each pair of windows.
    #[test]
    fn no_oscillation_under_alternating_loss() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "osc".into(), shared.clone());
        let now = Instant::now();

        // First drive quality to Bad with persistent high loss.
        for window in 0..3u64 {
            let t = now + Duration::from_millis(window * 600);
            monitor.process_packet(&packet(1 + window * 10, t));
            monitor.process_packet(&packet(11 + window * 10, t + Duration::from_millis(1)));
            monitor.poll(t + Duration::from_millis(600), false);
        }
        assert_eq!(shared.quality(), StreamQuality::Bad);

        // Alternate: one clean window, one lossy window, 20 pairs.
        // Quality must stay Bad (smoothed stays well above 0.025).
        let mut base_seq = 31u64;
        let mut t = now + Duration::from_millis(1800);
        let mut quality_changes = 0u32;
        let mut prev_quality = shared.quality();

        for _ in 0..20 {
            // Clean window: 10 consecutive packets.
            for i in 0..10u64 {
                monitor.process_packet(&packet(base_seq + i, t + Duration::from_millis(i * 5)));
            }
            base_seq += 10;
            t += Duration::from_millis(600);
            monitor.poll(t, false);
            let q = shared.quality();
            if q != prev_quality {
                quality_changes += 1;
                prev_quality = q;
            }

            // Lossy window: only first and last packet (simulate ~80% loss).
            monitor.process_packet(&packet(base_seq, t + Duration::from_millis(1)));
            monitor.process_packet(&packet(base_seq + 9, t + Duration::from_millis(2)));
            base_seq += 10;
            t += Duration::from_millis(600);
            monitor.poll(t, false);
            let q = shared.quality();
            if q != prev_quality {
                quality_changes += 1;
                prev_quality = q;
            }
        }

        assert!(
            quality_changes <= 2,
            "quality must not oscillate; saw {quality_changes} changes over 40 windows"
        );
    }

    // ── Publisher bandwidth-limit scenarios ─────────────────────────────────

    /// Three simulcast layers; the high layer abruptly takes on 40% packet loss
    /// (publisher limited by bandwidth). The SFU needs to switch to the mid
    /// layer fast — but after time-confirmed severe loss rather than from one
    /// 500-ms receiver report.
    #[test]
    fn publisher_bw_limit_fast_detection() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "high".into(), shared.clone());
        let now = Instant::now();
        // is_any_sibling_active=true because mid and low layers are healthy.
        let siblings = true;

        // --- Healthy phase: 6 clean windows → Excellent ---
        let mut seq = 1u64;
        let mut t = now;
        for _ in 0..6 {
            for i in 0..10u64 {
                monitor.process_packet(&packet(seq + i, t + Duration::from_millis(i * 5)));
            }
            seq += 10;
            t += Duration::from_millis(600);
            monitor.poll(t, siblings);
        }
        assert_eq!(
            shared.quality(),
            StreamQuality::Excellent,
            "precondition: high layer healthy"
        );

        // --- Degraded phase: publisher limits bandwidth → 40% loss ---
        // Send seq+0,1,2, drop seq+3,4,5,6, send seq+7,8,9.
        // expected = (seq+9) − window_start(seq−1) = 10
        // actual   = 6  →  interval_loss = 4/10 = 40%
        for _ in 0..3 {
            for i in 0..10u64 {
                if i < 3 || i >= 7 {
                    monitor.process_packet(&packet(seq + i, t + Duration::from_millis(i * 5)));
                }
            }
            seq += 10;
            t += Duration::from_millis(600);
            monitor.poll(t, siblings);
        }

        assert_eq!(
            shared.quality(),
            StreamQuality::Bad,
            "must detect Bad after time-confirmed 40% loss; \
             smoothed={:.3}",
            monitor.smoothed_loss_ratio
        );
        let _ = seq; // seq used to silence warning
    }

    /// Publisher oscillating layer: the encoder keeps turning a simulcast layer
    /// on for a brief burst (~700 ms) then pausing it while siblings are active.
    ///
    /// A pause alone is not a loss signal. The layer may be selected only by the
    /// downstream controller's separately guarded probe and keyframe transition;
    /// the upstream monitor must retain its last real-loss classification.
    #[test]
    fn publisher_oscillating_layer_remains_a_keyframe_reactivation_candidate() {
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "osc".into(), shared.clone());
        let now = Instant::now();

        let mut seq = 1u64;
        let mut t = now;

        // --- Baseline: two clean windows confirm the monitor starts healthy ---
        for _ in 0..2 {
            for i in 0..10u64 {
                monitor.process_packet(&packet(seq + i, t + Duration::from_millis(i * 10)));
            }
            seq += 10;
            t += Duration::from_millis(600);
            monitor.poll(t, true);
        }
        assert_ne!(
            shared.quality(),
            StreamQuality::Bad,
            "precondition: layer must start healthy"
        );

        // --- 3 oscillation cycles: pause → brief clean burst → pause ---
        for cycle in 0..3 {
            // Pause: no packets for > 1 s while siblings are active.
            t += Duration::from_millis(1100);
            monitor.poll(t, true);

            assert!(
                shared.is_inactive(),
                "cycle {cycle}: must be dormant on pause"
            );
            assert!(
                shared.is_activation_candidate(),
                "cycle {cycle}: a pause alone must not make the layer Bad"
            );

            // Resume — step 1: one "wake" packet updates last_packet_at so the
            // next poll can exit the inactivity branch and reset the window.
            monitor.process_packet(&packet(seq, t + Duration::from_millis(10)));
            seq += 1;
            monitor.poll(t + Duration::from_millis(50), true); // was_inactive → reset window

            // Resume — step 2: 10 clean packets spanning > 500 ms from the
            // first one. The following poll fires the measurement window.
            for i in 0..10u64 {
                monitor.process_packet(&packet(seq + i, t + Duration::from_millis(100 + i * 10)));
            }
            seq += 10;
            t += Duration::from_millis(700); // ≥ 500 ms from first burst packet
            monitor.poll(t, true);

            assert!(
                shared.is_activation_candidate(),
                "cycle {cycle}: clean packets must retain reactivation eligibility"
            );
        }

        assert!(shared.is_activation_candidate());
    }

    // ── BitrateEstimate stability: real H.264 testdata ──────────────────────

    /// Feed a sequence of frame byte-sizes into a fresh `BitrateEstimate` at
    /// `fps` frames per second.  Each entry is sent as a single RTP packet so
    /// the estimator sees the correct byte count without fragmentation noise.
    ///
    /// Returns one `estimate_bps()` sample per 500 ms tick window, captured
    /// immediately after each tick fires.
    fn replay_frames_into_bwe(frame_sizes: &[usize], fps: u64) -> Vec<f64> {
        let frame_interval = Duration::from_micros(1_000_000 / fps);
        let tick_dur = Duration::from_millis(500);
        let t0 = Instant::now();
        let mut bwe = BitrateEstimate::new(t0);
        let mut now = t0;
        let mut next_sample = t0 + tick_dur;
        let mut samples: Vec<f64> = Vec::new();

        for &frame_bytes in frame_sizes {
            now += frame_interval;
            let mut pkt = RtpPacket::default();
            pkt.arrival_ts = now;
            pkt.playout_time = now;
            let payload_len = frame_bytes.saturating_sub(pkt.header_len);
            pkt.payload = std::sync::Arc::from(vec![0; payload_len].as_slice());

            bwe.record(&pkt);

            // Capture one sample per elapsed 500 ms window.
            while now >= next_sample {
                bwe.poll(next_sample);
                samples.push(bwe.estimate_bps());
                next_sample += tick_dur;
            }
        }
        samples
    }

    /// Verify that `BitrateEstimate` produces a stable, CBR-like output when
    /// fed the quarter H.264 stream (CBR target 150 kbps).
    ///
    /// Ground truth (from `make estimate INPUT_H264=quarter_q.h264`):
    ///   avg ≈ 149.76 kbps, min ≈ 97 kbps, max ≈ 225 kbps → raw ratio ≈ 2.33×
    ///
    /// After warmup the estimator must:
    /// 1. Track the right order of magnitude (within 0.8× … 3× of target).
    /// 2. Be MORE STABLE than the raw 1-second rolling window – i.e. the
    ///    max/min ratio of sampled estimates must beat the raw 2.33× ratio.
    #[test]
    fn bwe_quarter_h264_stable_cbr_estimate() {
        // Ground truth from `make estimate`: raw 1-second rolling-window ratio.
        const RAW_MAX_KBPS: f64 = 225.86;
        const RAW_MIN_KBPS: f64 = 97.01;
        const RAW_RATIO: f64 = RAW_MAX_KBPS / RAW_MIN_KBPS; // ≈ 2.33×

        const TARGET_BPS: f64 = 150_000.0;

        let frames = pulsebeam_testdata::h264_frame_sizes(pulsebeam_testdata::RAW_H264_QUARTER_CBR);
        let samples = replay_frames_into_bwe(&frames, 30);

        // 45 s @ 30 fps → ~90 ticks; skip first 10 (5 s warmup).
        assert!(
            samples.len() >= 20,
            "expected ≥ 20 samples, got {}",
            samples.len()
        );
        let steady: &[f64] = &samples[10..];

        let mean = steady.iter().sum::<f64>() / steady.len() as f64;
        let max = steady.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let min = steady.iter().copied().fold(f64::INFINITY, f64::min);

        // Rough range check: mean must be in [target×0.8, target×3].
        assert_ge!(
            mean,
            TARGET_BPS * 1.0,
            "mean estimate too low: {:.0} bps (target {:.0})",
            mean,
            TARGET_BPS
        );
        assert_le!(
            mean,
            TARGET_BPS * 1.15,
            "mean estimate too high: {:.0} bps (target {:.0})",
            mean,
            TARGET_BPS
        );

        // Stability: BitrateEstimate must be tighter than the raw per-second
        // rolling window.  The peak-hold + EWMA should absorb the 97–225 kbps
        // fluctuation into a near-flat line.
        let ratio = max / min.max(1.0);
        assert!(
            ratio < RAW_RATIO,
            "estimate less stable than raw 1 s window: \
             min={:.0} max={:.0} ratio={:.2}× > raw_ratio={:.2}×",
            min,
            max,
            ratio,
            RAW_RATIO
        );
    }

    // ── BitrateEstimate stability: synthetic VBR / screen-share patterns ────

    /// A VBR stream with a large I-frame every `gop` frames (e.g. every 30 frames
    /// = 1 s at 30 fps).  The estimator must absorb the I-frame spike and produce
    /// a stable flat line — consecutive 500 ms samples must not differ by more
    /// than 2× from one another.
    #[test]
    fn bwe_vbr_periodic_keyframes_no_oscillation() {
        // GOP = 30 frames (1 s at 30 fps); I-frame 20× larger than P-frame.
        const FPS: u64 = 30;
        const GOP: usize = 30;
        const P_FRAME_BYTES: usize = 3_000;
        const I_FRAME_BYTES: usize = P_FRAME_BYTES * 20; // 60 KB spike

        // 60 seconds of content.
        let nal_sizes: Vec<usize> = (0..FPS as usize * 60)
            .map(|i| {
                if i % GOP == 0 {
                    I_FRAME_BYTES
                } else {
                    P_FRAME_BYTES
                }
            })
            .collect();

        let samples = replay_frames_into_bwe(&nal_sizes, FPS);

        // Skip 5 s warmup.
        assert!(samples.len() >= 20, "too few samples: {}", samples.len());
        let steady = &samples[10..];

        // No two consecutive 500 ms windows may differ by more than 2×.
        // A naive instantaneous tracker would see alternating ~60 kbps (P-frames)
        // and ~480 kbps (I-frame tick) windows — a 8× swing.
        for pair in steady.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let ratio = if a > b {
                a / b.max(1.0)
            } else {
                b / a.max(1.0)
            };
            assert!(
                ratio < 2.0,
                "consecutive sample oscillation: {:.0} → {:.0} bps (ratio {:.2}×)",
                a,
                b,
                ratio
            );
        }
    }

    /// Screen-sharing pattern: burst of active content followed by a "still"
    /// idle phase at 2 fps (the minimum real-world screen-share send rate).
    /// The estimator must correctly track the lower 2 fps bitrate after the
    /// window slides past the active burst — neither stuck high nor dropping
    /// to zero.
    #[test]
    fn bwe_screen_share_idle_2fps_tracks_bitrate() {
        // Phase 1: 5 s active at ~500 kbps (30 fps, ~2 KB P-frames + 40 KB
        //   I-frame every 30 frames).
        const FPS: u64 = 30;
        const GOP: usize = 30;
        const P_BYTES: usize = 2_083; // ≈ 500 kbps / 30 fps / 8 bits
        const I_BYTES: usize = 40_000;
        // Phase 2: static screen at 2 fps — one ~2 KB P-frame every 500 ms.
        // instant_bps per tick = 2000 × 8 / 0.5 s = 32 000 bps.
        const IDLE_FRAME_BYTES: usize = 2_000;
        const IDLE_BPS: f64 = IDLE_FRAME_BYTES as f64 * 8.0 * 2.0; // 32 000 bps

        let active_frames = FPS as usize * 5;
        let active_nals: Vec<usize> = (0..active_frames)
            .map(|i| if i % GOP == 0 { I_BYTES } else { P_BYTES })
            .collect();

        let tick_dur = Duration::from_millis(500);
        let t0 = Instant::now();
        let mut bwe = BitrateEstimate::new(t0);
        let mut now = t0;

        // Warm the estimator with active frames.
        for &sz in &active_nals {
            now += Duration::from_micros(1_000_000 / FPS);
            let mut pkt = RtpPacket::default();
            pkt.arrival_ts = now;
            pkt.playout_time = now;
            let payload_len = sz.saturating_sub(pkt.header_len);
            pkt.payload = std::sync::Arc::from(vec![0; payload_len].as_slice());
            bwe.record(&pkt);
        }
        let post_active = bwe.estimate_bps();

        // Phase 2: 10 s idle at 2 fps — one IDLE_FRAME_BYTES packet per tick.
        let idle_ticks = 20usize; // 10 s
        let mut idle_samples = Vec::with_capacity(idle_ticks);
        for _ in 0..idle_ticks {
            now += tick_dur;
            let mut pkt = RtpPacket::default();
            pkt.arrival_ts = now;
            pkt.playout_time = now;
            let payload_len = IDLE_FRAME_BYTES.saturating_sub(pkt.header_len);
            pkt.payload = std::sync::Arc::from(vec![0; payload_len].as_slice());
            bwe.record(&pkt);
            bwe.poll(now);
            idle_samples.push(bwe.estimate_bps());
        }

        let final_idle = *idle_samples.last().unwrap();

        // After the 5 s window has fully slid to idle content, the estimate
        // must converge to the 2 fps bitrate with the usual 10 % headroom.
        assert!(
            final_idle >= IDLE_BPS * 1.05,
            "estimate undershot idle 2fps bitrate: final_idle={:.0} idle_bps={:.0}",
            final_idle,
            IDLE_BPS
        );
        assert!(
            final_idle <= IDLE_BPS * 2.0,
            "estimate did not converge to idle bitrate: final_idle={:.0} idle_bps={:.0}",
            final_idle,
            IDLE_BPS
        );

        // Sanity: active phase must have been well above idle.
        assert!(
            post_active > 200_000.0,
            "post-active estimate unexpectedly low: {:.0} bps",
            post_active
        );
    }

    // ── BitrateEstimate: admission/demand split ─────────────────────────────

    fn send_tick(bwe: &mut BitrateEstimate, now: &mut Instant, tick_dur: Duration, bps: f64) {
        *now += tick_dur;
        let bytes = (bps * tick_dur.as_secs_f64() / 8.0) as usize;
        let mut pkt = RtpPacket::default();
        pkt.arrival_ts = *now;
        pkt.playout_time = *now;
        let payload_len = bytes.saturating_sub(pkt.header_len);
        pkt.payload = std::sync::Arc::from(vec![0; payload_len].as_slice());
        bwe.record(&pkt);
        bwe.poll(*now);
    }

    /// A brief burst (3 ticks, 1.5s — shorter than the 7-tick/3.5s admission
    /// majority window) must raise the fast `demand_bps()` signal strongly,
    /// so a screen-share flurry can still ask str0m for headroom — but must
    /// only weakly move the conservative `estimate_bps()` (admission/cost)
    /// value, which is what used to swing wildly on VBR content and steal
    /// shared bandwidth from sibling slots. A longer, genuinely sustained
    /// burst must still, in time, raise the admission value too — it isn't
    /// ignored forever, just not trusted on one unlucky window.
    #[test]
    fn brief_burst_raises_demand_far_more_than_admission_estimate() {
        const LOW_BPS: f64 = 50_000.0;
        const BURST_BPS: f64 = 500_000.0;
        let tick_dur = Duration::from_millis(500);
        let t0 = Instant::now();
        let mut bwe = BitrateEstimate::new(t0);
        let mut now = t0;

        // Warm up fully at LOW so both signals settle.
        for _ in 0..14 {
            send_tick(&mut bwe, &mut now, tick_dur, LOW_BPS);
        }

        let mut peak_admission = 0.0_f64;
        let mut peak_demand = 0.0_f64;
        for _ in 0..3 {
            send_tick(&mut bwe, &mut now, tick_dur, BURST_BPS);
            peak_admission = peak_admission.max(bwe.estimate_bps());
            peak_demand = peak_demand.max(bwe.demand_bps());
        }

        assert!(
            peak_demand > BURST_BPS * 0.8,
            "demand signal failed to react to the burst: peak={:.0}",
            peak_demand
        );
        assert!(
            peak_admission < peak_demand * 0.5,
            "admission estimate tracked the brief burst almost as closely as \
             demand did — it should dampen a burst this short far more: \
             admission_peak={:.0} demand_peak={:.0}",
            peak_admission,
            peak_demand
        );

        // A genuinely sustained burst (5 ticks — a majority of the 7-tick
        // admission window) must still, in time, raise the admission value.
        for _ in 0..5 {
            send_tick(&mut bwe, &mut now, tick_dur, BURST_BPS);
        }
        assert!(
            bwe.estimate_bps() > LOW_BPS * 3.0,
            "admission estimate failed to confirm a genuinely sustained \
             burst: {:.0}",
            bwe.estimate_bps()
        );
    }
}
