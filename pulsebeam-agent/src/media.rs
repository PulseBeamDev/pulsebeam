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
        }
    }
}

/// An [`H264Looper`] whose frame rate follows a [`VbrProfile`], approximating a VBR encoder.
///
/// Frame rate is the lever rather than frame size because it is what a real encoder actually does
/// with static content, and because it keeps every emitted frame a valid, decodable H.264 access
/// unit - the receiver-side QoE checks in the simulator would reject synthesised partial frames.
pub struct VbrLooper {
    asset: Arc<SharedH264Asset>,
    index: usize,
    profile: VbrProfile,
}

impl VbrLooper {
    pub fn new(data: &[u8], profile: VbrProfile) -> Self {
        Self {
            asset: Arc::new(SharedH264Asset::new(data)),
            index: 0,
            profile,
        }
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
        let mut next_frame_at = start;

        loop {
            tokio::time::sleep_until(next_frame_at).await;
            let now = tokio::time::Instant::now();
            let elapsed = now.duration_since(start);

            let fps = if self.is_active(elapsed) {
                self.profile.active_fps
            } else {
                self.profile.idle_fps
            }
            .max(1);
            next_frame_at = now + Duration::from_secs_f64(1.0 / fps as f64);

            if sender.keyframe_rx.is_requested() {
                tracing::debug!(?mid, ?rid, first_idr = self.asset.first_idr, "keyframe reset");
                self.index = self.asset.first_idr;
            }

            // Derive the timestamp from wall-clock elapsed rather than a frame counter: the frame
            // rate changes between phases, so a counter would drift against real time and the
            // receiver would see the media clock stall during quiet stretches.
            let frame = MediaFrame {
                ts: MediaTime::from_90khz((elapsed.as_secs_f64() * clock_rate) as u64),
                data: self.next(),
                capture_time: now,
                abs_capture_time: Some(crate::clock::capture_wallclock()),
                contiguous: true,
                is_keyframe: false,
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
