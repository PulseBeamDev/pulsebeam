use std::{sync::Arc, time::Duration};

use str0m::media::MediaTime;
use tokio::sync::watch;

use crate::{MediaFrame, agent::LocalEncoding};

#[derive(Clone)]
pub struct KeyframeNotifier(watch::Sender<u64>);

#[derive(Clone, Debug)]
pub struct KeyframeReceiver(watch::Receiver<u64>);

impl KeyframeNotifier {
    pub(crate) fn pair() -> (Self, KeyframeReceiver) {
        let (tx, rx) = watch::channel(0u64);
        (KeyframeNotifier(tx), KeyframeReceiver(rx))
    }

    pub fn notify(&self) {
        self.0.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub(crate) fn receiver(&self) -> KeyframeReceiver {
        KeyframeReceiver(self.0.subscribe())
    }
}

impl KeyframeReceiver {
    pub fn is_requested(&mut self) -> bool {
        if self.0.has_changed().unwrap_or(false) {
            let _ = self.0.borrow_and_update();
            true
        } else {
            false
        }
    }
}

pub struct SharedH264Asset {
    pub(crate) frames: Vec<Arc<[u8]>>,
    pub(crate) first_idr: usize,
}

impl SharedH264Asset {
    pub fn new(data: &[u8]) -> Self {
        let slicer = H264FrameSlicer::new(data);
        let frames: Vec<Arc<[u8]>> = slicer.map(Arc::from).collect();
        let first_idr = Self::find_first_idr(&frames);

        Self { frames, first_idr }
    }

    fn find_first_idr(frames: &[Arc<[u8]>]) -> usize {
        frames
            .iter()
            .position(|f| Self::frame_has_idr(f))
            .unwrap_or(0)
    }

    fn frame_has_idr(frame: &[u8]) -> bool {
        let mut i = 0usize;
        while i.saturating_add(3) < frame.len() {
            let (Some(&b0), Some(&b1), Some(&b2)) = (
                frame.get(i),
                frame.get(i.saturating_add(1)),
                frame.get(i.saturating_add(2)),
            ) else {
                break;
            };
            if b0 == 0 && b1 == 0 {
                let header_pos = if b2 == 1 {
                    i.saturating_add(3)
                } else if b2 == 0 && frame.get(i.saturating_add(3)) == Some(&1) {
                    i.saturating_add(4)
                } else {
                    i = i.saturating_add(1);
                    continue;
                };
                if frame.get(header_pos).is_some_and(|&b| b & 0x1F == 5) {
                    return true;
                }
                i = header_pos;
            } else {
                i = i.saturating_add(1);
            }
        }
        false
    }
}

/// Convert a computed rate or timestamp to an integer without letting a NaN or
/// a negative silently become a plausible value — `as u64` yields 0 for both,
/// which reads downstream as "no bitrate" or "time zero".
pub(crate) fn saturating_u64_from_f64(v: f64) -> u64 {
    debug_assert!(v.is_finite(), "{v} is not a finite quantity");
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to a positive, finite value below u64::MAX above"
    )]
    {
        v.min(u64::MAX as f64) as u64
    }
}

/// Rewrite every Annex-B NAL unit type to a non-IDR coded slice (type 1),
/// preserving the framing so str0m still packetizes and depacketizes the stream
/// but the SFU's h264::classify finds no IDR, SPS, or PPS. This simulates
/// SFrame/E2EE, where the media bitstream is opaque to the SFU and the Dependency
/// Descriptor is the only keyframe signal.
fn opaque_frame(frame: &[u8]) -> Arc<[u8]> {
    const NON_IDR_SLICE: u8 = 1;
    let mut out = frame.to_vec();
    let mut i = 0usize;
    while i.saturating_add(3) < out.len() {
        let start_code = out.get(i) == Some(&0)
            && out.get(i.saturating_add(1)) == Some(&0)
            && out.get(i.saturating_add(2)) == Some(&1);
        if start_code {
            let header = i.saturating_add(3);
            // Keep forbidden_zero_bit + nal_ref_idc, replace only the 5-bit type.
            if let Some(byte) = out.get_mut(header) {
                *byte = (*byte & 0xE0) | NON_IDR_SLICE;
            }
            i = header.saturating_add(1);
        } else {
            i = i.saturating_add(1);
        }
    }
    out.into()
}

pub struct H264Looper {
    asset: Arc<SharedH264Asset>,
    index: usize,
    fps: u32,
    /// When set, attach a synthetic temporal Dependency Descriptor to each frame
    /// so the SFU can exercise decode-target shedding. The DD's temporal pattern
    /// is independent of the (non-scalable) asset — it drives the SFU's DD path,
    /// not real H.264 temporal decodability.
    dd: Option<pulsebeam_core::dd::temporal::TemporalDdSource>,
    /// Simulate SFrame/E2EE by making the payload opaque before sending: the H.264
    /// start codes are overwritten so the SFU's payload probe finds no IDR, SPS, or
    /// PPS, leaving the Dependency Descriptor as the only forwarding signal.
    opaque_payload: bool,
}

impl H264Looper {
    pub fn new(data: &[u8], fps: u32) -> Self {
        let asset = Arc::new(SharedH264Asset::new(data));
        Self {
            asset,
            index: 0,
            fps,
            dd: None,
            opaque_payload: false,
        }
    }

    pub fn new_shared(asset: Arc<SharedH264Asset>, fps: u32) -> Self {
        Self {
            asset,
            index: 0,
            fps,
            dd: None,
            opaque_payload: false,
        }
    }

    /// Emit a synthetic L1T{temporal_layers} Dependency Descriptor per frame.
    pub fn with_temporal_layers(mut self, temporal_layers: u8) -> Self {
        self.dd = Some(pulsebeam_core::dd::temporal::TemporalDdSource::new(
            temporal_layers,
        ));
        self
    }

    /// Make the payload opaque before it is sent, simulating SFrame/E2EE where the
    /// SFU cannot read the codec bitstream and must rely on the Dependency
    /// Descriptor alone. Only meaningful alongside `with_temporal_layers`.
    pub fn with_opaque_payload(mut self) -> Self {
        self.opaque_payload = true;
        self
    }

    fn next(&mut self) -> Arc<[u8]> {
        debug_assert!(
            self.index < self.asset.frames.len(),
            "frame cursor left the asset"
        );
        let frame = self
            .asset
            .frames
            .get(self.index)
            .cloned()
            .unwrap_or_default();
        let frame = &frame;
        self.index = self
            .index
            .saturating_add(1)
            .checked_rem(self.asset.frames.len())
            .unwrap_or(0);
        if self.opaque_payload {
            return opaque_frame(frame);
        }
        frame.clone()
    }

    pub async fn run(mut self, mut sender: LocalEncoding) {
        let clock_rate = 90_000u64;
        let frame_interval = Duration::from_secs_f64(1.0 / self.fps as f64);
        let mid = sender.mid;
        let rid = sender.rid;

        let temporal_layers = self
            .dd
            .as_ref()
            .map(pulsebeam_core::dd::temporal::TemporalDdSource::temporal_layers)
            .unwrap_or(1);
        let mut interval = tokio::time::interval(frame_interval);
        let mut frame_count: u64 = 0;
        // The pipeline owns Dependency Descriptor generation; the source only
        // declares its scalability depth and which frames are keyframes.
        let mut frame_sender = crate::pipeline::FrameSender::new(mid, rid, 1, temporal_layers);

        loop {
            let tick_time = interval.tick().await;

            if sender.keyframe_rx.is_requested() {
                tracing::debug!(
                    ?mid,
                    ?rid,
                    first_idr = self.asset.first_idr,
                    "keyframe reset"
                );
                self.index = self.asset.first_idr;
            }

            let is_keyframe = self.index == self.asset.first_idr;
            let frame_data = self.next();
            let next_ts = frame_count
                .saturating_mul(clock_rate)
                .checked_div(self.fps as u64)
                .unwrap_or(0);

            let frame = MediaFrame {
                audio_level: None,
                voice_activity: None,
                ts: MediaTime::from_90khz(next_ts),
                data: frame_data,
                capture_time: tick_time,
                abs_capture_time: Some(crate::clock::capture_wallclock()),
                contiguous: true,
                is_keyframe,
                target_bitrate_bps: None,
                resolution: None,
                dependency_descriptor: None,
                temporal_layers: None,
            };

            for packet in frame_sender.packetize(&frame) {
                if sender.send(packet).await.is_err() {
                    return;
                }
            }
            frame_count = frame_count.saturating_add(1);
        }
    }
}

/// How a [`VbrLooper`] alternates between busy and quiet content.
///
/// Models screen sharing, which is strongly variable-bitrate: a static desktop encodes to almost
/// nothing because the encoder simply skips frames, then scrolling or a window change produces a
/// burst at the full layer rate. Camera video, by contrast, is close to constant-bitrate.
///
/// The quiet phase is the interesting one for congestion control. The sender becomes application
/// limited, which is what puts str0m into ALR and makes the probe controller - rather than the
/// arrival of media - solely responsible for keeping the bandwidth estimate alive. If probing
/// stalls there, the estimate decays while the screen is still, and the burst when the user
/// scrolls again has nowhere to go.
#[derive(Debug, Clone, Copy)]
pub struct VbrProfile {
    /// How long a burst of activity lasts.
    pub active: Duration,
    /// Frame rate during activity.
    pub active_fps: u32,
    /// How long the content stays static.
    pub idle: Duration,
    /// Frame rate while static. Real encoders drop to a few frames per second, so the bitrate
    /// falls by roughly `active_fps / idle_fps`.
    pub idle_fps: u32,
    /// Only emit frames at or below this size while static.
    ///
    /// Frame *rate* alone does not reproduce what a static screen does to the sender. A still
    /// desktop encodes near-empty P-frames - tens to a couple of hundred bytes - where moving
    /// content produces frames several times the MTU. That difference is what reaches congestion
    /// control: padding is drawn from the RTX cache of recently sent packets, so a quiet screen
    /// leaves only tiny packets to pad with, and str0m emits one padding packet per event-loop
    /// round trip. A probe cluster then cannot reach a high target however long it runs.
    pub idle_max_frame_bytes: usize,
    /// Silence between the end of one replay of the schedule and the start of the next.
    ///
    /// A truly static screen emits nothing at all, for as long as nobody touches it. That is a
    /// different regime from a low frame *rate*: past a few seconds of silence the SFU marks the
    /// layer dead, it drops out of `desired`, and the RTX cache the pacer draws padding from
    /// drains - so probes have nothing to send and report a rate far below the link. A schedule
    /// captured from a real screen share still has a frame every second or two, which never
    /// reaches that regime.
    pub loop_idle: Duration,
    /// What the encoder declares this layer will cost, in bits per second.
    ///
    /// Sent as a Video Layers Allocation, and the reason it exists separately from the frames is
    /// that the two genuinely disagree: a still screen encodes almost nothing while still being a
    /// 2.5 Mbps layer the moment anyone scrolls. The SFU cannot infer that from bytes on the
    /// wire, so the sender declares it.
    pub declared_target_bps: u64,
    /// What the declared target falls to while the content is static.
    ///
    /// Real encoders step their target down when there is nothing to send rather than dropping it
    /// to zero - the production log shows the same layer at 1250 kbps and later at 729. Those
    /// steps are what an allocator has to stay stable across.
    pub idle_target_bps: u64,
    /// How much of the gap to the current goal the declared target closes per frame.
    ///
    /// Chrome's `targetBitrate` climbs in visible steps rather than jumping - roughly 1.5 to 2.0
    /// to 2.5 Mbps over a couple of seconds once a share becomes active. A two-state target would
    /// cross every allocation boundary in one go and skip the intermediate values entirely, which
    /// are exactly the ones that sit near a decision threshold.
    pub target_step: f64,
}

impl VbrProfile {
    /// Screen sharing: long static stretches broken by short scrolls.
    ///
    /// 2 fps against 30 fps is about a 15x bitrate drop, deep enough to hold the sender in ALR
    /// for the whole quiet phase.
    pub fn screenshare() -> Self {
        Self {
            active: Duration::from_secs(4),
            active_fps: 30,
            idle: Duration::from_secs(20),
            idle_fps: 2,
            // Small enough to land in a single sub-MTU RTP packet, as a near-empty P-frame does.
            idle_max_frame_bytes: 300,
            loop_idle: Duration::from_millis(500),
            // The client's `detail` preset: one layer at 2.5 Mbps (see pulsebeam-js
            // libs/core/src/preset.ts, VIDEO_PRESETS.detail).
            declared_target_bps: 2_500_000,
            target_step: 0.25,
            // Roughly the 729/1250 step seen in production.
            idle_target_bps: 1_460_000,
        }
    }

    /// Screen sharing as the client actually configures it: a single 2.5 Mbps layer.
    ///
    /// `VIDEO_PRESETS.detail` in the client is `layers: 1, baseBitrate: 2_500_000` - the full
    /// ladder is negotiated but only `f` is ever sent, because resolution is worth more than
    /// adaptability for screen content. Two consequences the other profiles do not reproduce:
    ///
    /// * It costs 2.5 Mbps, twice the camera's top layer. Where the two nearly fill the estimate,
    ///   the allocator's choice between them is finely balanced - which is the regime where
    ///   pricing bugs surface.
    /// * With one live layer there is nothing to fall back to, so it pauses outright instead of
    ///   degrading. A viewer sees the share vanish rather than soften.
    pub fn screenshare_detail() -> Self {
        Self {
            // Production peaks at 8fps, not the preset's 15 cap. `maintain-resolution` means the
            // encoder sheds frames rather than pixels, so a screen share rarely reaches its
            // configured ceiling: Chrome's framesPerSecond for a live 1080p share sits between 0
            // and 8, spiky, with long stretches at zero.
            active_fps: 8,
            ..Self::screenshare()
        }
    }

    /// Screen sharing that goes genuinely still: the captured schedule, then a long silence.
    ///
    /// `loop_idle` is well past the SFU's 3s stream-dead timeout, so the layer is marked
    /// unhealthy and the pacer's RTX cache empties - the conditions under which probing starves
    /// and the estimate collapses. [`Self::screenshare`] never gets there; its schedule has a
    /// frame every two seconds.
    pub fn screenshare_static() -> Self {
        Self {
            loop_idle: Duration::from_secs(20),
            ..Self::screenshare()
        }
    }
}

/// An [`H264Looper`] whose output follows a [`VbrProfile`], approximating a VBR encoder.
///
/// Varies both frame rate and frame size, because congestion control reacts to each differently:
/// rate governs whether the sender is application-limited (and so whether str0m enters ALR), while
/// size governs what ends up in the RTX cache and therefore how large a padding packet a probe can
/// draw. Reproducing the production failure needs both.
///
/// Frames are always taken whole from the asset rather than synthesised, so every emitted frame
/// stays a valid, decodable H.264 access unit - the receiver-side QoE checks would reject
/// truncated ones.
pub struct VbrLooper {
    asset: Arc<SharedH264Asset>,
    index: usize,
    /// Indices of frames small enough to stand in for static content, ascending.
    small: Vec<usize>,
    small_index: usize,
    profile: VbrProfile,
    frame_times: Option<Vec<Duration>>,
    /// Current declared target, moved toward its goal a step at a time.
    declared_bps: f64,
}

impl VbrLooper {
    /// Move the declared target toward whichever goal the content is currently at, and report it.
    ///
    /// The gap between this and what is actually on the wire is the whole point of declaring it.
    /// A static screen sends almost nothing - a few kbps of near-empty frames - while remaining a
    /// 2.5 Mbps layer the instant the user scrolls. No amount of measuring bytes recovers that,
    /// which is why the sender says so directly.
    fn step_target(&mut self, active: bool) -> u64 {
        let goal = if active {
            self.profile.declared_target_bps
        } else {
            self.profile.idle_target_bps
        } as f64;
        self.declared_bps += (goal - self.declared_bps) * self.profile.target_step;
        saturating_u64_from_f64(self.declared_bps.round())
    }

    pub fn new(data: &[u8], profile: VbrProfile) -> Self {
        let asset = Arc::new(SharedH264Asset::new(data));
        let small: Vec<usize> = asset
            .frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.len() <= profile.idle_max_frame_bytes)
            .map(|(i, _)| i)
            .collect();
        Self {
            asset,
            index: 0,
            small,
            small_index: 0,
            profile,
            frame_times: None,
            declared_bps: 0.0,
        }
    }

    pub fn new_scheduled(data: &[u8], timing: &str, profile: VbrProfile) -> Self {
        let mut looper = Self::new(data, profile);
        let frame_times: Vec<Duration> = timing
            .lines()
            .map(|line| Duration::from_micros(line.parse().unwrap_or_default()))
            .collect();
        debug_assert!(!frame_times.is_empty());
        debug_assert_eq!(looper.asset.frames.len(), frame_times.len());
        debug_assert!(
            frame_times
                .windows(2)
                .all(|pair| matches!(pair, [a, b] if a < b))
        );
        looper.frame_times = Some(frame_times);
        looper
    }

    /// Next frame for the static phase: the smallest frames the asset has. Falls back to the
    /// normal sequence when the asset has nothing small enough.
    fn next_small(&mut self) -> Arc<[u8]> {
        if self.small.is_empty() {
            return self.next();
        }
        let slot = self.small_index.checked_rem(self.small.len()).unwrap_or(0);
        let idx = self.small.get(slot).copied().unwrap_or(0);
        self.small_index = self.small_index.wrapping_add(1);
        self.asset.frames.get(idx).cloned().unwrap_or_default()
    }

    fn next(&mut self) -> Arc<[u8]> {
        debug_assert!(
            self.index < self.asset.frames.len(),
            "frame cursor left the asset"
        );
        let frame = self
            .asset
            .frames
            .get(self.index)
            .cloned()
            .unwrap_or_default();
        let frame = &frame;
        self.index = self
            .index
            .saturating_add(1)
            .checked_rem(self.asset.frames.len())
            .unwrap_or(0);
        frame.clone()
    }

    /// Whether we are in an active burst at `elapsed` into the run.
    fn is_active(&self, elapsed: Duration) -> bool {
        let cycle = self
            .profile
            .active
            .checked_add(self.profile.idle)
            .unwrap_or(self.profile.active);
        if cycle.is_zero() {
            return true;
        }
        let phase = u64::try_from(
            elapsed
                .as_nanos()
                .checked_rem(cycle.as_nanos())
                .unwrap_or(0),
        )
        .unwrap_or(u64::MAX);
        Duration::from_nanos(phase) < self.profile.active
    }

    pub async fn run(mut self, mut sender: LocalEncoding) {
        let clock_rate = 90_000f64;
        let mid = sender.mid;
        let rid = sender.rid;
        let mut frame_sender = crate::pipeline::FrameSender::new(mid, rid, 1, 1);

        let start = tokio::time::Instant::now();
        if let Some(frame_times) = self.frame_times.take() {
            debug_assert_eq!(self.asset.frames.len(), frame_times.len());
            let loop_duration = frame_times
                .last()
                .copied()
                .unwrap_or_default()
                .saturating_add(self.profile.loop_idle);
            let mut loop_start = start;
            let mut index = 0usize;
            loop {
                debug_assert!(index < frame_times.len());
                let due = frame_times
                    .get(index)
                    .and_then(|offset| loop_start.checked_add(*offset))
                    .unwrap_or(loop_start);
                tokio::time::sleep_until(due).await;
                let now = tokio::time::Instant::now();
                if sender.keyframe_rx.is_requested() {
                    index = self.asset.first_idr;
                    debug_assert_eq!(index, 0, "scheduled fixture must begin with its first IDR");
                    loop_start = now;
                }
                debug_assert!(index < self.asset.frames.len());
                let frame = MediaFrame {
                    audio_level: None,
                    voice_activity: None,
                    ts: MediaTime::from_90khz(saturating_u64_from_f64(
                        now.duration_since(start).as_secs_f64() * clock_rate,
                    )),
                    data: self.asset.frames.get(index).cloned().unwrap_or_default(),
                    capture_time: now,
                    abs_capture_time: Some(crate::clock::capture_wallclock()),
                    contiguous: true,
                    is_keyframe: index == self.asset.first_idr,
                    target_bitrate_bps: Some(
                        self.step_target(self.is_active(now.duration_since(start))),
                    ),
                    resolution: None,
                    dependency_descriptor: None,
                    temporal_layers: None,
                };
                for packet in frame_sender.packetize(&frame) {
                    if sender.send(packet).await.is_err() {
                        return;
                    }
                }
                index = index.saturating_add(1);
                if index == frame_times.len() {
                    index = 0;
                    loop_start = loop_start.checked_add(loop_duration).unwrap_or(loop_start);
                }
            }
        }
        let mut next_frame_at = start;

        loop {
            tokio::time::sleep_until(next_frame_at).await;
            let now = tokio::time::Instant::now();
            let elapsed = now.duration_since(start);

            let active = self.is_active(elapsed);
            let fps = if active {
                self.profile.active_fps
            } else {
                self.profile.idle_fps
            }
            .max(1);
            next_frame_at = now
                .checked_add(Duration::from_secs_f64(1.0 / fps as f64))
                .unwrap_or(now);

            if sender.keyframe_rx.is_requested() {
                tracing::debug!(
                    ?mid,
                    ?rid,
                    first_idr = self.asset.first_idr,
                    "keyframe reset"
                );
                self.index = self.asset.first_idr;
            }

            // Derive the timestamp from wall-clock elapsed rather than a frame counter: the frame
            // rate changes between phases, so a counter would drift against real time and the
            // receiver would see the media clock stall during quiet stretches.
            let is_keyframe = active && self.index == self.asset.first_idr;
            let data = if active {
                self.next()
            } else {
                self.next_small()
            };
            let frame = MediaFrame {
                audio_level: None,
                voice_activity: None,
                ts: MediaTime::from_90khz(saturating_u64_from_f64(
                    elapsed.as_secs_f64() * clock_rate,
                )),
                data,
                capture_time: now,
                abs_capture_time: Some(crate::clock::capture_wallclock()),
                contiguous: true,
                is_keyframe,
                target_bitrate_bps: Some(self.step_target(active)),
                resolution: None,
                dependency_descriptor: None,
                temporal_layers: None,
            };

            for packet in frame_sender.packetize(&frame) {
                if sender.send(packet).await.is_err() {
                    return;
                }
            }
        }
    }
}

pub struct H264FrameSlicer<'a> {
    data: &'a [u8],
    pos: usize,
}

/// Where the NAL header begins if an Annex-B start code (3- or 4-byte) sits at
/// `at`, otherwise `None`.
fn start_code_at(data: &[u8], at: usize) -> Option<usize> {
    if data.get(at) != Some(&0) || data.get(at.saturating_add(1)) != Some(&0) {
        return None;
    }
    match data.get(at.saturating_add(2)) {
        Some(&1) => Some(at.saturating_add(3)),
        Some(&0) if data.get(at.saturating_add(3)) == Some(&1) => Some(at.saturating_add(4)),
        _ => None,
    }
}

impl<'a> H264FrameSlicer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn next_nalu_bounds(&self, start: usize) -> Option<(usize, usize, u8)> {
        let mut i = start;
        while i.saturating_add(3) < self.data.len() {
            let Some(header_pos) = start_code_at(self.data, i) else {
                i = i.saturating_add(1);
                continue;
            };
            let nalu_start = i;
            let nalu_type = self.data.get(header_pos).map_or(0, |b| b & 0x1F);

            let mut next = header_pos;
            while next.saturating_add(3) < self.data.len() {
                if start_code_at(self.data, next).is_some() {
                    return Some((nalu_start, next, nalu_type));
                }
                next = next.saturating_add(1);
            }
            return Some((nalu_start, self.data.len(), nalu_type));
        }
        None
    }

    fn is_new_access_unit(&self, nalu_type: u8, nalu_start: usize, _nalu_end: usize) -> bool {
        match nalu_type {
            6..=9 => true,
            1 | 5 => {
                let header_pos = start_code_at(self.data, nalu_start)
                    .unwrap_or_else(|| nalu_start.saturating_add(3));
                self.data.get(header_pos.saturating_add(1)).is_some_and(
                    |first_byte_of_slice_header| (first_byte_of_slice_header & 0x80) != 0,
                )
            }
            _ => false,
        }
    }
}

impl<'a> Iterator for H264FrameSlicer<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }

        let start_pos = self.pos;
        let mut end_pos = self.pos;
        let mut has_vcl = false;

        let mut search_pos = self.pos;
        while let Some((n_start, n_end, n_type)) = self.next_nalu_bounds(search_pos) {
            if has_vcl && self.is_new_access_unit(n_type, n_start, n_end) {
                self.pos = n_start;
                return self.data.get(start_pos..n_start);
            }

            if n_type == 1 || n_type == 5 {
                has_vcl = true;
            }

            end_pos = n_end;
            search_pos = n_end;
        }

        self.pos = self.data.len();
        if end_pos > start_pos {
            self.data.get(start_pos..end_pos)
        } else {
            None
        }
    }
}

/// A synthetic audio source: fixed-size packets at a steady cadence, with a declared loudness.
///
/// Enough to exercise forwarding and speaker selection, which is what the SFU does with audio. It
/// is not an encoder — the payload is filler — because nothing downstream of the selector inspects
/// it, and a real codec would add a dependency for no extra coverage.
///
/// The level is the point. The SFU ranks speakers by RFC 6464 loudness and drops any audio packet
/// that arrives without one, so a source that does not declare a level is a source whose audio
/// never reaches anybody.
pub struct AudioLooper {
    /// Loudness while talking, in negative dBov: 0 is full scale, around -30 is ordinary speech.
    level_dbov: i8,
    /// Whether this source ever talks, as opposed to sitting quietly unmuted.
    talks: bool,
    packet_ms: u64,
    /// Packets of speech before pausing, and of pause before speaking again.
    ///
    /// Real speech is talk spurts separated by silence, and that alternation is the whole reason
    /// the SFU has a speaker selector: it ranks by recent loudness and decays it, so a source at a
    /// constant level exercises the ranking and none of the switching. Roughly 1.8s of speech and
    /// 1.2s of pause at a 20ms cadence.
    spurt_packets: u64,
    pause_packets: u64,
    /// Where in the cycle this source starts.
    ///
    /// Two sources at the same level and the same phase always talk over each other, so the
    /// selector ranks them and never switches. Offsetting one makes them take turns, which is the
    /// only way a plan reaches the slot-stealing path.
    phase_offset: u64,
}

impl AudioLooper {
    /// Someone talking at an ordinary level, in 20ms packets — the Opus default cadence.
    pub fn speaking() -> Self {
        Self {
            level_dbov: -30,
            talks: true,
            packet_ms: 20,
            spurt_packets: 90,
            pause_packets: 60,
            phase_offset: 0,
        }
    }

    /// Present but quiet, as an unmuted listener in a room is: background only, never a spurt.
    pub fn quiet() -> Self {
        Self {
            level_dbov: -70,
            talks: false,
            ..Self::speaking()
        }
    }

    /// Start this source part-way through its speech cycle, so it takes turns with another.
    pub fn with_phase_offset(mut self, packets: u64) -> Self {
        self.phase_offset = packets;
        self
    }

    /// Override the declared loudness, for plans about who the SFU picks.
    pub fn with_level_dbov(mut self, level_dbov: i8) -> Self {
        self.level_dbov = level_dbov;
        self.talks = level_dbov > -60;
        self
    }

    /// Where this source is in its speech cycle, and what that sounds like on the wire.
    ///
    /// Returns the declared level, whether this packet is speech, and how many bytes it carries.
    /// Silence is quiet *and* small: Opus drops to a few bytes per packet when nobody is talking,
    /// so a source that keeps sending full-size frames through its pauses misrepresents both the
    /// loudness the SFU ranks on and the bandwidth it costs.
    fn at(&self, packet: u64) -> (i8, bool, usize) {
        if !self.talks {
            return (self.level_dbov, false, 8);
        }
        let cycle = self.spurt_packets.saturating_add(self.pause_packets).max(1);
        let phase = packet
            .saturating_add(self.phase_offset)
            .checked_rem(cycle)
            .unwrap_or(0);
        if phase >= self.spurt_packets {
            // Between spurts: comfort noise, far below anything the selector will rank.
            return (-70, false, 8);
        }
        // Speech is not flat. A slow swing of a few dB keeps the ranking from being a constant.
        let swing = i8::try_from((phase % 12) / 4)
            .unwrap_or(0)
            .saturating_mul(3);
        (self.level_dbov.saturating_add(swing), true, 160)
    }

    pub async fn run(self, sender: LocalEncoding) {
        const CLOCK_RATE: u64 = 48_000;
        let mid = sender.mid;
        let rid = sender.rid;
        let mut frame_sender = crate::pipeline::FrameSender::new(mid, rid, 1, 0);
        let mut interval = tokio::time::interval(Duration::from_millis(self.packet_ms));
        let mut packets: u64 = 0;

        loop {
            let tick_time = interval.tick().await;
            let ts = packets
                .saturating_mul(CLOCK_RATE)
                .saturating_mul(self.packet_ms)
                .checked_div(1000)
                .unwrap_or(0);
            packets = packets.saturating_add(1);

            let (level_dbov, speech, payload_bytes) = self.at(packets);
            let frame = MediaFrame {
                audio_level: Some(level_dbov),
                voice_activity: Some(speech),
                ts: MediaTime::new(ts, str0m::media::Frequency::FORTY_EIGHT_KHZ),
                data: Arc::from(vec![0u8; payload_bytes].as_slice()),
                capture_time: tick_time,
                abs_capture_time: Some(crate::clock::capture_wallclock()),
                contiguous: true,
                is_keyframe: false,
                target_bitrate_bps: None,
                resolution: None,
                dependency_descriptor: None,
                temporal_layers: None,
            };

            for packet in frame_sender.packetize(&frame) {
                if sender.send(packet).await.is_err() {
                    return;
                }
            }
        }
    }
}
