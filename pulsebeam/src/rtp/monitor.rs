use pulsebeam_runtime::sync::Arc;
#[cfg(test)]
use pulsebeam_runtime::sync::atomic::AtomicU8;
use pulsebeam_runtime::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::ops::Deref;
use std::time::Duration;
use str0m::bwe::Bitrate;
use tokio::time::Instant;

use crate::entity::TrackKind;
use crate::rtp::RtpPacket;

const SIMULCAST_LAYER_PAUSE_TIMEOUT: Duration = Duration::from_millis(1000);
const SIMULCAST_LAYER_PAUSE_TIMEOUT_VLA: Duration = Duration::from_secs(10);
const STREAM_DEAD_TIMEOUT: Duration = Duration::from_millis(3000);
const LOSS_MEASUREMENT_WINDOW: Duration = Duration::from_millis(500);
const RATE_RISE_TIME_CONSTANT: Duration = Duration::from_millis(150);
/// Reactive-cost fall constant — matches str0m's `EstimateSmoother::ESTIMATE_WINDOW` (3 s)
/// so per-layer allocator costs converge on the same timescale as the reported BWE.
const RATE_FALL_TIME_CONSTANT: Duration = Duration::from_secs(3);
/// Stable-cost fall constant — very slow decay keeps the desired-bitrate signal high,
/// motivating str0m's probe controller to maintain headroom even when the sender
/// temporarily reduces its declared rate.
const STABLE_RATE_FALL_TIME_CONSTANT: Duration = Duration::from_secs(30);
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

#[derive(Debug)]
struct RateFilter {
    filtered_bps: f64,
    last_update: Option<Instant>,
    rise_tau: f64,
    fall_tau: f64,
}

impl RateFilter {
    fn new() -> Self {
        Self::with_taus(RATE_RISE_TIME_CONSTANT, RATE_FALL_TIME_CONSTANT)
    }

    fn with_taus(rise: Duration, fall: Duration) -> Self {
        Self {
            filtered_bps: 0.0,
            last_update: None,
            rise_tau: rise.as_secs_f64(),
            fall_tau: fall.as_secs_f64(),
        }
    }

    fn update(&mut self, now: Instant, raw_bps: f64) {
        let Some(last) = self.last_update.replace(now) else {
            self.filtered_bps = raw_bps;
            return;
        };
        let elapsed = now.saturating_duration_since(last).as_secs_f64();
        let tau = if raw_bps >= self.filtered_bps {
            self.rise_tau
        } else {
            self.fall_tau
        };
        let alpha = (-elapsed / tau).exp();
        self.filtered_bps = raw_bps + (self.filtered_bps - raw_bps) * alpha;
    }

    fn reset(&mut self) {
        self.filtered_bps = 0.0;
        self.last_update = None;
    }

    fn current(&self) -> f64 {
        self.filtered_bps
    }
}

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
        Self::new_with_height(inactive, bitrate_bps, 0)
    }

    pub fn new_with_height(inactive: bool, bitrate_bps: u64, height: u32) -> Self {
        Self(Arc::new(StreamStateInner::new(
            inactive,
            bitrate_bps,
            height,
        )))
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
    healthy: AtomicBool,
    /// Both bitrate signals packed into one atomic so they are always written
    /// and read together — eliminating the race where a snapshot could observe
    /// a new reactive value paired with a stale stable value (or vice versa).
    ///
    /// Layout: upper 32 bits = stable_bps, lower 32 bits = reactive_bps.
    /// Max representable bitrate = u32::MAX ≈ 4.3 Gbps, far above any real stream.
    bitrates: AtomicU64,
    height: AtomicU32,
    #[cfg(test)]
    quality: AtomicU8,
}

impl StreamStateInner {
    fn pack(reactive_bps: u64, stable_bps: u64) -> u64 {
        let r = reactive_bps.min(u32::MAX as u64);
        let s = stable_bps.min(u32::MAX as u64);
        (s << 32) | r
    }

    pub fn new(inactive: bool, bitrate_bps: u64, height: u32) -> Self {
        Self {
            inactive: AtomicBool::new(inactive),
            healthy: AtomicBool::new(!inactive),
            bitrates: AtomicU64::new(Self::pack(bitrate_bps, bitrate_bps)),
            height: AtomicU32::new(height),
            #[cfg(test)]
            quality: AtomicU8::new(StreamQuality::Good as u8),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn is_inactive(&self) -> bool {
        self.inactive.load(Ordering::Relaxed)
    }

    pub fn bitrate_bps(&self) -> f64 {
        (self.bitrates.load(Ordering::Relaxed) & 0xFFFF_FFFF) as f64
    }

    pub fn stable_bitrate_bps(&self) -> f64 {
        (self.bitrates.load(Ordering::Relaxed) >> 32) as f64
    }

    /// Read both signals from a single atomic load — guarantees they come from
    /// the same write and can never be a (reactive, stable) pair that was never
    /// stored together.
    pub fn bitrates_snapshot(&self) -> (f64, f64) {
        let packed = self.bitrates.load(Ordering::Relaxed);
        let reactive = (packed & 0xFFFF_FFFF) as f64;
        let stable = (packed >> 32) as f64;
        (reactive, stable)
    }

    pub fn height(&self) -> u32 {
        self.height.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn quality(&self) -> StreamQuality {
        match self.quality.load(Ordering::Relaxed) {
            0 => StreamQuality::Bad,
            2 => StreamQuality::Excellent,
            _ => StreamQuality::Good,
        }
    }

    #[cfg(test)]
    pub fn is_activation_candidate(&self) -> bool {
        self.quality() != StreamQuality::Bad
    }
}

#[cfg(test)]
pub struct StreamStateUpdater<'a> {
    state: &'a StreamStateInner,
}

#[cfg(test)]
impl<'a> StreamStateUpdater<'a> {
    pub fn bitrate(self, bps: u64) -> Self {
        let packed = StreamStateInner::pack(bps, bps);
        self.state.bitrates.store(packed, Ordering::Relaxed);
        self
    }

    pub fn stable_bitrate(self, bps: u64) -> Self {
        let current = self.state.bitrates.load(Ordering::Relaxed);
        let reactive = current & 0xFFFF_FFFF;
        self.state
            .bitrates
            .store(StreamStateInner::pack(reactive, bps), Ordering::Relaxed);
        self
    }
    pub fn height(self, height: u32) -> Self {
        self.state.height.store(height, Ordering::Relaxed);
        self
    }
    pub fn quality(self, q: StreamQuality) -> Self {
        self.state.healthy.store(
            q != StreamQuality::Bad && !self.state.inactive.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.state.quality.store(q as u8, Ordering::Relaxed);
        self
    }
    pub fn inactive(self, val: bool) -> Self {
        self.state.inactive.store(val, Ordering::Relaxed);
        self.state.healthy.store(!val, Ordering::Relaxed);
        self
    }
}

#[derive(Debug)]
pub struct StreamMonitor {
    shared_state: StreamState,
    nominal_bitrate_bps: u64,
    declared_target_bps: u64,
    vla_inactive: bool,

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

    cost_filter: RateFilter,
    stable_filter: RateFilter,

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
        #[cfg(test)]
        shared_state
            .quality
            .store(current_quality as u8, Ordering::Relaxed);
        Self {
            stream_id,
            kind,
            shared_state,
            nominal_bitrate_bps,
            declared_target_bps: 0,
            vla_inactive: false,
            last_packet_at: now,
            window_start_ts: now,
            window_start_seq: 0,
            window_highest_seq: None,
            window_actual_packets: 0,
            smoothed_loss_ratio: 0.0,
            audio_monitor,
            bwe: BitrateEstimate::new(),
            cost_filter: RateFilter::new(),
            stable_filter: RateFilter::with_taus(
                RATE_RISE_TIME_CONSTANT,
                STABLE_RATE_FALL_TIME_CONSTANT,
            ),
            current_quality,
            quality_transition_since: None,
            quality_transition_target: None,
            quality_transition_last_evidence: None,
        }
    }

    pub fn process_packet(&mut self, packet: &RtpPacket) {
        let was_inactive = self.shared_state.is_inactive();
        let may_activate = !self.vla_inactive;
        self.last_packet_at = packet.arrival_ts;
        if may_activate {
            if was_inactive {
                let activation_bitrate = if self.declared_target_bps > 0 {
                    self.declared_target_bps
                } else {
                    self.nominal_bitrate_bps
                };
                if activation_bitrate > 0 {
                    self.shared_state.bitrates.store(
                        StreamStateInner::pack(activation_bitrate, activation_bitrate),
                        Ordering::Relaxed,
                    );
                    debug_assert_ne!(self.shared_state.bitrate_bps(), 0.0);
                }
            }
            self.shared_state.inactive.store(false, Ordering::Relaxed);
            self.publish_health();
        }
        self.bwe.record(packet);

        if was_inactive && may_activate {
            self.window_highest_seq = None;
            self.window_start_seq = 0;
            self.window_actual_packets = 0;
            self.window_start_ts = packet.arrival_ts;
        }

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

    fn publish_health(&self) {
        let healthy =
            !self.shared_state.is_inactive() && self.current_quality != StreamQuality::Bad;
        self.shared_state.healthy.store(healthy, Ordering::Relaxed);
    }

    fn publish_inactive(&mut self) {
        self.shared_state.healthy.store(false, Ordering::Relaxed);
        self.shared_state.inactive.store(true, Ordering::Relaxed);
        self.shared_state.bitrates.store(0, Ordering::Relaxed);
        self.stable_filter.reset();
        debug_assert!(self.shared_state.is_inactive());
        debug_assert!(!self.shared_state.is_healthy());
        debug_assert_eq!(self.shared_state.bitrate_bps(), 0.0);
    }

    pub fn apply_vla(&mut self, target_bps: u64, height: Option<u32>) -> bool {
        let first_declaration = self.declared_target_bps == 0 && target_bps > 0;
        self.declared_target_bps = target_bps;
        self.vla_inactive = target_bps == 0;
        if let Some(height) = height {
            debug_assert_ne!(height, 0);
            self.shared_state.height.store(height, Ordering::Relaxed);
        }
        if self.vla_inactive {
            self.publish_inactive();
        }
        first_declaration
    }

    pub fn poll(&mut self, now: Instant, is_any_sibling_active: bool) {
        self.bwe.poll(now);
        // Unified raw target: prefer the VLA-declared target (set by apply_vla in
        // track.rs) because it reflects the encoder's committed rate and is therefore
        // more stable than instantaneous byte measurements. Fall back to the measured
        // tick rate when no VLA is present.
        let declared = self.declared_target_bps;
        let raw_bps = if declared > 0 {
            declared as f64
        } else {
            self.bwe.tick_bps()
        };
        let raw_with_floor = if declared > 0 {
            raw_bps
        } else {
            raw_bps.max(self.nominal_bitrate_bps as f64)
        };

        self.cost_filter.update(now, raw_with_floor);
        self.stable_filter.update(now, raw_with_floor);

        // Single packed write — reactive and stable are always observed together.
        self.shared_state.bitrates.store(
            StreamStateInner::pack(
                self.cost_filter.current() as u64,
                self.stable_filter.current() as u64,
            ),
            Ordering::Relaxed,
        );
        if let Some(audio_monitor) = self.audio_monitor.as_mut() {
            audio_monitor.poll(now);
        }

        // Step A: Inactivity & Flap Prevention
        let time_since_last_packet = now.saturating_duration_since(self.last_packet_at);
        let was_inactive = self.shared_state.is_inactive();

        // The sender's Video Layers Allocation can declare this layer inactive,
        // which lets us deactivate it at once instead of waiting out the packet
        // timeout. Unlike the timeout path this doesn't require a live sibling —
        // an explicit "off" from the sender is authoritative even for a solo layer.
        let vla_active = self.declared_target_bps > 0;
        let pause_timeout = if vla_active {
            SIMULCAST_LAYER_PAUSE_TIMEOUT_VLA
        } else {
            SIMULCAST_LAYER_PAUSE_TIMEOUT
        };
        let timed_out = time_since_last_packet > pause_timeout && is_any_sibling_active;
        if timed_out || self.vla_inactive {
            self.publish_inactive();
            if !was_inactive {
                tracing::debug!(
                    stream_id = %self.stream_id,
                    "Simulcast layer paused while siblings active; retaining its last loss classification for keyframe-gated reactivation"
                );
                self.quality_transition_since = None;
                self.quality_transition_target = None;
                self.quality_transition_last_evidence = None;
                self.cost_filter.reset();
                self.stable_filter.reset();
            }
            return;
        }

        let dead_timeout = if vla_active {
            SIMULCAST_LAYER_PAUSE_TIMEOUT_VLA
        } else {
            STREAM_DEAD_TIMEOUT
        };
        if time_since_last_packet > dead_timeout {
            self.publish_inactive();
            if !was_inactive {
                self.reset(now);
            }
            return;
        }

        self.shared_state.inactive.store(false, Ordering::Relaxed);
        self.publish_health();

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
                Bitrate::from(self.nominal_bitrate_bps),
            );
            self.current_quality = new_quality;
            self.quality_transition_since = None;
            self.quality_transition_target = None;
            self.quality_transition_last_evidence = None;
            #[cfg(test)]
            self.shared_state
                .quality
                .store(new_quality as u8, Ordering::Relaxed);
            self.publish_health();
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
        self.bwe = BitrateEstimate::new();
        self.cost_filter.reset();
        self.stable_filter.reset();
        // Audio stays stubbed Excellent even across a dead-stream reset —
        // see `new`.
        self.current_quality = match self.kind {
            TrackKind::Audio => StreamQuality::Excellent,
            TrackKind::Video | TrackKind::Data => StreamQuality::Good,
        };
        #[cfg(test)]
        self.shared_state
            .quality
            .store(self.current_quality as u8, Ordering::Relaxed);
        self.declared_target_bps = 0;
        self.vla_inactive = false;
        self.publish_inactive();
    }
}

#[derive(Debug)]
pub struct BitrateEstimate {
    tick_start: Option<Instant>,
    accumulated_bytes: usize,
    tick_bps: f64,
    warm: bool,
}

impl Default for BitrateEstimate {
    fn default() -> Self {
        Self::new()
    }
}

impl BitrateEstimate {
    /// Matches str0m's `AckedBitrateEstimator::BITRATE_WINDOW` (150 ms) so
    /// per-layer throughput samples arrive at the same frequency as str0m's
    /// internal throughput measurement.
    const TICK: Duration = Duration::from_millis(150);

    pub fn new() -> Self {
        Self {
            tick_start: None,
            accumulated_bytes: 0,
            tick_bps: 0.0,
            warm: false,
        }
    }

    pub fn record(&mut self, pkt: &RtpPacket) {
        self.advance_time(pkt.playout_time);
        self.accumulated_bytes += pkt.header_len + pkt.payload.len();
    }

    pub fn poll(&mut self, now: Instant) {
        self.advance_time(now);
    }

    fn advance_time(&mut self, time: Instant) {
        let current_tick = *self.tick_start.get_or_insert(time);
        if time < current_tick + Self::TICK {
            return;
        }
        let elapsed = time.saturating_duration_since(current_tick);
        let ticks_passed = (elapsed.as_millis() / Self::TICK.as_millis()) as usize;
        self.tick_bps = (self.accumulated_bytes as f64 * 8.0) / Self::TICK.as_secs_f64();
        self.accumulated_bytes = 0;
        self.warm = true;
        // If more than one tick elapsed, the stream was silent for those ticks;
        // report zero for the current reading (the filter in StreamMonitor will
        // decay slowly, as intended).
        if ticks_passed > 1 {
            self.tick_bps = 0.0;
        }
        self.tick_start = Some(current_tick + Self::TICK * ticks_passed as u32);
    }

    pub fn tick_bps(&self) -> f64 {
        self.tick_bps
    }

    pub fn is_warm(&self) -> bool {
        self.warm
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

    /// A 1% loss rate must be reported as roughly 1%, not tens of percent.
    ///
    /// The upstream monitor gates stream health, and an unhealthy layer is dropped from
    /// allocation entirely - so an over-reported loss ratio silently pauses a stream that the
    /// link could carry. In simulation, a link configured to drop 1% produced a smoothed ratio of
    /// 21-35% and the SFU paused the stream while BWE still reported 1.9 Mbps available.
    #[test]
    fn loss_ratio_tracks_actual_loss_rate() {
        let shared = StreamState::new(false, 1_250_000);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "high".into(), shared.clone());
        let start = Instant::now();

        // 30 packets per 500ms window, 20 windows. Drop every 100th packet: exactly 1%.
        let mut t = start;
        let mut seq = 1u64;
        for _ in 0..20 {
            for _ in 0..30 {
                if !seq.is_multiple_of(100) {
                    monitor.process_packet(&packet(seq, t));
                }
                seq += 1;
                t += Duration::from_millis(500 / 30);
            }
            monitor.poll(t, false);
        }

        let reported = monitor.smoothed_loss_ratio;
        assert!(
            reported < 0.05,
            "1% packet loss should report as roughly 1%, got {:.1}%. An inflated ratio marks the \
             layer unhealthy and pauses a stream the link can carry.",
            reported * 100.0
        );
    }

    /// 1% loss must still read as ~1% when the network also reorders packets.
    ///
    /// This is the case that breaks in simulation. `interval_loss` compares `expected` - a
    /// sequence span sampled at window close - against packets that arrived *within* the window.
    /// A packet reordered across the boundary is missing from the window that expected it, and
    /// when it lands in the next window `saturating_sub` clamps the correction away. Windows can
    /// therefore over-report but never under-report, and the deliberately asymmetric EWMA
    /// (0.50 rising, 0.20 falling) turns that one-sided noise into a persistently high value.
    #[test]
    fn loss_ratio_tracks_actual_loss_rate_with_reordering() {
        let shared = StreamState::new(false, 1_250_000);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "high".into(), shared.clone());
        let start = Instant::now();

        // Same 1% loss, but each packet's arrival is displaced by up to +/-3 positions, which is
        // what a few ms of jitter does to a stream sent ~1ms apart.
        let mut t = start;
        let mut seq = 1u64;
        for _ in 0..20 {
            let mut batch: Vec<u64> = Vec::new();
            for _ in 0..30 {
                if !seq.is_multiple_of(100) {
                    batch.push(seq);
                }
                seq += 1;
            }
            // Deterministic local shuffle: swap adjacent pairs three apart.
            for i in (0..batch.len().saturating_sub(3)).step_by(6) {
                batch.swap(i, i + 3);
            }
            for s in batch {
                monitor.process_packet(&packet(s, t));
                t += Duration::from_millis(500 / 30);
            }
            monitor.poll(t, false);
        }

        let reported = monitor.smoothed_loss_ratio;
        assert!(
            reported < 0.05,
            "1% packet loss with mild reordering should still report as roughly 1%, got {:.1}%",
            reported * 100.0
        );
    }

    /// A low-rate stream must not be declared Bad by one or two lost packets.
    ///
    /// This is the screen-share case: static content encodes at 2-5 fps, so a measurement window
    /// holds only a handful of packets. `MIN_LOSS_EVIDENCE_PACKETS` was 5, and
    /// `VIDEO_SEVERE_LOSS_THRESHOLD` (0.30) transitions to Bad *immediately* with no confirmation
    /// window - so two losses out of six expected read as 33% and instantly marked the layer
    /// unhealthy. An unhealthy layer is dropped from allocation, so the SFU paused a stream the
    /// link could carry. Observed in simulation at 1% link loss with windows of `expected: 6,
    /// actual: 4` and `expected: 14, actual: 8`.
    #[test]
    fn sparse_low_rate_stream_survives_occasional_loss() {
        let shared = StreamState::new(false, 400_000);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "high".into(), shared.clone());
        let start = Instant::now();

        // ~4 fps: two packets per frame, so a window sees only a handful of packets. Loss is
        // pseudo-random at 1% rather than every-Nth, because what actually breaks is two drops
        // landing in the same sparse window - which an evenly spaced pattern never produces.
        let mut rng: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next_rand = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        let mut t = start;
        let mut seq = 1u64;
        for _ in 0..200 {
            for _ in 0..4 {
                if next_rand() % 100 != 0 {
                    monitor.process_packet(&packet(seq, t));
                }
                seq += 1;
                t += Duration::from_millis(125);
            }
            monitor.poll(t, false);
        }

        assert_ne!(
            monitor.current_quality,
            StreamQuality::Bad,
            "a 1% loss rate must not mark a low-frame-rate layer unhealthy; smoothed ratio was \
             {:.1}%",
            monitor.smoothed_loss_ratio * 100.0
        );
    }

    #[test]
    fn video_upstream_bitrate_never_falls_below_nominal_layer_rate() {
        let nominal = 1_250_000u64;
        let shared = StreamState::new(false, nominal);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "high".into(), shared.clone());
        let now = Instant::now();

        // First poll: no declared VLA target and bwe not warm yet (tick_bps=0).
        // The nominal floor must keep bitrate_bps at the nominal rate.
        monitor.process_packet(&packet(1, now));
        monitor.poll(now + Duration::from_millis(600), false);
        assert_ge!(shared.bitrate_bps(), nominal as f64);

        // Drive many ticks at a rate far below nominal — the cost filter starts
        // from the nominal floor, and slow-fall keeps it well above the low rate
        // even after several seconds.
        let mut t = now + Duration::from_millis(600);
        for seq in 2..20u64 {
            // One tiny packet per tick: ~80 bps — far below nominal.
            monitor.process_packet(&packet(seq, t));
            t += Duration::from_millis(600);
            monitor.poll(t, false);
        }
        // After sustained low-rate ticks the slow-fall filter still holds
        // close to nominal (4s fall tau: ~e^(-8/4) ≈ 0.135 decay ratio
        // over 8s, so bitrate stays well above nominal * 0.5).
        assert_ge!(shared.bitrate_bps(), nominal as f64 * 0.5);
    }

    #[test]
    fn vla_inactive_deactivates_layer_without_waiting_for_timeout() {
        let shared = StreamState::new(false, 400_000);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "v0".into(), shared.clone());
        let now = Instant::now();

        // Fresh packet, no VLA inactivity, no active sibling: stays active.
        monitor.process_packet(&packet(1, now));
        monitor.poll(now, false);
        assert!(!shared.is_inactive());

        // The sender declares this layer inactive via VLA. It deactivates on the
        // next poll even though the packet is recent and there's no sibling — no
        // 1s timeout wait.
        monitor.apply_vla(0, None);
        assert!(shared.is_inactive());
        monitor.poll(now + Duration::from_millis(50), false);
        assert!(
            shared.is_inactive(),
            "VLA-declared-inactive layer must deactivate immediately"
        );

        // Sender re-activates it; fresh packets bring it back.
        monitor.apply_vla(400_000, None);
        monitor.process_packet(&packet(2, now + Duration::from_millis(60)));
        monitor.poll(now + Duration::from_millis(70), false);
        assert!(
            !shared.is_inactive(),
            "layer must reactivate once the sender declares it active again"
        );
    }

    #[test]
    fn packet_cannot_reactivate_vla_inactive_layer() {
        let shared = StreamState::new(false, 400_000);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "q".into(), shared.clone());
        let now = Instant::now();

        monitor.apply_vla(0, None);
        monitor.process_packet(&packet(1, now));

        assert!(shared.is_inactive());
        assert!(!shared.is_healthy());

        monitor.apply_vla(400_000, None);
        monitor.process_packet(&packet(2, now + Duration::from_millis(10)));

        assert!(!shared.is_inactive());
        assert!(shared.is_healthy());
        assert_eq!(shared.bitrate_bps(), 400_000.0);
    }

    #[test]
    fn height_is_always_resolved_to_fallback_or_vla_value() {
        let shared = StreamState::new_with_height(true, 400_000, 360);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "h".into(), shared.clone());

        assert_eq!(shared.height(), 360);

        monitor.apply_vla(400_000, None);
        assert_eq!(shared.height(), 360);

        monitor.apply_vla(400_000, Some(1056));
        assert_eq!(shared.height(), 1056);
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

    fn make_packet(now: Instant, size_bytes: usize) -> RtpPacket {
        let mut pkt = RtpPacket::default();
        pkt.arrival_ts = now;
        pkt.playout_time = now;
        let payload_len = size_bytes.saturating_sub(pkt.header_len);
        pkt.payload = std::sync::Arc::from(vec![0; payload_len].as_slice());
        pkt
    }

    fn send_tick(bwe: &mut BitrateEstimate, now: &mut Instant, tick_dur: Duration, bps: f64) {
        *now += tick_dur;
        let bytes = (bps * tick_dur.as_secs_f64() / 8.0) as usize;
        bwe.record(&make_packet(*now, bytes));
        bwe.poll(*now);
    }

    #[test]
    fn bwe_tick_bps_accurately_tracks_cbr_rate() {
        let t0 = Instant::now();
        let mut bwe = BitrateEstimate::new();
        let mut now = t0;

        // Send steady 1 Mbit/s: 62500 bytes per 500ms tick.
        for _ in 0..10 {
            send_tick(&mut bwe, &mut now, BitrateEstimate::TICK, 1_000_000.0);
        }
        assert!(bwe.is_warm());
        // tick_bps should closely reflect 1 Mbit/s (within 5% of accounting for
        // header overhead).
        assert_ge!(bwe.tick_bps(), 900_000.0);
        assert_le!(bwe.tick_bps(), 1_100_000.0);
    }

    #[test]
    fn bwe_silence_produces_zero_tick_bps() {
        let t0 = Instant::now();
        let mut bwe = BitrateEstimate::new();
        let mut now = t0;

        // Two send_tick calls are required before the first tick boundary fires
        // and is_warm becomes true (the first call seeds tick_start; the second
        // advances past it).
        send_tick(&mut bwe, &mut now, BitrateEstimate::TICK, 500_000.0);
        send_tick(&mut bwe, &mut now, BitrateEstimate::TICK, 500_000.0);
        assert!(bwe.is_warm());

        // Skip 2 full ticks with no packets.
        now += BitrateEstimate::TICK * 2;
        bwe.poll(now);
        assert_eq!(
            bwe.tick_bps(),
            0.0,
            "missed ticks must produce zero tick_bps"
        );
    }

    #[test]
    fn rate_filter_fast_rise_slow_fall() {
        let now = Instant::now();
        let mut f = RateFilter::new();

        // First update seeds the filter.
        f.update(now, 100_000.0);
        assert_eq!(f.current(), 100_000.0);

        // Rise: jump to 1 Mbit/s. After one RATE_RISE_TIME_CONSTANT tau
        // the filter should be > 50% of the way there.
        let t1 = now + RATE_RISE_TIME_CONSTANT;
        f.update(t1, 1_000_000.0);
        let after_rise = f.current();
        assert_ge!(
            after_rise,
            550_000.0,
            "fast rise: filter should be at least 55% toward new value after 1 tau"
        );

        // Fall: drop to 0 — after RATE_RISE_TIME_CONSTANT (much less than RATE_FALL_TIME_CONSTANT)
        // the filter should still hold most of its value.
        let t2 = t1 + RATE_RISE_TIME_CONSTANT;
        f.update(t2, 0.0);
        let after_short_fall = f.current();
        assert_ge!(
            after_short_fall,
            after_rise * 0.7,
            "slow fall: filter should retain most value after 1 rise-tau"
        );

        // After a full RATE_FALL_TIME_CONSTANT has elapsed the filter should be
        // about 63% decayed (1/e ≈ 0.37 remaining).
        let t3 = t2 + RATE_FALL_TIME_CONSTANT;
        f.update(t3, 0.0);
        let after_full_tau = f.current();
        assert_le!(
            after_full_tau,
            after_rise * 0.45,
            "slow fall: filter should be well decayed after one full fall tau"
        );
    }

    #[test]
    fn stream_monitor_cost_filter_unifies_vla_and_measured() {
        // With a declared VLA target, bitrate_bps should reflect the smoothed
        // VLA value, not the raw measured tick rate.
        let shared = StreamState::new(false, 0);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "vla".into(), shared.clone());
        let now = Instant::now();

        // Seed one measured-rate tick at a very low rate.
        let tiny_pkt = make_packet(now, 100);
        monitor.process_packet(&tiny_pkt);
        monitor.poll(now + BitrateEstimate::TICK, false);

        // Now inject a VLA-declared target of 1 Mbit/s.
        monitor.apply_vla(1_000_000, None);
        monitor.poll(now + BitrateEstimate::TICK * 2, false);

        // bitrate_bps should now be influenced by the VLA target (rising toward 1M).
        assert_ge!(
            shared.bitrate_bps(),
            400_000.0,
            "after 1 rise-tau with VLA=1M, bitrate_bps should have risen substantially"
        );
    }

    #[test]
    fn vla_active_layer_survives_packet_silence_with_sibling() {
        let nominal = 400_000u64;
        let shared = StreamState::new(false, nominal);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "h".into(), shared.clone());
        let now = Instant::now();

        monitor.process_packet(&packet(1, now));
        monitor.apply_vla(875_000, None);

        // >1 s of silence with an active sibling — old 1 s timeout would fire.
        monitor.poll(now + Duration::from_millis(1500), true);

        assert!(
            !shared.is_inactive(),
            "VLA-declared layer must survive 1.5 s packet silence"
        );
        assert!(
            shared.bitrate_bps() >= 875_000.0 * 0.5,
            "cost should be near VLA target, not zero"
        );
    }

    #[test]
    fn no_vla_layer_still_times_out_with_sibling() {
        let nominal = 400_000u64;
        let shared = StreamState::new(false, nominal);
        let mut monitor = StreamMonitor::new(TrackKind::Video, "h".into(), shared.clone());
        let now = Instant::now();

        monitor.process_packet(&packet(1, now));

        // No VLA declared — 1 s timeout still applies.
        monitor.poll(now + Duration::from_millis(1200), true);

        assert!(
            shared.is_inactive(),
            "non-VLA layer must time out after 1.2 s with active sibling"
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
}
