use std::{sync::Arc, time::Duration};

use str0m::media::MediaTime;
use tokio::sync::watch;

use crate::{MediaFrame, agent::LocalEncoding};

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
        while i + 3 < frame.len() {
            if frame[i] == 0 && frame[i + 1] == 0 {
                let header_pos = if frame[i + 2] == 1 {
                    i + 3
                } else if i + 4 < frame.len() && frame[i + 2] == 0 && frame[i + 3] == 1 {
                    i + 4
                } else {
                    i += 1;
                    continue;
                };
                if header_pos < frame.len() && (frame[header_pos] & 0x1F) == 5 {
                    return true;
                }
                i = header_pos;
            } else {
                i += 1;
            }
        }
        false
    }
}

pub struct H264Looper {
    asset: Arc<SharedH264Asset>,
    index: usize,
    fps: u32,
}

impl H264Looper {
    pub fn new(data: &[u8], fps: u32) -> Self {
        let asset = Arc::new(SharedH264Asset::new(data));
        Self {
            asset,
            index: 0,
            fps,
        }
    }

    pub fn new_shared(asset: Arc<SharedH264Asset>, fps: u32) -> Self {
        Self {
            asset,
            index: 0,
            fps,
        }
    }

    fn next(&mut self) -> Arc<[u8]> {
        let frame = &self.asset.frames[self.index];
        self.index = (self.index + 1) % self.asset.frames.len();
        frame.clone()
    }

    pub async fn run(mut self, mut sender: LocalEncoding) {
        let clock_rate = 90_000u64;
        let frame_interval = Duration::from_secs_f64(1.0 / self.fps as f64);
        let mid = sender.mid;
        let rid = sender.rid;

        let mut interval = tokio::time::interval(frame_interval);
        let mut frame_count: u64 = 0;

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

            let frame_data = self.next();
            let next_ts = (frame_count * clock_rate) / self.fps as u64;

            let frame = MediaFrame {
                ts: MediaTime::from_90khz(next_ts),
                data: frame_data,
                capture_time: tick_time,
                abs_capture_time: Some(crate::clock::capture_wallclock()),
                contiguous: true,
                is_keyframe: false,
                // A constant-rate source has nothing to declare beyond what it is sending, so
                // leave VLA off and let the SFU measure. That is also the pre-VLA path, which is
                // worth still exercising: not every sender emits the extension.
                target_bitrate_bps: None,
                resolution: None,
            };

            if sender.send(frame).await.is_err() {
                break;
            }
            frame_count += 1;
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
            // 15fps cap, per the client's detail preset.
            active_fps: 15,
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
}

impl VbrLooper {
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
        }
    }

    pub fn new_scheduled(data: &[u8], timing: &str, profile: VbrProfile) -> Self {
        let mut looper = Self::new(data, profile);
        let frame_times: Vec<Duration> = timing
            .lines()
            .map(|line| Duration::from_micros(line.parse().expect("valid frame timestamp")))
            .collect();
        debug_assert!(!frame_times.is_empty());
        debug_assert_eq!(looper.asset.frames.len(), frame_times.len());
        debug_assert!(frame_times.windows(2).all(|pair| pair[0] < pair[1]));
        looper.frame_times = Some(frame_times);
        looper
    }

    /// Next frame for the static phase: the smallest frames the asset has. Falls back to the
    /// normal sequence when the asset has nothing small enough.
    fn next_small(&mut self) -> Arc<[u8]> {
        if self.small.is_empty() {
            return self.next();
        }
        let idx = self.small[self.small_index % self.small.len()];
        self.small_index = self.small_index.wrapping_add(1);
        self.asset.frames[idx].clone()
    }

    fn next(&mut self) -> Arc<[u8]> {
        let frame = &self.asset.frames[self.index];
        self.index = (self.index + 1) % self.asset.frames.len();
        frame.clone()
    }

    /// Whether we are in an active burst at `elapsed` into the run.
    fn is_active(&self, elapsed: Duration) -> bool {
        let cycle = self.profile.active + self.profile.idle;
        if cycle.is_zero() {
            return true;
        }
        let phase = (elapsed.as_nanos() % cycle.as_nanos()) as u64;
        Duration::from_nanos(phase) < self.profile.active
    }

    pub async fn run(mut self, mut sender: LocalEncoding) {
        let clock_rate = 90_000f64;
        let mid = sender.mid;
        let rid = sender.rid;

        let start = tokio::time::Instant::now();
        if let Some(frame_times) = self.frame_times.take() {
            debug_assert_eq!(self.asset.frames.len(), frame_times.len());
            let loop_duration = frame_times
                .last()
                .copied()
                .expect("non-empty frame schedule")
                + self.profile.loop_idle;
            let mut loop_start = start;
            let mut index = 0usize;
            loop {
                debug_assert!(index < frame_times.len());
                tokio::time::sleep_until(loop_start + frame_times[index]).await;
                let now = tokio::time::Instant::now();
                if sender.keyframe_rx.is_requested() {
                    index = self.asset.first_idr;
                    debug_assert_eq!(index, 0, "scheduled fixture must begin with its first IDR");
                    loop_start = now;
                }
                debug_assert!(index < self.asset.frames.len());
                let frame = MediaFrame {
                    ts: MediaTime::from_90khz(
                        (now.duration_since(start).as_secs_f64() * clock_rate) as u64,
                    ),
                    data: self.asset.frames[index].clone(),
                    capture_time: now,
                    abs_capture_time: Some(crate::clock::capture_wallclock()),
                    contiguous: true,
                    is_keyframe: false,
                    target_bitrate_bps: Some(if self.is_active(now.duration_since(start)) {
                        self.profile.declared_target_bps
                    } else {
                        self.profile.idle_target_bps
                    }),
                    resolution: None,
                };
                if sender.send(frame).await.is_err() {
                    return;
                }
                index += 1;
                if index == frame_times.len() {
                    index = 0;
                    loop_start += loop_duration;
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
            next_frame_at = now + Duration::from_secs_f64(1.0 / fps as f64);

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
            let frame = MediaFrame {
                ts: MediaTime::from_90khz((elapsed.as_secs_f64() * clock_rate) as u64),
                data: if active {
                    self.next()
                } else {
                    self.next_small()
                },
                capture_time: now,
                abs_capture_time: Some(crate::clock::capture_wallclock()),
                contiguous: true,
                is_keyframe: false,
                target_bitrate_bps: Some(if active {
                    self.profile.declared_target_bps
                } else {
                    self.profile.idle_target_bps
                }),
                resolution: None,
            };

            if sender.send(frame).await.is_err() {
                break;
            }
        }
    }
}

pub struct H264FrameSlicer<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> H264FrameSlicer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn next_nalu_bounds(&self, start: usize) -> Option<(usize, usize, u8)> {
        let mut i = start;
        while i + 3 < self.data.len() {
            if self.data[i] == 0
                && self.data[i + 1] == 0
                && (self.data[i + 2] == 1 || (self.data[i + 2] == 0 && self.data[i + 3] == 1))
            {
                let nalu_start = i;
                let header_pos = if self.data[i + 2] == 1 { i + 3 } else { i + 4 };
                let nalu_type = self.data[header_pos] & 0x1F;

                let mut next = header_pos;
                while next + 3 < self.data.len() {
                    if self.data[next] == 0
                        && self.data[next + 1] == 0
                        && (self.data[next + 2] == 1
                            || (self.data[next + 2] == 0 && self.data[next + 3] == 1))
                    {
                        return Some((nalu_start, next, nalu_type));
                    }
                    next += 1;
                }
                return Some((nalu_start, self.data.len(), nalu_type));
            }
            i += 1;
        }
        None
    }

    fn is_new_access_unit(&self, nalu_type: u8, nalu_start: usize, _nalu_end: usize) -> bool {
        match nalu_type {
            6..=9 => true,
            1 | 5 => {
                let header_pos = if self.data[nalu_start + 2] == 1 {
                    nalu_start + 3
                } else {
                    nalu_start + 4
                };

                if self.data.len() > header_pos + 1 {
                    let first_byte_of_slice_header = self.data[header_pos + 1];
                    return (first_byte_of_slice_header & 0x80) != 0;
                }
                false
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
                return Some(&self.data[start_pos..n_start]);
            }

            if n_type == 1 || n_type == 5 {
                has_vcl = true;
            }

            end_pos = n_end;
            search_pos = n_end;
        }

        self.pos = self.data.len();
        if end_pos > start_pos {
            Some(&self.data[start_pos..end_pos])
        } else {
            None
        }
    }
}
