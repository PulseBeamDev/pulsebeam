use std::sync::Arc;
use std::time::Duration;

use pulsebeam_agent_native::agent_core::MediaSlot;
use pulsebeam_agent_native::{AgentEvent, LocalMedia, MediaFrame, MediaTime};
use tokio::sync::broadcast;

#[derive(Clone, Copy, Debug)]
pub struct VbrProfile {
    pub active: Duration,
    pub idle: Duration,
    pub loop_idle: Duration,
    pub declared_target_bps: u64,
    pub idle_target_bps: u64,
    pub target_step: f64,
}

impl VbrProfile {
    pub fn screenshare() -> Self {
        Self {
            active: Duration::from_secs(4),
            idle: Duration::from_secs(20),
            loop_idle: Duration::from_millis(500),
            declared_target_bps: 2_500_000,
            idle_target_bps: 1_460_000,
            target_step: 0.25,
        }
    }

    pub fn screenshare_detail() -> Self {
        Self::screenshare()
    }

    pub fn screenshare_static() -> Self {
        Self {
            loop_idle: Duration::from_secs(20),
            ..Self::screenshare()
        }
    }
}

pub struct VideoSource {
    frames: Vec<Arc<[u8]>>,
    first_idr: usize,
    fps: u32,
    opaque: bool,
    repeat_keyframes: bool,
}

impl VideoSource {
    pub fn new(data: &[u8], fps: u32) -> Self {
        let frames: Vec<Arc<[u8]>> = pulsebeam_agent_native::media::H264FrameSlicer::new(data)
            .map(Arc::from)
            .collect();
        debug_assert!(!frames.is_empty());
        let first_idr = frames
            .iter()
            .position(|frame| has_nal(frame, 5))
            .unwrap_or(0);
        Self {
            frames,
            first_idr,
            fps,
            opaque: false,
            repeat_keyframes: true,
        }
    }

    pub fn opaque(mut self) -> Self {
        self.opaque = true;
        self
    }

    pub fn without_natural_keyframe_repeats(mut self) -> Self {
        self.repeat_keyframes = false;
        self
    }

    pub async fn run(
        self,
        media: LocalMedia,
        encoding: Option<String>,
        mut events: broadcast::Receiver<AgentEvent>,
    ) {
        let mut index = 0usize;
        let mut frame_count = 0u64;
        let mut interval =
            tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(self.fps.max(1))));
        loop {
            let capture_time = interval.tick().await;
            while let Ok(event) = events.try_recv() {
                if keyframe_matches(&event, media.slot(), encoding.as_deref()) {
                    index = self.first_idr;
                }
            }
            debug_assert!(index < self.frames.len());
            let source = self.frames.get(index).cloned().unwrap_or_default();
            let data = if self.opaque {
                opaque_frame(&source)
            } else {
                source
            };
            let frame = MediaFrame {
                audio_level: None,
                voice_activity: None,
                ts: MediaTime::from_90khz(
                    frame_count.saturating_mul(90_000) / u64::from(self.fps.max(1)),
                ),
                data,
                capture_time,
                abs_capture_time: Some(pulsebeam_agent_native::clock::capture_wallclock()),
                contiguous: true,
                is_keyframe: index == self.first_idr,
                target_bitrate_bps: None,
                resolution: None,
                dependency_descriptor: None,
                temporal_layers: None,
            };
            let _ = media.send_encoding(encoding.clone(), frame).await;
            frame_count = frame_count.saturating_add(1);
            index = if index.saturating_add(1) < self.frames.len() {
                index.saturating_add(1)
            } else if self.repeat_keyframes {
                0
            } else {
                self.frames.len().saturating_sub(1)
            };
        }
    }
}

pub struct VbrSource {
    frames: Vec<Arc<[u8]>>,
    frame_times: Vec<Duration>,
    profile: VbrProfile,
    target_bps: f64,
}

impl VbrSource {
    pub fn scheduled(data: &[u8], timing: &str, profile: VbrProfile) -> Self {
        let frames = pulsebeam_agent_native::media::H264FrameSlicer::new(data)
            .map(Arc::from)
            .collect::<Vec<_>>();
        let frame_times = timing
            .lines()
            .map(|line| Duration::from_micros(line.parse().unwrap_or_default()))
            .collect::<Vec<_>>();
        debug_assert!(!frames.is_empty());
        debug_assert_eq!(frames.len(), frame_times.len());
        Self {
            frames,
            frame_times,
            profile,
            target_bps: 0.0,
        }
    }

    fn active(&self, elapsed: Duration) -> bool {
        let cycle = self.profile.active.saturating_add(self.profile.idle);
        cycle.is_zero() || elapsed.as_nanos() % cycle.as_nanos() < self.profile.active.as_nanos()
    }

    fn step_target(&mut self, active: bool) -> u64 {
        let goal = if active {
            self.profile.declared_target_bps
        } else {
            self.profile.idle_target_bps
        } as f64;
        self.target_bps += (goal - self.target_bps) * self.profile.target_step;
        saturating_u64(self.target_bps.round())
    }

    pub async fn run(
        mut self,
        media: LocalMedia,
        encoding: Option<String>,
        mut events: broadcast::Receiver<AgentEvent>,
    ) {
        let started = tokio::time::Instant::now();
        let loop_duration = self
            .frame_times
            .last()
            .copied()
            .unwrap_or_default()
            .saturating_add(self.profile.loop_idle);
        let mut loop_started = started;
        let mut index = 0usize;
        loop {
            let due = loop_started
                .checked_add(self.frame_times.get(index).copied().unwrap_or_default())
                .unwrap_or(loop_started);
            tokio::time::sleep_until(due).await;
            let now = tokio::time::Instant::now();
            while let Ok(event) = events.try_recv() {
                if keyframe_matches(&event, media.slot(), encoding.as_deref()) {
                    index = 0;
                    loop_started = now;
                }
            }
            let elapsed = now.duration_since(started);
            let active = self.active(elapsed);
            let frame = MediaFrame {
                audio_level: None,
                voice_activity: None,
                ts: MediaTime::from_90khz(saturating_u64(elapsed.as_secs_f64() * 90_000.0)),
                data: self.frames.get(index).cloned().unwrap_or_default(),
                capture_time: now,
                abs_capture_time: Some(pulsebeam_agent_native::clock::capture_wallclock()),
                contiguous: true,
                is_keyframe: index == 0,
                target_bitrate_bps: Some(self.step_target(active)),
                resolution: None,
                dependency_descriptor: None,
                temporal_layers: None,
            };
            let _ = media.send_encoding(encoding.clone(), frame).await;
            index = index.saturating_add(1);
            if index == self.frame_times.len() {
                index = 0;
                loop_started = loop_started
                    .checked_add(loop_duration)
                    .unwrap_or(loop_started);
            }
        }
    }
}

fn keyframe_matches(event: &AgentEvent, slot: &MediaSlot, encoding: Option<&str>) -> bool {
    let AgentEvent::KeyframeRequested {
        slot: requested_slot,
        encoding: requested_encoding,
    } = event
    else {
        return false;
    };
    let MediaSlot::LocalVideo(slot) = slot else {
        return false;
    };
    requested_slot == slot && requested_encoding.as_deref() == encoding
}

fn saturating_u64(value: f64) -> u64 {
    debug_assert!(value.is_finite());
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the finite value is clamped to the target integer range"
    )]
    {
        value.clamp(0.0, u64::MAX as f64) as u64
    }
}

fn opaque_frame(frame: &[u8]) -> Arc<[u8]> {
    let mut frame = frame.to_vec();
    let mut index = 0usize;
    while index.saturating_add(3) < frame.len() {
        if frame.get(index..index.saturating_add(3)) == Some(&[0, 0, 1]) {
            if let Some(header) = frame.get_mut(index.saturating_add(3)) {
                *header = (*header & 0xe0) | 1;
            }
            index = index.saturating_add(4);
        } else {
            index = index.saturating_add(1);
        }
    }
    frame.into()
}

fn has_nal(frame: &[u8], wanted: u8) -> bool {
    let mut index = 0usize;
    while index.saturating_add(3) < frame.len() {
        let header = if frame.get(index..index.saturating_add(4)) == Some(&[0, 0, 0, 1]) {
            index.saturating_add(4)
        } else if frame.get(index..index.saturating_add(3)) == Some(&[0, 0, 1]) {
            index.saturating_add(3)
        } else {
            index = index.saturating_add(1);
            continue;
        };
        if frame.get(header).is_some_and(|byte| byte & 0x1f == wanted) {
            return true;
        }
        index = header;
    }
    false
}
