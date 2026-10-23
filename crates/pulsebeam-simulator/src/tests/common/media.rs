use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::decoder::H264ReferenceDecoder;
use pulsebeam_agent_native::agent_core::MediaSlot;
use pulsebeam_agent_native::{EncodingLimits, H264Frame, InputFrame, LocalSource, StreamUpdate};
use pulsebeam_rtc_simulation::NativeH264AccessUnit;

/// Application fixture declaration, not an RTP-engine encoding object.
#[derive(Clone, Debug)]
pub struct FixtureEncoding {
    pub rid: String,
}
impl FixtureEncoding {
    pub fn new(rid: impl Into<String>) -> Self {
        Self { rid: rid.into() }
    }
}

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

#[derive(Default)]
pub struct KeyframeRecoveryProbe {
    pub armed: bool,
    pub request_once: bool,
    pub requests: Vec<(String, Option<String>)>,
    pub discarded_responses: u64,
    pub one_shot_requests: u64,
}

pub fn video_frames(data: &[u8]) -> Arc<[Arc<[u8]>]> {
    pulsebeam_agent_native::media::H264FrameSlicer::new(data)
        .map(Arc::from)
        .collect()
}

pub struct VideoSource {
    frames: Arc<[H264Frame]>,
    first_idr: usize,
    index: usize,
    fps: u32,
    opaque: bool,
    repeat_keyframes: bool,
    recovery_probe: Arc<Mutex<KeyframeRecoveryProbe>>,
}
impl VideoSource {
    pub fn from_frames(frames: Arc<[Arc<[u8]>]>, fps: u32) -> Self {
        let first_idr = frames
            .iter()
            .position(|frame| has_nal(frame, 5))
            .expect("H264 fixture needs an actual keyframe");
        // Read the elementary stream, rather than guessing dimensions from RID.
        let dimensions = H264ReferenceDecoder::new("fixture", "dimensions")
            .try_decode_observation(&frames[first_idr], None)
            .expect("fixture keyframe must decode");
        let frames = frames
            .iter()
            .map(|data| H264Frame {
                payload: data.to_vec(),
                timestamp_us: 0,
                width: u16::try_from(dimensions.width).unwrap(),
                height: u16::try_from(dimensions.height).unwrap(),
                keyframe: has_nal(data, 5),
                temporal_index: None,
                base_layer_sync: false,
            })
            .collect();
        Self::new(frames, first_idr, fps)
    }
    pub fn from_native(frames: &[NativeH264AccessUnit], fps: u32) -> Self {
        let first_idr = frames
            .iter()
            .position(|frame| frame.keyframe)
            .expect("native keyframe");
        let frames = frames
            .iter()
            .map(|frame| H264Frame {
                payload: frame.data.clone(),
                timestamp_us: 0,
                width: u16::try_from(frame.width).unwrap(),
                height: u16::try_from(frame.height).unwrap(),
                keyframe: frame.keyframe,
                temporal_index: frame.temporal_index,
                base_layer_sync: frame.base_layer_sync,
            })
            .collect();
        Self::new(frames, first_idr, fps)
    }
    fn new(frames: Arc<[H264Frame]>, first_idr: usize, fps: u32) -> Self {
        Self {
            frames,
            first_idr,
            index: first_idr,
            fps: fps.max(1),
            opaque: false,
            repeat_keyframes: true,
            recovery_probe: Arc::default(),
        }
    }
    /// Restart the fixture at its genuine IDR for a fresh native source handle.
    /// Only immutable corpus bytes and observation probes are shared.
    pub fn fresh(&self) -> Self {
        Self {
            frames: self.frames.clone(),
            first_idr: self.first_idr,
            index: self.first_idr,
            fps: self.fps,
            opaque: self.opaque,
            repeat_keyframes: self.repeat_keyframes,
            recovery_probe: self.recovery_probe.clone(),
        }
    }
    pub fn dimensions(&self) -> (u16, u16) {
        let frame = &self.frames[self.first_idr];
        (frame.width, frame.height)
    }
    pub fn opaque(mut self) -> Self {
        self.opaque = true;
        self
    }
    pub fn without_natural_keyframe_repeats(mut self) -> Self {
        self.repeat_keyframes = false;
        self
    }
    pub fn with_recovery_probe(mut self, probe: Arc<Mutex<KeyframeRecoveryProbe>>) -> Self {
        self.recovery_probe = probe;
        self
    }
    fn request(&mut self, slot: &MediaSlot, encoding: &Option<String>) {
        let mut probe = self.recovery_probe.lock().unwrap();
        if probe.armed {
            let MediaSlot::LocalVideo(slot) = slot else {
                panic!("video source slot")
            };
            probe.requests.push((slot.clone(), encoding.clone()));
            if probe.requests.len() == 1 {
                probe.discarded_responses += 1;
                return;
            }
        }
        self.index = self.first_idr;
    }
    fn frame(&mut self, timestamp_us: i64) -> H264Frame {
        let mut frame = self.frames[self.index].clone();
        frame.timestamp_us = timestamp_us;
        if frame.keyframe {
            tracing::debug!(timestamp_us, width = frame.width, height = frame.height, temporal_index = ?frame.temporal_index, bytes = frame.payload.len(), "submitting fixture H264 keyframe to native source");
        }
        if self.opaque {
            frame.payload = opaque_frame(&frame.payload);
        }
        self.index = if self.index + 1 < self.frames.len() {
            self.index + 1
        } else if self.repeat_keyframes {
            self.first_idr
        } else {
            self.frames.len() - 1
        };
        frame
    }
    /// One caller capture batch, with actual encoding-specific elementary
    /// streams. Never submit independent per-RID frames to a native sender.
    pub async fn run_batch(mut sources: Vec<(Option<String>, Self)>, media: LocalSource) {
        assert!(!sources.is_empty());
        let fps = sources[0].1.fps;
        assert!(sources.iter().all(|(_, source)| source.fps == fps));
        let capture_origin = media.capture_time_us().await.expect("native capture clock");
        let feedback = media.feedback();
        let mut last_keyframes = vec![true; sources.len()];
        let started = tokio::time::Instant::now();
        let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(fps)));
        loop {
            let capture = interval.tick().await;
            while let Some(update) = feedback.try_next() {
                match update {
                    StreamUpdate::Value(feedback) => {
                        require_encoder_feedback(&feedback, &last_keyframes)
                            .expect("unrecoverable native encoder rejection");
                        let indexed = feedback
                            .encoding_keyframes_requested
                            .iter()
                            .any(|requested| *requested);
                        for (index, (encoding, source)) in sources.iter_mut().enumerate() {
                            if feedback.encoding_keyframes_requested.get(index) == Some(&true)
                                || (feedback.keyframe_requested && !indexed)
                            {
                                source.request(media.slot(), encoding);
                            }
                        }
                    }
                    StreamUpdate::Lagged { .. } => panic!("fixture producer feedback lagged"),
                    StreamUpdate::Closed => return,
                }
            }
            let timestamp = capture_origin
                .checked_add(i64::try_from(capture.duration_since(started).as_micros()).unwrap())
                .expect("capture timestamp range");
            let mut frames: Vec<_> = sources
                .iter_mut()
                .map(|(_, source)| source.frame(timestamp))
                .collect();
            last_keyframes = frames.iter().map(|frame| frame.keyframe).collect();
            let input = if frames.len() == 1 {
                InputFrame::H264 {
                    frame: frames.remove(0),
                }
            } else {
                InputFrame::H264Simulcast { frames }
            };
            media
                .submit(input)
                .await
                .expect("native fixture submission");
        }
    }
}

// Native keyframe feedback is observed only after an encode attempt. An
// already-submitted delta can be rejected for that request. Recover precisely
// that case by honoring the request with genuine fixture IDR bytes; all other
// errors remain fatal and no rejected frame is counted as output.
fn require_encoder_feedback(
    feedback: &pulsebeam_agent_native::ProducerFeedback,
    last_keyframes: &[bool],
) -> anyhow::Result<()> {
    let Some(error) = &feedback.encoder_error else {
        return Ok(());
    };
    let indexed = feedback
        .encoding_keyframes_requested
        .iter()
        .any(|request| *request);
    let requested_delta = if indexed {
        feedback
            .encoding_keyframes_requested
            .iter()
            .enumerate()
            .any(|(index, request)| *request && last_keyframes.get(index) == Some(&false))
    } else {
        feedback.keyframe_requested && last_keyframes.contains(&false)
    };
    anyhow::ensure!(
        *error == pulsebeam_agent_native::VideoEncoderError::InvalidFrame && requested_delta,
        "native encoder rejection: {error:?}; feedback={feedback:?}; last_keyframes={last_keyframes:?}"
    );
    tracing::debug!(
        ?error,
        "native rejected an already-submitted delta for a keyframe request"
    );
    Ok(())
}

pub struct VbrSource {
    source: VideoSource,
    frame_times: Arc<[Duration]>,
    profile: VbrProfile,
    target_bps: f64,
}
impl VbrSource {
    pub fn scheduled(
        frames: Arc<[Arc<[u8]>]>,
        frame_times: Arc<[Duration]>,
        profile: VbrProfile,
    ) -> Self {
        assert!(!frames.is_empty());
        assert_eq!(frames.len(), frame_times.len());
        Self {
            source: VideoSource::from_frames(frames, 30),
            frame_times,
            profile,
            target_bps: 0.0,
        }
    }
    pub fn fresh(&self) -> Self {
        Self {
            source: self.source.fresh(),
            frame_times: self.frame_times.clone(),
            profile: self.profile,
            target_bps: 0.0,
        }
    }
    pub fn dimensions(&self) -> (u16, u16) {
        self.source.dimensions()
    }
    fn active(&self, elapsed: Duration) -> bool {
        let cycle = self.profile.active.saturating_add(self.profile.idle);
        cycle.is_zero() || elapsed.as_nanos() % cycle.as_nanos() < self.profile.active.as_nanos()
    }
    fn step_target(&mut self, active: bool) -> u32 {
        let goal = if active {
            self.profile.declared_target_bps
        } else {
            self.profile.idle_target_bps
        } as f64;
        self.target_bps += (goal - self.target_bps) * self.profile.target_step;
        u32::try_from(saturating_u64(self.target_bps.round()))
            .expect("fixture bitrate must fit native limit")
    }
    pub async fn run(mut self, media: LocalSource) {
        let mut last_keyframe = true;
        let capture_origin = media.capture_time_us().await.expect("native capture clock");
        let feedback = media.feedback();
        let started = tokio::time::Instant::now();
        let loop_duration = self
            .frame_times
            .last()
            .unwrap()
            .saturating_add(self.profile.loop_idle);
        let mut loop_started = started;
        loop {
            let due = loop_started + self.frame_times[self.source.index];
            tokio::time::sleep_until(due).await;
            let now = tokio::time::Instant::now();
            while let Some(update) = feedback.try_next() {
                match update {
                    StreamUpdate::Value(feedback) => {
                        require_encoder_feedback(&feedback, &[last_keyframe])
                            .expect("unrecoverable native VBR encoder rejection");
                        if feedback.keyframe_requested {
                            self.source.index = self.source.first_idr;
                            loop_started = now;
                        }
                    }
                    StreamUpdate::Lagged { .. } => panic!("VBR feedback lagged"),
                    StreamUpdate::Closed => return,
                }
            }
            let elapsed = now.duration_since(started);
            let active = self.active(elapsed);
            let target = self.step_target(active);
            media
                .set_limits(vec![EncodingLimits {
                    max_bitrate_bps: Some(target),
                    max_framerate: None,
                }])
                .await
                .expect("native VBR limits");
            let frame = self.source.frame(
                capture_origin
                    .checked_add(i64::try_from(elapsed.as_micros()).unwrap())
                    .expect("capture timestamp range"),
            );
            last_keyframe = frame.keyframe;
            media
                .submit(InputFrame::H264 { frame })
                .await
                .expect("native VBR frame");
            if self.source.index == self.source.first_idr {
                loop_started += loop_duration;
            }
        }
    }
}

fn saturating_u64(value: f64) -> u64 {
    debug_assert!(value.is_finite());
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "finite value is clamped to the target integer range"
    )]
    {
        value.clamp(0.0, u64::MAX as f64) as u64
    }
}
fn opaque_frame(frame: &[u8]) -> Vec<u8> {
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
    frame
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

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_agent_native::{ProducerFeedback, VideoEncoderError};

    #[test]
    fn fresh_generation_resets_fixture_state_not_shared_observations() {
        // State-only values, never submitted as RTC media.
        let frame = H264Frame {
            payload: Vec::new(),
            timestamp_us: 0,
            width: 2,
            height: 2,
            keyframe: true,
            temporal_index: None,
            base_layer_sync: false,
        };
        let mut source = VideoSource::new(vec![frame.clone(), frame].into(), 0, 30)
            .opaque()
            .without_natural_keyframe_repeats();
        source.index = 1;
        let fresh = source.fresh();
        assert_eq!(fresh.index, fresh.first_idr);
        assert_eq!(source.index, 1);
        assert!(fresh.opaque);
        assert!(!fresh.repeat_keyframes);
        assert!(Arc::ptr_eq(&source.frames, &fresh.frames));
        assert!(Arc::ptr_eq(&source.recovery_probe, &fresh.recovery_probe));
        let vbr = VbrSource {
            source,
            frame_times: vec![Duration::ZERO, Duration::from_millis(33)].into(),
            profile: VbrProfile::screenshare(),
            target_bps: 123.0,
        };
        let fresh = vbr.fresh();
        assert_eq!(fresh.source.index, fresh.source.first_idr);
        assert_eq!(fresh.target_bps, 0.0);
        assert!(Arc::ptr_eq(&vbr.frame_times, &fresh.frame_times));
    }

    #[test]
    fn recover_only_a_rejected_delta_for_an_actual_keyframe_request() {
        let mut feedback = ProducerFeedback {
            keyframe_requested: true,
            encoding_keyframes_requested: vec![true],
            rates: None,
            encoder_error: Some(VideoEncoderError::InvalidFrame),
        };
        assert!(require_encoder_feedback(&feedback, &[false]).is_ok());
        assert!(require_encoder_feedback(&feedback, &[true]).is_err());
        feedback.encoding_keyframes_requested = vec![false, true];
        assert!(require_encoder_feedback(&feedback, &[false, true]).is_err());
        feedback.encoding_keyframes_requested = vec![false];
        feedback.keyframe_requested = false;
        assert!(require_encoder_feedback(&feedback, &[false]).is_err());
        feedback.keyframe_requested = true;
        feedback.encoder_error = Some(VideoEncoderError::InvalidConfiguration);
        assert!(require_encoder_feedback(&feedback, &[false]).is_err());
    }
}
