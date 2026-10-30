use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::decoder::{H264ReferenceDecoder, PixelSample};
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

pub struct NativeVideoFixture {
    temporal_layers: u8,
    encodings: BTreeMap<String, Vec<NativeH264AccessUnit>>,
}

impl std::fmt::Debug for NativeVideoFixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeVideoFixture")
            .field("temporal_layers", &self.temporal_layers)
            .field("rids", &self.encodings.keys())
            .finish_non_exhaustive()
    }
}

impl NativeVideoFixture {
    /// Build application-owned input once, before any scenario is driven.
    pub fn stock_temporal(
        encodings: &[(&str, pulsebeam_rtc_simulation::NativeH264FixtureEncoding)],
        temporal_layers: u8,
        frames: u32,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !encodings.is_empty() && encodings.len() <= 3,
            "native fixture needs one to three explicitly named encodings"
        );
        let world = pulsebeam_rtc_simulation::SimulationWorld::acquire();
        let mut supplied = BTreeMap::new();
        for (rid, encoding) in encodings {
            anyhow::ensure!(
                !rid.is_empty() && !supplied.contains_key(*rid),
                "empty or duplicate fixture RID"
            );
            let units = pulsebeam_rtc_simulation::stock_h264_fixture_for_encoding(
                &world,
                *encoding,
                temporal_layers,
                frames,
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            anyhow::ensure!(
                units.len() == usize::try_from(frames)?,
                "precomputed fixture cannot supply its declared cadence"
            );
            supplied.insert((*rid).to_owned(), units);
        }
        Ok(Self {
            temporal_layers,
            encodings: supplied,
        })
    }

    pub fn temporal_layers(&self) -> u8 {
        self.temporal_layers
    }

    pub fn validate_rids(&self, rids: &[Option<String>]) -> anyhow::Result<()> {
        let declared: std::collections::BTreeSet<_> =
            rids.iter().filter_map(Option::as_ref).collect();
        anyhow::ensure!(
            declared.len() == rids.len()
                && declared.len() == self.encodings.len()
                && declared.iter().all(|rid| self.encodings.contains_key(*rid)),
            "provided native corpora must match every declared RID"
        );
        Ok(())
    }

    pub fn encoding(&self, rid: &str) -> Option<&[NativeH264AccessUnit]> {
        self.encodings.get(rid).map(Vec::as_slice)
    }
}

#[derive(Debug, Default)]
pub struct SolidVideoInput {
    light: AtomicBool,
    quiet: AtomicBool,
}
impl SolidVideoInput {
    pub fn pause(&self) {
        self.quiet.store(true, Ordering::Release);
    }
    pub fn update(&self) {
        self.light.store(true, Ordering::Release);
        self.quiet.store(false, Ordering::Release);
    }
    pub fn expected(&self) -> PixelSample {
        PixelSample::solid(
            if self.light.load(Ordering::Acquire) {
                176
            } else {
                48
            },
            96,
            96,
        )
    }
    fn index(&self) -> usize {
        usize::from(self.light.load(Ordering::Acquire))
    }
    fn is_quiet(&self) -> bool {
        self.quiet.load(Ordering::Acquire)
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
    pub drop_target: Option<(String, Option<String>)>,
    pub discarded_targets: Vec<(String, Option<String>)>,
    pub target_resolution: Option<(u16, u16)>,
    pub one_shot_requests: u64,
}

pub fn video_frames(data: &[u8]) -> Arc<[Arc<[u8]>]> {
    pulsebeam_agent_native::media::H264FrameSlicer::new(data)
        .map(Arc::from)
        .collect()
}

struct ReferencePictures {
    canonical: Arc<[Arc<[u8]>]>,
    samples: Arc<[Option<PixelSample>]>,
    input_sizes: Vec<super::provenance::H264InputSize>,
}

fn reference_pictures(frames: &[H264Frame], first_idr: usize) -> Arc<ReferencePictures> {
    type Cache = BTreeMap<Vec<Arc<[u8]>>, Arc<ReferencePictures>>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    let key: Vec<Arc<[u8]>> = frames
        .iter()
        .map(|frame| Arc::from(frame.payload.as_slice()))
        .collect();
    if let Some(reference) = cache.lock().unwrap().get(&key) {
        return reference.clone();
    }
    let canonical = frames
        .iter()
        .map(|frame| {
            Arc::from(
                super::provenance::inspect(&frame.payload)
                    .expect("fixture coded slices")
                    .1,
            )
        })
        .collect();
    let mut samples = vec![None; frames.len()];
    let mut decoder = H264ReferenceDecoder::new("supplied fixture", "content reference");
    for index in first_idr..frames.len() {
        samples[index] = Some(
            decoder
                .try_decode_observation(&frames[index].payload, None)
                .expect("supplied fixture content must decode")
                .pixel_sample,
        );
    }
    let reference = Arc::new(ReferencePictures {
        canonical,
        samples: samples.into(),
        input_sizes: frames
            .iter()
            .map(|frame| super::provenance::H264InputSize::of(&frame.payload))
            .collect(),
    });
    // Cache immutable corpus work across worlds, never captured-event evidence.
    // An uncached corpus still works; measurement records are never evicted.
    let mut cache = cache.lock().unwrap();
    if cache.len() < 64 {
        cache.insert(key, reference.clone());
    }
    reference
}

pub struct VideoSource {
    frames: Arc<[H264Frame]>,
    first_idr: usize,
    index: usize,
    fps: u32,
    opaque: bool,
    repeat_keyframes: bool,
    solid_picture: Option<Arc<SolidVideoInput>>,
    publisher: Option<String>,
    ledger: Option<Arc<Mutex<super::provenance::SourceLedger>>>,
    references: Option<Arc<ReferencePictures>>,
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
            solid_picture: None,
            publisher: None,
            ledger: None,
            references: None,
            recovery_probe: Arc::default(),
        }
    }
    /// Restart the fixture at its genuine IDR for a fresh native source handle.
    /// Fixture bytes, application picture selection and probes survive rearm.
    pub fn fresh(&self) -> Self {
        Self {
            frames: self.frames.clone(),
            first_idr: self.first_idr,
            index: self.first_idr,
            fps: self.fps,
            opaque: self.opaque,
            repeat_keyframes: self.repeat_keyframes,
            solid_picture: self.solid_picture.clone(),
            publisher: self.publisher.clone(),
            ledger: None,
            references: self.references.clone(),
            recovery_probe: self.recovery_probe.clone(),
        }
    }
    pub fn with_solid_picture(mut self, selection: Arc<SolidVideoInput>) -> Self {
        assert_eq!(self.frames.len(), 2);
        assert!(self.frames.iter().all(|frame| frame.keyframe));
        self.solid_picture = Some(selection);
        self
    }
    pub fn dimensions(&self) -> (u16, u16) {
        let frame = &self.frames[self.first_idr];
        (frame.width, frame.height)
    }
    pub fn identify_as(&mut self, publisher: &str) {
        self.publisher = Some(publisher.to_owned());
    }
    pub fn period(&self) -> Duration {
        // Input capture timestamps have microsecond precision. Bound the
        // largest scheduled interval after that quantization, not received FPS.
        Duration::from_micros(1_000_000u64.div_ceil(u64::from(self.fps)))
    }
    pub fn record_in(&mut self, ledger: Arc<Mutex<super::provenance::SourceLedger>>) {
        self.references
            .get_or_insert_with(|| reference_pictures(&self.frames, self.first_idr));
        self.ledger = Some(ledger);
    }
    pub fn reference_coded_frames(&self) -> impl Iterator<Item = &[u8]> {
        self.references
            .as_ref()
            .expect("registered source codebook")
            .canonical
            .iter()
            .map(AsRef::as_ref)
    }
    fn admit(&self, capture: u64, encoding: &str) {
        if let Some(ledger) = &self.ledger {
            ledger.lock().unwrap().admit(capture, encoding);
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
            let target = (slot.clone(), encoding.clone());
            probe.requests.push(target.clone());
            if probe.drop_target.as_ref() == Some(&target) {
                probe.target_resolution = Some(self.dimensions());
                if probe.discarded_responses == 0 {
                    probe.discarded_responses += 1;
                    probe.discarded_targets.push(target);
                    return;
                }
            }
        }
        self.index = self.first_idr;
    }
    fn frame(
        &mut self,
        timestamp_us: i64,
        capture: u64,
        encoding: &str,
        captured: std::time::Instant,
    ) -> H264Frame {
        if let Some(selection) = &self.solid_picture {
            self.index = selection.index();
        }
        let mut frame = self.frames[self.index].clone();
        frame.timestamp_us = timestamp_us;
        tracing::trace!(target: "pulsebeam_fixture_video", corpus_index = self.index, timestamp_us, width = frame.width, height = frame.height, keyframe = frame.keyframe, "fixture capture");
        if frame.keyframe {
            tracing::debug!(timestamp_us, width = frame.width, height = frame.height, temporal_index = ?frame.temporal_index, bytes = frame.payload.len(), "submitting fixture H264 keyframe to native source");
        }
        if self.opaque {
            frame.payload = opaque_frame(&frame.payload);
        } else if let Some(publisher) = &self.publisher {
            if let Some(ledger) = &self.ledger {
                let event = super::provenance::SourceEvent {
                    source: publisher.clone(),
                    capture,
                    encoding: encoding.to_owned(),
                };
                let payload = super::provenance::stamp_event(&frame.payload, &event);
                let references = self.references.as_ref().expect("registered codebook");
                let supplied = ledger.lock().unwrap().supply(
                    &event,
                    super::provenance::SuppliedPicture {
                        captured,
                        dimensions: (frame.width, frame.height),
                        temporal_index: frame.temporal_index,
                        canonical: references.canonical[self.index].clone(),
                        input_size: references.input_sizes[self.index].with_event(payload.len()),
                        pixel_sample: references.samples[self.index]
                            .expect("actual source content reference"),
                        admitted: false,
                    },
                );
                supplied.expect("source event collection");
                frame.payload = payload;
            } else {
                frame.payload = super::provenance::stamp(&frame.payload, publisher);
            }
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
        let capture_clock = super::native::callback_clock().expect("fixture capture clock anchor");
        let feedback = media.feedback();
        let mut last_keyframes = vec![true; sources.len()];
        let mut sequence = 0u64;
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
            if sources.iter().any(|(_, source)| {
                source
                    .solid_picture
                    .as_ref()
                    .is_some_and(|picture| picture.is_quiet())
            }) {
                continue;
            }
            let timestamp = capture_origin
                .checked_add(i64::try_from(capture.duration_since(started).as_micros()).unwrap())
                .expect("capture timestamp range");
            let captured = capture_clock
                .project(timestamp)
                .expect("fixture capture time projection");
            let mut frames: Vec<_> = sources
                .iter_mut()
                .map(|(encoding, source)| {
                    source.frame(
                        timestamp,
                        sequence,
                        encoding.as_deref().unwrap_or(""),
                        captured,
                    )
                })
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
            for (encoding, source) in &sources {
                source.admit(sequence, encoding.as_deref().unwrap_or(""));
            }
            sequence = sequence.checked_add(1).expect("fixture capture sequence");
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
    pub fn identify_as(&mut self, publisher: &str) {
        self.source.identify_as(publisher);
    }
    pub fn period(&self) -> Duration {
        self.frame_times
            .windows(2)
            .map(|pair| pair[1].saturating_sub(pair[0]))
            .chain([self.profile.loop_idle])
            .max()
            .filter(|period| !period.is_zero())
            .expect("nonzero fixture schedule")
    }
    pub fn record_in(&mut self, ledger: Arc<Mutex<super::provenance::SourceLedger>>) {
        self.source.record_in(ledger);
    }
    pub fn reference_coded_frames(&self) -> impl Iterator<Item = &[u8]> {
        self.source.reference_coded_frames()
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
        let mut sequence = 0u64;
        let capture_origin = media.capture_time_us().await.expect("native capture clock");
        let capture_clock = super::native::callback_clock().expect("fixture capture clock anchor");
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
            let timestamp = capture_origin
                .checked_add(i64::try_from(elapsed.as_micros()).unwrap())
                .expect("capture timestamp range");
            let frame = self.source.frame(
                timestamp,
                sequence,
                "",
                capture_clock
                    .project(timestamp)
                    .expect("VBR capture time projection"),
            );
            last_keyframe = frame.keyframe;
            media
                .submit(InputFrame::H264 { frame })
                .await
                .expect("native VBR frame");
            self.source.admit(sequence, "");
            sequence = sequence.checked_add(1).expect("fixture capture sequence");
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
    fn provided_native_rids_must_cover_the_declaration_exactly_once() {
        // Declaration arithmetic only; real codec bytes are qualified in RTC
        // owner tests and the existing clean native conference.
        let fixture = NativeVideoFixture {
            temporal_layers: 3,
            encodings: BTreeMap::from([("q".into(), Vec::new()), ("h".into(), Vec::new())]),
        };
        assert!(
            fixture
                .validate_rids(&[Some("q".into()), Some("h".into())])
                .is_ok()
        );
        for names in [
            vec![],
            vec![Some("q")],
            vec![Some("q"), Some("q")],
            vec![Some("q"), Some("f")],
            vec![Some("q"), None],
            vec![Some("q"), Some("h"), Some("f")],
        ] {
            let rids: Vec<_> = names
                .into_iter()
                .map(|name| name.map(str::to_owned))
                .collect();
            assert!(fixture.validate_rids(&rids).is_err());
        }
    }

    #[test]
    fn actual_fixture_capture_records_event_bytes_and_decoded_content_before_admission() {
        use super::super::provenance::{self, SourceLedger};
        let mut source =
            VideoSource::from_frames(video_frames(pulsebeam_testdata::RAW_H264_QUARTER_CBR), 30);
        source.identify_as("admission/source-owner");
        let ledger = Arc::new(Mutex::new(SourceLedger::new(
            "admission/source-owner",
            source.period(),
        )));
        source.record_in(ledger.clone());
        let captured = std::time::Instant::now();
        let frame = source.frame(0, 7, "q", captured);
        let (event, canonical) = provenance::inspect_event(&frame.payload).unwrap();
        let event = event.unwrap();
        assert_eq!((event.capture, event.encoding.as_str()), (7, "q"));
        {
            let ledger = ledger.lock().unwrap();
            let supplied = ledger.authenticate(&event, &canonical).unwrap();
            assert_eq!(supplied.captured, captured);
            assert!(!supplied.admitted);
            let actual = super::super::provenance::H264InputSize::of(&frame.payload);
            assert_eq!(supplied.input_size.bytes, actual.bytes);
            assert_eq!(supplied.input_size.nals, actual.nals);
            assert_eq!(supplied.input_size.vcl_bytes, actual.vcl_bytes);
            H264ReferenceDecoder::new("actual", "stamped input")
                .try_decode_observation(&frame.payload, None)
                .unwrap()
                .pixel_sample
                .require_fixture(supplied.pixel_sample)
                .unwrap();
        }
        source.admit(7, "q");
        assert!(
            ledger
                .lock()
                .unwrap()
                .authenticate(&event, &canonical)
                .unwrap()
                .admitted
        );
    }

    #[test]
    fn picture_update_survives_keyframe_feedback_and_rearm() {
        // Input-factory state only, never submitted as RTC media.
        let frames = [0, 1].map(|value| H264Frame {
            payload: vec![value],
            timestamp_us: 0,
            width: 2,
            height: 2,
            keyframe: true,
            temporal_index: None,
            base_layer_sync: false,
        });
        let input = Arc::new(SolidVideoInput::default());
        let mut source = VideoSource::new(frames.into(), 0, 1).with_solid_picture(input.clone());
        let now = std::time::Instant::now();
        assert_eq!(source.frame(0, 0, "q", now).payload, [0]);
        input.pause();
        assert!(input.is_quiet());
        input.update();
        assert!(!input.is_quiet());
        source.request(&MediaSlot::LocalVideo("v0".into()), &Some("q".into()));
        assert_eq!(source.frame(1, 1, "q", now).payload, [1]);
        assert_eq!(source.fresh().frame(2, 2, "q", now).payload, [1]);
    }

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
        source.identify_as("fixture-source");
        source.index = 1;
        let fresh = source.fresh();
        assert_eq!(fresh.index, fresh.first_idr);
        assert_eq!(fresh.publisher.as_deref(), Some("fixture-source"));
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
        assert_eq!(fresh.source.publisher.as_deref(), Some("fixture-source"));
        assert_eq!(fresh.target_bps, 0.0);
        assert!(Arc::ptr_eq(&vbr.frame_times, &fresh.frame_times));
    }

    #[test]
    fn loss_probe_discards_only_the_first_requested_target_not_native_sibling_requests() {
        // State-only values, never submitted as RTC media.
        let frame = H264Frame {
            payload: Vec::new(),
            timestamp_us: 0,
            width: 320,
            height: 180,
            keyframe: true,
            temporal_index: None,
            base_layer_sync: false,
        };
        let probe = Arc::new(Mutex::new(KeyframeRecoveryProbe {
            armed: true,
            drop_target: Some(("v0".into(), Some("q".into()))),
            ..Default::default()
        }));
        let mut source = VideoSource::new(vec![frame.clone(), frame].into(), 0, 30)
            .with_recovery_probe(probe.clone());
        let slot = MediaSlot::LocalVideo("v0".into());
        source.index = 1;
        source.request(&slot, &Some("h".into()));
        assert_eq!(
            source.index, 0,
            "native sibling responses are not discarded"
        );
        assert_eq!(probe.lock().unwrap().discarded_responses, 0);
        source.index = 1;
        source.request(&slot, &Some("q".into()));
        assert_eq!(source.index, 1, "first genuine target response is lost");
        source.request(&slot, &Some("q".into()));
        assert_eq!(source.index, 0, "the target retry supplies a genuine IDR");
        let probe = probe.lock().unwrap();
        assert_eq!(probe.discarded_responses, 1);
        assert_eq!(probe.discarded_targets, [("v0".into(), Some("q".into()))]);
        assert_eq!(probe.target_resolution, Some((320, 180)));
        assert_eq!(
            probe.requests.len(),
            3,
            "all native requests remain visible"
        );
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
