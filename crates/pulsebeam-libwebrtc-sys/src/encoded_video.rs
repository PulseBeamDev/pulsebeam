//! Direct H.264 Annex-B input using an internal encoder adapter. No caller
//! codec implementation is required for sending encoded access units.

use std::{
    cell::Cell,
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};

use crate::{
    CodecError, CodecSupport, EncodedImageCallback, EncodedVideoCodec, EncodedVideoFrame,
    EncodedVideoMetadata, PeerConnectionFactory, PeerError, PeerErrorKind, SdpVideoFormat,
    VideoCodecFormat, VideoEncoder, VideoEncoderFactory, VideoEncoderFactoryHandle,
    VideoEncoderInfo, VideoEncoderSettings, VideoFrame, VideoFrameType, VideoRateControl,
    VideoResolution, VideoSource, VideoTrack, ffi,
};

const MAX_PENDING: usize = 16;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

// Opt-in boundary tracing, not an acceptance counter or frame identity. Never
// log payload bytes or let a diagnostic write fail a native codec callback.
fn trace_input(event: &str, details: std::fmt::Arguments<'_>) {
    if std::env::var_os("PULSEBEAM_WEBRTC_ENCODED_DIAGNOSTICS").is_some() {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr().lock(),
            "[webrtc-encoded-input] {event} {details}"
        );
    }
}

/// One encoded H.264 access unit, represented as start-code-delimited Annex-B
/// NAL units. Timestamps use caller monotonic microseconds; WebRTC assigns the
/// wire RTP timestamp from its capture clock, rather than trusting an input RTP
/// clock. Each source is a separate stream.
#[derive(Clone, Debug)]
pub struct H264AccessUnit {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub timestamp_us: i64,
    pub key_frame: bool,
    pub qp: Option<u8>,
}

/// One complete compressed access unit. Its codec metadata must match the
/// format selected when constructing the input factory. Timestamps are
/// monotonic capture microseconds, not caller-controlled wire RTP timestamps.
#[derive(Clone, Debug)]
pub struct EncodedVideoAccessUnit {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub timestamp_us: i64,
    pub key_frame: bool,
    pub qp: Option<u8>,
    pub metadata: EncodedVideoMetadata,
}

#[derive(Default)]
struct SourceFeedback {
    retired: AtomicBool,
    submitted: AtomicU64,
    keyframe_requested: AtomicBool,
    encoding_keyframes: AtomicU8,
    rates: Mutex<Option<VideoRateControl>>,
    encoder_error: Mutex<Option<CodecError>>,
    readiness: Option<Arc<crate::readiness::Readiness>>,
}

impl SourceFeedback {
    fn notify(&self) {
        if let Some(readiness) = &self.readiness {
            // Executor notification never dispatches engine work. Do not let a
            // custom waker unwind through a codec callback or fail the encoder.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| readiness.notify()));
        }
    }

    fn retire(&self) {
        let _rates = self.rates.lock().unwrap_or_else(|error| error.into_inner());
        self.retired.store(true, Ordering::Release);
    }
    fn set_rates(&self, rates: VideoRateControl) {
        let changed = {
            let mut previous = self.rates.lock().unwrap_or_else(|error| error.into_inner());
            if self.retired.load(Ordering::Acquire) {
                return;
            }
            let changed = *previous != Some(rates);
            *previous = Some(rates);
            changed
        };
        if changed {
            self.notify();
        }
    }

    fn request_keyframe(&self) {
        if self.retired.load(Ordering::Acquire) {
            return;
        }
        if !self.keyframe_requested.swap(true, Ordering::AcqRel) {
            self.notify();
        }
    }

    fn request_encoding_keyframe(&self, index: usize) {
        if self.retired.load(Ordering::Acquire) {
            return;
        }
        let bit = 1 << index;
        let changed = self.encoding_keyframes.fetch_or(bit, Ordering::AcqRel) & bit == 0;
        self.request_keyframe();
        if changed {
            self.notify();
        }
    }

    fn record_error(&self, error: CodecError) {
        *self
            .encoder_error
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(error);
        self.notify();
    }
}

struct Pending {
    source_id: u64,
    sequence: u64,
    dropped: Arc<AtomicU64>,
    feedback: Arc<SourceFeedback>,
    frames: Vec<EncodedVideoAccessUnit>,
}

impl Pending {
    fn decodable_units(&self, previous: [bool; 3], selected: &[bool]) -> Vec<bool> {
        self.frames
            .iter()
            .enumerate()
            .map(|(index, unit)| {
                let decodable = selected[index] && (unit.key_frame || previous[index]);
                if selected[index] && !decodable {
                    self.feedback.request_encoding_keyframe(index);
                }
                decodable
            })
            .collect()
    }

    fn check_keyframe_requests(
        &self,
        frame_types: &[VideoFrameType],
        selected: &[bool],
    ) -> Result<(), CodecError> {
        let mut missing_keyframe = false;
        for (index, unit) in self
            .frames
            .iter()
            .enumerate()
            .filter(|(index, _)| selected[*index])
        {
            if frame_types.get(index) == Some(&VideoFrameType::Key) {
                // Preserve every native request before rejecting the capture
                // batch. Returning early strands later rungs on delta input.
                self.feedback.request_encoding_keyframe(index);
                missing_keyframe |= !unit.key_frame;
            }
        }
        if missing_keyframe {
            Err(self.reject(CodecError::InvalidFrame))
        } else {
            Ok(())
        }
    }

    fn reject(&self, error: CodecError) -> CodecError {
        trace_input(
            "reject",
            format_args!("source={:p} error={error:?}", Arc::as_ptr(&self.feedback)),
        );
        self.dropped
            .fetch_add(self.frames.len() as u64, Ordering::Relaxed);
        self.feedback.record_error(error);
        error
    }
}

#[derive(Default)]
struct Broker {
    next_token: i64,
    next_source: u64,
    bytes: usize,
    pending: HashMap<i64, Pending>,
    order: VecDeque<i64>,
}

impl Broker {
    fn insert(
        &mut self,
        source_id: u64,
        dropped: Arc<AtomicU64>,
        feedback: Arc<SourceFeedback>,
        frames: Vec<EncodedVideoAccessUnit>,
    ) -> Result<i64, CodecError> {
        let size: usize = frames.iter().map(|frame| frame.data.len()).sum();
        if size > MAX_PENDING_BYTES || self.next_token == i64::MAX {
            return Err(CodecError::InvalidFrame);
        }
        while self.pending.len() >= MAX_PENDING || self.bytes > MAX_PENDING_BYTES - size {
            let Some(old) = self.order.pop_front() else {
                return Err(CodecError::InvalidFrame);
            };
            if let Some(previous) = self.pending.remove(&old) {
                trace_input(
                    "evict",
                    format_args!(
                        "source={:p} token={old} units={}",
                        Arc::as_ptr(&previous.feedback),
                        previous.frames.len()
                    ),
                );
                self.bytes -= previous
                    .frames
                    .iter()
                    .map(|frame| frame.data.len())
                    .sum::<usize>();
                previous
                    .dropped
                    .fetch_add(previous.frames.len() as u64, Ordering::Relaxed);
            }
        }
        let sequence = feedback
            .submitted
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| CodecError::InvalidFrame)?;
        self.next_token += 1;
        let token = self.next_token;
        self.bytes += size;
        self.order.push_back(token);
        for (index, frame) in frames.iter().enumerate() {
            trace_input(
                "insert",
                format_args!(
                    "source={:p} token={token} index={index} capture_us={} key={} bytes={} width={} height={}",
                    Arc::as_ptr(&feedback),
                    frame.timestamp_us,
                    frame.key_frame,
                    frame.data.len(),
                    frame.width,
                    frame.height
                ),
            );
        }
        self.pending.insert(
            token,
            Pending {
                source_id,
                sequence,
                dropped,
                feedback,
                frames,
            },
        );
        Ok(token)
    }

    fn take(&mut self, token: i64) -> Option<Pending> {
        let pending = self.pending.remove(&token)?;
        self.bytes -= pending
            .frames
            .iter()
            .map(|frame| frame.data.len())
            .sum::<usize>();
        // Older tokens lost upstream are pruned by insert's bounded FIFO.
        self.order.retain(|queued| *queued != token);
        Some(pending)
    }

    fn remove_source(&mut self, id: u64) {
        let tokens: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(&token, pending)| (pending.source_id == id).then_some(token))
            .collect();
        for token in tokens {
            let _ = self.take(token);
        }
    }
}

/// Codec setup for encoded video sending. Supply `encoder_factory()` to
/// the peer factory builder, then create sources using that same factory.
/// `new()` advertises constrained-baseline H.264, level 3.1, packetization
/// mode 1. `new_for_format` accepts VP8, VP9, AV1 or H265, without supplying
/// a decoder. The caller must provide a frame matching its advertised format.
/// A source's declared native initialization profile, never inferred from
/// capture geometry, SSRC, queue order or the first encoded access unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H264SourceProfile {
    L1T1,
    L1T3,
    SimulcastL1T1,
    SimulcastL1T3,
}

#[derive(Clone)]
pub struct EncodedH264Input {
    broker: Arc<Mutex<Broker>>,
    encoder: VideoEncoderFactoryHandle,
    format: VideoCodecFormat,
    codec: crate::video::DirectEncodedVideo,
    temporal_fps: Option<[u8; 3]>,
    simulcast: bool,
}

impl EncodedH264Input {
    pub fn new() -> Result<Self, CodecError> {
        Self::new_for_format(format())
    }

    /// Advertise a single encoded-input format without requiring a bundled
    /// software encoder. Format parameters must agree with the compressed input.
    pub fn new_for_format(format: VideoCodecFormat) -> Result<Self, CodecError> {
        Self::build(format, None, false)
    }

    /// Declare single-encoding H264 L1T3 input. Fractions are the producer's
    /// cumulative temporal frame-rate capabilities, on native's 0..=255 scale.
    /// They are passed unchanged, not used to build a GOP or calculate VLA.
    /// Select L1T3 on the sender before submitting explicitly indexed units.
    pub fn new_l1t3(fps: [u8; 3]) -> Result<Self, CodecError> {
        Self::new_l1t3_for_format(format(), fps)
    }

    pub(crate) fn new_l1t3_for_format(
        format: VideoCodecFormat,
        fps: [u8; 3],
    ) -> Result<Self, CodecError> {
        if format.name != "H264" || !same_codec_format(&format, &self::format()) {
            return Err(CodecError::UnsupportedFormat);
        }
        if format
            .scalability_modes
            .iter()
            .any(|mode| !matches!(mode.as_str(), "L1T1" | "L1T3"))
        {
            return Err(CodecError::UnsupportedFormat);
        }
        if fps[0] == 0 || fps[0] > fps[1] || fps[1] > fps[2] || fps[2] != 255 {
            return Err(CodecError::InvalidConfiguration);
        }
        let format = format
            .with_scalability_mode(crate::VideoScalabilityMode::parse("L1T1")?)
            .with_scalability_mode(crate::VideoScalabilityMode::parse("L1T3")?);
        Self::build(format, Some(fps), false)
    }

    /// One installed factory supporting single-encoding and three-rung sources,
    /// each with L1T1 or L1T3. Every T3 encoding shares these declared cumulative
    /// FPS capabilities. Select the source profile explicitly before input.
    pub fn new_h264_profiles(fps: [u8; 3]) -> Result<Self, CodecError> {
        Self::new_h264_profiles_for_format(format(), fps)
    }
    pub(crate) fn new_h264_profiles_for_format(
        format: VideoCodecFormat,
        fps: [u8; 3],
    ) -> Result<Self, CodecError> {
        let input = Self::new_l1t3_for_format(format, fps)?;
        Self::build(input.format, Some(fps), true)
    }
    /// Direct three-rung H264 L1T1 simulcast. Submit ordered, distinct rung batches.
    /// Native publication is not atomic. Stock receivers do not receive all
    /// simulcast rungs concurrently; native initialization must retain all three.
    pub fn new_simulcast() -> Result<Self, CodecError> {
        Self::new_simulcast_for_format(format())
    }

    pub(crate) fn new_simulcast_for_format(format: VideoCodecFormat) -> Result<Self, CodecError> {
        if format.name != "H264" || !format.scalability_modes.is_empty() {
            return Err(CodecError::UnsupportedFormat);
        }
        Self::build(format, None, true)
    }

    fn build(
        mut format: VideoCodecFormat,
        temporal_fps: Option<[u8; 3]>,
        simulcast: bool,
    ) -> Result<Self, CodecError> {
        if format.name == "VP9" && !format.parameters.iter().any(|p| p.key == "profile-id") {
            format = format.with_parameter("profile-id", "0");
        }
        if format.name == "AV1" && format.parameters.is_empty() {
            format = format
                .with_parameter("level-idx", "5")
                .with_parameter("profile", "0")
                .with_parameter("tier", "0");
        }
        if format.name == "H265" && format.parameters.is_empty() {
            format = format
                .with_parameter("level-id", "93")
                .with_parameter("tx-mode", "SRST");
        }
        if !matches!(
            format.name.as_str(),
            "H264" | "VP8" | "VP9" | "AV1" | "H265"
        ) || (format.name == "H264"
            && (!same_codec_format(&format, &self::format())
                || (temporal_fps.is_none() && !format.scalability_modes.is_empty())))
        {
            return Err(CodecError::UnsupportedFormat);
        }
        use crate::video::DirectEncodedVideo;
        let codec = match format.name.as_str() {
            "H264" if simulcast => DirectEncodedVideo::H264Simulcast,
            "H264" if temporal_fps.is_some() => DirectEncodedVideo::H264L1T3,
            "H264" => DirectEncodedVideo::H264,
            "VP8" => DirectEncodedVideo::Vp8,
            "VP9" => DirectEncodedVideo::Vp9,
            "AV1" => DirectEncodedVideo::Av1,
            "H265" => DirectEncodedVideo::H265,
            _ => unreachable!("validated encoded format"),
        };
        let broker = Arc::new(Mutex::new(Broker::default()));
        let encoder = VideoEncoderFactoryHandle::new_direct_encoded(
            InputFactory {
                broker: broker.clone(),
                format: format.clone(),
                temporal_fps,
                simulcast,
            },
            codec,
        )?;
        Ok(Self {
            broker,
            encoder,
            format,
            codec,
            temporal_fps,
            simulcast,
        })
    }

    pub fn encoder_factory(&self) -> VideoEncoderFactoryHandle {
        self.encoder.clone()
    }

    /// Register this wire format for encoded-only reception. Attach an ordered
    /// encoded sink before media. A no-output decoder lets libwebrtc perform native
    /// reference finding and frame-buffer ordering, but never fabricates decoded
    /// pixels; decoded sinks remain rejected. Closing interception does not
    /// enable decoded delivery, and a closed encoded sink cannot be reattached.
    pub fn encoded_receive_factory(&self) -> Result<crate::VideoDecoderFactoryHandle, CodecError> {
        crate::VideoDecoderFactoryHandle::encoded_receive(self.format.clone())
    }

    pub fn create_source_with_profile(
        &self,
        factory: &PeerConnectionFactory,
        profile: H264SourceProfile,
    ) -> Result<EncodedH264Source, PeerError> {
        self.create_profile_source_with_readiness(factory, profile, None)
    }
    pub(crate) fn create_profile_source_with_readiness(
        &self,
        factory: &PeerConnectionFactory,
        profile: H264SourceProfile,
        readiness: Option<Arc<crate::readiness::Readiness>>,
    ) -> Result<EncodedH264Source, PeerError> {
        use crate::video::DirectEncodedVideo;
        let codec = match profile {
            H264SourceProfile::L1T1 if self.format.name == "H264" => DirectEncodedVideo::H264,
            H264SourceProfile::L1T3 if self.temporal_fps.is_some() => DirectEncodedVideo::H264L1T3,
            H264SourceProfile::SimulcastL1T1 if self.simulcast => DirectEncodedVideo::H264Simulcast,
            H264SourceProfile::SimulcastL1T3 if self.simulcast && self.temporal_fps.is_some() => {
                DirectEncodedVideo::H264SimulcastL1T3
            }
            _ => {
                return Err(PeerError {
                    kind: PeerErrorKind::UnsupportedParameter,
                    message: "profile is not supported by this installed input factory".into(),
                });
            }
        };
        let mut source = self.create_source_with_readiness(factory, readiness)?;
        source.codec = codec;
        Ok(source)
    }
    pub fn create_source(
        &self,
        factory: &PeerConnectionFactory,
    ) -> Result<EncodedH264Source, PeerError> {
        self.create_source_with_readiness(factory, None)
    }

    pub(crate) fn create_source_with_readiness(
        &self,
        factory: &PeerConnectionFactory,
        readiness: Option<Arc<crate::readiness::Readiness>>,
    ) -> Result<EncodedH264Source, PeerError> {
        if !factory.uses_video_encoder(&self.encoder) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "factory is not configured with this encoded video input".into(),
            });
        }
        let source = factory.create_video_source()?;
        let id = {
            let mut broker = self
                .broker
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            broker.next_source = broker.next_source.checked_add(1).ok_or_else(|| PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "encoded source identifier exhausted".into(),
            })?;
            broker.next_source
        };
        Ok(EncodedH264Source {
            source,
            broker: self.broker.clone(),
            format: self.format.clone(),
            codec: self.codec,
            id,
            closed: false,
            last_timestamp_us: Cell::new(None),
            seen_keyframe: Cell::new(false),
            dropped: Arc::new(AtomicU64::new(0)),
            feedback: Arc::new(SourceFeedback {
                readiness,
                ..SourceFeedback::default()
            }),
        })
    }
}

/// A sequence-bound encoded-video source; use `create_track` to retain the
/// underlying source in the returned video track. Dropping this handle stops
/// accepting input and discards any queued access units for this source.
pub struct EncodedH264Source {
    source: VideoSource,
    broker: Arc<Mutex<Broker>>,
    format: VideoCodecFormat,
    codec: crate::video::DirectEncodedVideo,
    id: u64,
    closed: bool,
    last_timestamp_us: Cell<Option<i64>>,
    seen_keyframe: Cell<bool>,
    dropped: Arc<AtomicU64>,
    feedback: Arc<SourceFeedback>,
}

impl EncodedH264Source {
    pub fn stream_id(&self) -> u64 {
        self.id
    }

    pub fn create_track(
        &self,
        factory: &PeerConnectionFactory,
        id: &str,
    ) -> Result<VideoTrack, PeerError> {
        if self.closed || !factory.owns_video_source(&self.source) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "encoded source is closed or belongs to another factory".into(),
            });
        }
        let track = factory.create_video_track(id, &self.source)?;
        track.mark_direct_encoded(self.codec);
        Ok(track)
    }

    pub fn push(&self, frame: H264AccessUnit) -> Result<(), CodecError> {
        self.push_encoded(EncodedVideoAccessUnit {
            data: frame.data,
            width: frame.width,
            height: frame.height,
            timestamp_us: frame.timestamp_us,
            key_frame: frame.key_frame,
            qp: frame.qp,
            metadata: EncodedVideoMetadata {
                codec: EncodedVideoCodec::H264 {
                    base_layer_sync: false,
                },
                simulcast_index: None,
                spatial_index: None,
                temporal_index: None,
                end_of_picture: true,
            },
        })
    }

    pub fn push_encoded(&self, frame: EncodedVideoAccessUnit) -> Result<(), CodecError> {
        if self.codec.simulcast() {
            return Err(CodecError::InvalidConfiguration);
        }
        self.validate_unit(&frame, None)?;
        self.enqueue(vec![frame])
    }

    /// Reserve one bounded three-rung capture batch before triggering native
    /// work. All units share capture time and have ordered native indexes 0..2.
    /// Native initialization must expose all three ordered streams; ambiguous
    /// single-encoder fallback returns `InvalidConfiguration` asynchronously.
    /// L1T3 batches preserve each rung's explicit temporal index and base-layer
    /// sync flag; keyframes must be T0. Attached temporal senders require actual
    /// negotiated dependency descriptors before input admission.
    /// Native inactive/unallocated rungs may be dropped and are counted. Images
    /// are published separately; a later callback failure cannot retract an
    /// earlier image. Dimensions must match native sender configuration.
    pub fn push_simulcast(&self, frames: [EncodedVideoAccessUnit; 3]) -> Result<(), CodecError> {
        if !self.codec.simulcast() {
            return Err(CodecError::InvalidConfiguration);
        }
        let time = frames[0].timestamp_us;
        for (index, frame) in frames.iter().enumerate() {
            self.validate_unit(frame, Some(index as u8))?;
            if frame.timestamp_us != time
                || (index > 0
                    && (frames[index - 1].width >= frame.width
                        || frames[index - 1].height >= frame.height))
            {
                return Err(CodecError::InvalidFrame);
            }
        }
        self.enqueue(Vec::from(frames))
    }

    fn validate_unit(
        &self,
        frame: &EncodedVideoAccessUnit,
        index: Option<u8>,
    ) -> Result<(), CodecError> {
        if self.closed {
            return Err(CodecError::Released);
        }
        let matching_codec = matches!(
            (self.format.name.as_str(), frame.metadata.codec),
            ("H264", EncodedVideoCodec::H264 { .. })
                | ("VP8", EncodedVideoCodec::Vp8 { .. })
                | ("VP9", EncodedVideoCodec::Vp9 { .. })
                | ("AV1", EncodedVideoCodec::Av1)
                | ("H265", EncodedVideoCodec::H265)
        );
        if frame.width == 0
            || frame.height == 0
            || frame.width > 4096
            || frame.height > 4096
            || frame.timestamp_us < 0
            || (self.source.uses_controlled_execution()
                && frame.timestamp_us as u128 > crate::MAX_CONTROLLED_TIME.as_micros())
            || (self.format.name == "VP8" && !valid_vp8(frame))
            || self
                .last_timestamp_us
                .get()
                .is_some_and(|last| frame.timestamp_us <= last)
            || (!self.seen_keyframe.get() && !frame.key_frame)
            || (self.format.name == "H264" && frame.qp.is_some_and(|qp| qp > 51))
            || matches!(frame.metadata.codec, EncodedVideoCodec::Vp8 { key_index: Some(index), .. } if index > 31)
            || matches!(frame.metadata.codec, EncodedVideoCodec::Vp9 { num_spatial_layers, .. } if num_spatial_layers != 1)
            || !matching_codec
            || frame.metadata.simulcast_index != index
            || frame.metadata.spatial_index.is_some()
            || if self.codec.temporal() {
                !matches!(frame.metadata.temporal_index, Some(0..=2))
                    || (frame.key_frame && frame.metadata.temporal_index != Some(0))
            } else {
                frame.metadata.temporal_index.is_some()
            }
            || !frame.metadata.end_of_picture
            || frame.data.is_empty()
            || frame.data.len() > MAX_PENDING_BYTES
            || (self.format.name == "H264" && !valid_annex_b(&frame.data, frame.key_frame))
        {
            return Err(CodecError::InvalidFrame);
        }
        Ok(())
    }

    fn enqueue(&self, frames: Vec<EncodedVideoAccessUnit>) -> Result<(), CodecError> {
        if self.codec.temporal() && !self.source.admits_temporal_input() {
            return Err(CodecError::InvalidConfiguration);
        }
        let largest = frames.last().ok_or(CodecError::InvalidFrame)?;
        let (width, height, time) = (largest.width, largest.height, largest.timestamp_us);
        let key_frame = frames.iter().all(|frame| frame.key_frame);
        let token = self
            .broker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(self.id, self.dropped.clone(), self.feedback.clone(), frames)?;
        if !ffi::video_source_push_encoded_trigger(self.source.native(), width, height, time, token)
        {
            let _ = self
                .broker
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take(token);
            return Err(CodecError::Released);
        }
        trace_input(
            "trigger",
            format_args!(
                "source={:p} token={token} capture_us={time}",
                Arc::as_ptr(&self.feedback)
            ),
        );
        self.last_timestamp_us.set(Some(time));
        if key_frame {
            self.seen_keyframe.set(true);
        }
        Ok(())
    }

    /// Returns and clears keyframe feedback signaled by the sending encoder.
    /// Feedback becomes visible when WebRTC tries to encode the next frame;
    /// an already-submitted delta frame can therefore be rejected. There is no
    /// idle-source notification guarantee. Polling does not synthesize input or
    /// infer encoder feedback from received RTCP.
    pub fn take_keyframe_request(&self) -> bool {
        self.feedback
            .keyframe_requested
            .swap(false, Ordering::AcqRel)
    }

    /// Returns and clears source-local encoding-indexed native keyframe feedback.
    /// For three-rung input indexes correspond to the ordered sender encodings.
    /// Like aggregate feedback, requests become visible at native Encode time.
    /// Reading this mask and the aggregate boolean clears them independently.
    pub fn take_encoding_keyframe_requests(&self) -> [bool; 3] {
        let mask = self.feedback.encoding_keyframes.swap(0, Ordering::AcqRel);
        std::array::from_fn(|index| mask & (1 << index) != 0)
    }

    /// Most recent native rate control for the encoder associated with this
    /// stream's presentation token. After that association, later native rate
    /// callbacks update this snapshot without another submitted access unit.
    pub fn latest_rate_control(&self) -> Option<VideoRateControl> {
        *self
            .feedback
            .rates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Take the most recent asynchronous encoder rejection for this source.
    /// L1T3 native configuration fallback rejects publication here instead of
    /// emitting unlayered media. Rejections increment dropped_frames.
    pub fn take_encoder_error(&self) -> Option<CodecError> {
        self.feedback
            .encoder_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Queued access units, not capture tokens (a simulcast batch counts three).
    pub fn pending_frames(&self) -> usize {
        self.broker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pending
            .values()
            .filter(|pending| pending.source_id == self.id)
            .map(|pending| pending.frames.len())
            .sum()
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn close(&mut self) -> Result<(), CodecError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.feedback.retire();
        self.broker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove_source(self.id);
        self.source.close()
    }
}

impl Drop for EncodedH264Source {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

// Reject structurally invalid adapter input before reservation or dispatch,
// independently of the selected runtime.
// Entropy-coded corruption remains the real upstream decoder's responsibility.
fn valid_vp8(frame: &EncodedVideoAccessUnit) -> bool {
    let data = &frame.data;
    if data.len() < 3 {
        return false;
    }
    let tag = u32::from(data[0]) | (u32::from(data[1]) << 8) | (u32::from(data[2]) << 16);
    if (tag & 1 == 0) != frame.key_frame || (tag >> 5) as usize > data.len() - 3 {
        return false;
    }
    if !frame.key_frame {
        return true;
    }
    data.len() >= 10
        && data[3..6] == [0x9d, 0x01, 0x2a]
        && u32::from(u16::from_le_bytes([data[6], data[7]]) & 0x3fff) == frame.width
        && u32::from(u16::from_le_bytes([data[8], data[9]]) & 0x3fff) == frame.height
}

fn valid_annex_b(data: &[u8], key: bool) -> bool {
    if data.is_empty() || data.len() > MAX_PENDING_BYTES {
        return false;
    }
    let mut nal_types = Vec::new();
    let mut cursor = 0;
    while cursor < data.len() {
        let prefix = if data[cursor..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[cursor..].starts_with(&[0, 0, 1]) {
            3
        } else {
            return false;
        };
        let start = cursor + prefix;
        if start >= data.len() {
            return false;
        }
        let next = (start..data.len()).find(|&at| data[at..].starts_with(&[0, 0, 1]));
        let end = next.unwrap_or(data.len());
        if start == end || data[start] & 0x80 != 0 {
            return false;
        }
        let nal_type = data[start] & 0x1f;
        if nal_type == 7 {
            if end - start < 4 {
                return false;
            }
            let profile_level_id = format!(
                "{:02x}{:02x}{:02x}",
                data[start + 1],
                data[start + 2],
                data[start + 3]
            );
            if !same_codec_format(&h264_format(&profile_level_id), &format()) {
                return false;
            }
        }
        if !matches!(nal_type, 1 | 5 | 6 | 7 | 8 | 9) {
            return false;
        }
        nal_types.push(nal_type);
        cursor = end;
    }
    let idr = nal_types.contains(&5);
    if key {
        idr && nal_types.contains(&7) && nal_types.contains(&8)
    } else {
        !idr && nal_types.contains(&1)
    }
}

fn format() -> VideoCodecFormat {
    h264_format("42e01f")
}

fn h264_format(profile_level_id: &str) -> VideoCodecFormat {
    VideoCodecFormat::new("H264")
        .with_parameter("level-asymmetry-allowed", "1")
        .with_parameter("packetization-mode", "1")
        .with_parameter("profile-level-id", profile_level_id)
}

struct InputFactory {
    broker: Arc<Mutex<Broker>>,
    format: VideoCodecFormat,
    temporal_fps: Option<[u8; 3]>,
    simulcast: bool,
}
impl VideoEncoderFactory for InputFactory {
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        vec![self.format.clone()]
    }
    fn query_support(
        &self,
        format: &VideoCodecFormat,
        mode: Option<&str>,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        CodecSupport {
            supported: same_codec_format(format, &self.format)
                && (mode.is_none()
                    || mode == Some("L1T1")
                    || (self.temporal_fps.is_some() && mode == Some("L1T3"))),
            power_efficient: false,
        }
    }
    fn create(&self, format: &VideoCodecFormat) -> Result<Box<dyn VideoEncoder>, CodecError> {
        if !same_codec_format(format, &self.format) {
            return Err(CodecError::UnsupportedFormat);
        }
        Ok(Box::new(InputEncoder {
            broker: self.broker.clone(),
            released: false,
            rates: None,
            temporal_fps: self.temporal_fps,
            simulcast: self.simulcast,
            configuration: None,
            active_feedback: Weak::new(),
            chains: ReferenceChains::default(),
        }))
    }
}

fn same_codec_format(a: &VideoCodecFormat, b: &VideoCodecFormat) -> bool {
    // Negotiated SDP formats do not carry the factory's capability mode list.
    // Native H.264 identity handles profile aliases, levels and packetization.
    if a.name == "H264" && b.name == "H264" {
        SdpVideoFormat::from(a).is_same_codec(&SdpVideoFormat::from(b))
    } else {
        a.name == b.name && a.parameters == b.parameters
    }
}

// Submission order belongs to one source, never to the factory-global token
// space. Each simulcast encoding has an independent reference chain. Temporal
// input is conservatively held after any gap until a genuine keyframe arrives.
#[derive(Default)]
struct ReferenceChains {
    previous: Option<(u64, u64)>,
    intact: [bool; 3],
}
impl ReferenceChains {
    fn begin_capture(&mut self, source: u64, sequence: u64) -> [bool; 3] {
        let follows = self.previous.is_some_and(|(previous_source, previous)| {
            previous_source == source && previous.checked_add(1) == Some(sequence)
        });
        self.previous = Some((source, sequence));
        let previous = std::mem::take(&mut self.intact);
        if follows { previous } else { [false; 3] }
    }
}

struct InputEncoder {
    broker: Arc<Mutex<Broker>>,
    released: bool,
    rates: Option<VideoRateControl>,
    temporal_fps: Option<[u8; 3]>,
    simulcast: bool,
    configuration: Option<VideoEncoderSettings>,
    active_feedback: Weak<SourceFeedback>,
    chains: ReferenceChains,
}
// Encoded input carries a native presentation token, not caller raw pixels.
// Read only the native fields this adapter consumes; never materialize I420.
#[derive(Debug, PartialEq)]
struct InputFrameContext {
    width: u32,
    height: u32,
    rtp_timestamp: u32,
    presentation_token: Option<i64>,
}
impl InputFrameContext {
    fn from_native(frame: &crate::RawVideoFrame) -> Result<Self, CodecError> {
        let width = u32::try_from(frame.width()).map_err(|_| CodecError::InvalidFrame)?;
        let height = u32::try_from(frame.height()).map_err(|_| CodecError::InvalidFrame)?;
        if width == 0 || height == 0 {
            return Err(CodecError::InvalidFrame);
        }
        let metadata = frame.metadata();
        Ok(Self {
            width,
            height,
            rtp_timestamp: metadata.rtp_timestamp,
            presentation_token: metadata
                .presentation_timestamp
                .and_then(|value| value.microseconds().ok()),
        })
    }
}
impl InputEncoder {
    fn encode_pending(
        &mut self,
        context: InputFrameContext,
        frame_types: &[VideoFrameType],
        callback: EncodedImageCallback,
    ) -> Result<(), CodecError> {
        if self.released {
            return Err(CodecError::Released);
        }
        let token = context.presentation_token.ok_or(CodecError::InvalidFrame)?;
        let pending = self
            .broker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take(token)
            .ok_or(CodecError::InvalidFrame)?;
        let previous = self
            .chains
            .begin_capture(pending.source_id, pending.sequence);
        trace_input(
            "encode",
            format_args!(
                "source={:p} token={token} rtp={} requested={frame_types:?}",
                Arc::as_ptr(&pending.feedback),
                context.rtp_timestamp
            ),
        );
        let settings = self
            .configuration
            .as_ref()
            .ok_or_else(|| pending.reject(CodecError::InvalidConfiguration))?;
        let simulcast = self.simulcast && settings.simulcast_streams.len() == 3;
        let temporal = if simulcast {
            settings
                .simulcast_streams
                .iter()
                .all(|stream| stream.temporal_layers == 3)
        } else {
            settings
                .scalability_mode
                .as_ref()
                .map(crate::VideoScalabilityMode::as_str)
                == Some("L1T3")
        };
        if (temporal && self.temporal_fps.is_none())
            || pending
                .frames
                .iter()
                .any(|frame| frame.metadata.temporal_index.is_some() != temporal)
        {
            return Err(pending.reject(CodecError::InvalidConfiguration));
        }
        if temporal
            && (settings.simulcast_temporal_layers != vec![3; if simulcast { 3 } else { 1 }]
                || !matches!(settings.h264_temporal_layers, Some(1..=3)))
        {
            return Err(pending.reject(CodecError::InvalidConfiguration));
        }
        let mut selected = vec![true; pending.frames.len()];
        if self.simulcast && pending.frames.len() == 3 && !simulcast {
            return Err(pending.reject(CodecError::InvalidConfiguration));
        }
        if simulcast {
            if pending.frames.len() != 3 || (!temporal && settings.h264_temporal_layers != Some(1))
            {
                return Err(pending.reject(CodecError::InvalidConfiguration));
            }
            match settings.simulcast_streams.len() {
                3 => {
                    for (index, stream) in settings.simulcast_streams.iter().enumerate() {
                        let unit = &pending.frames[index];
                        if stream.temporal_layers != if temporal { 3 } else { 1 }
                            || (stream.active
                                && (unit.width, unit.height) != (stream.width, stream.height))
                        {
                            return Err(pending.reject(CodecError::InvalidConfiguration));
                        }
                        // Native VideoBitrateAllocation sums unset cells as
                        // zero. No positive cell means no allocated funding;
                        // preserve None vs Some(0) in the exposed snapshot.
                        selected[index] = stream.active
                            && self.rates.is_none_or(|rates| {
                                rates.layer_bitrates_bps[index]
                                    .iter()
                                    .flatten()
                                    .any(|&bps| bps > 0)
                            });
                    }
                }
                // A single-encoder SEA fallback and a collapsed sender can
                // expose the same zero-stream configuration. Geometry does
                // not prove original encoding identity. Never guess a rung.
                _ => return Err(pending.reject(CodecError::InvalidConfiguration)),
            }
        } else if pending.frames.len() != 1
            || (pending.frames[0].width, pending.frames[0].height)
                != (context.width, context.height)
        {
            return Err(pending.reject(CodecError::InvalidFrame));
        }
        trace_input(
            "select",
            format_args!(
                "source={:p} token={token} selected={selected:?} rates={:?}",
                Arc::as_ptr(&pending.feedback),
                self.rates
            ),
        );
        self.active_feedback = Arc::downgrade(&pending.feedback);
        if let Some(rates) = self.rates {
            pending.feedback.set_rates(rates);
        }
        // Validate every selected image before the first callback. Callback
        // publication is still non-atomic, exactly like the native interface.
        pending.check_keyframe_requests(frame_types, &selected)?;
        let decodable = pending.decodable_units(previous, &selected);
        let count = pending.frames.len();
        for (index, unit) in pending.frames.into_iter().enumerate() {
            if !selected[index] {
                pending.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if !decodable[index] {
                trace_input(
                    "hold_delta",
                    format_args!(
                        "source={:p} token={token} index={index}",
                        Arc::as_ptr(&pending.feedback)
                    ),
                );
                pending.dropped.fetch_add(1, Ordering::Relaxed);
                if let Err(error) =
                    callback.frame_dropped(context.rtp_timestamp, index as i32, true)
                {
                    pending
                        .dropped
                        .fetch_add((count - index - 1) as u64, Ordering::Relaxed);
                    pending.feedback.record_error(error);
                    return Err(error);
                }
                continue;
            }
            trace_input(
                "emit",
                format_args!(
                    "source={:p} token={token} index={index} rtp={} key={} bytes={}",
                    Arc::as_ptr(&pending.feedback),
                    context.rtp_timestamp,
                    unit.key_frame,
                    unit.data.len()
                ),
            );
            if let Err(error) = callback.emit_with_metadata(
                &EncodedVideoFrame {
                    data: unit.data,
                    width: unit.width,
                    height: unit.height,
                    rtp_timestamp: context.rtp_timestamp,
                    frame_type: if unit.key_frame {
                        VideoFrameType::Key
                    } else {
                        VideoFrameType::Delta
                    },
                    qp: unit.qp,
                },
                unit.metadata,
            ) {
                trace_input(
                    "emit_error",
                    format_args!(
                        "source={:p} token={token} index={index} error={error:?}",
                        Arc::as_ptr(&pending.feedback)
                    ),
                );
                pending
                    .dropped
                    .fetch_add((count - index) as u64, Ordering::Relaxed);
                pending.feedback.record_error(error);
                return Err(error);
            }
            self.chains.intact[index] = true;
        }
        Ok(())
    }
}

impl VideoEncoder for InputEncoder {
    fn initialize(&mut self, settings: VideoEncoderSettings) -> Result<(), CodecError> {
        self.released = false;
        self.rates = None;
        self.active_feedback = Weak::new();
        self.chains = ReferenceChains::default();
        self.configuration = Some(settings);
        Ok(())
    }
    fn encode(
        &mut self,
        _: VideoFrame,
        _: &[VideoFrameType],
        _: EncodedImageCallback,
    ) -> Result<(), CodecError> {
        Err(CodecError::InvalidFrame)
    }
    fn encode_value(
        &mut self,
        frame: crate::RawVideoFrame,
        types: Option<Vec<crate::EncodedImageFrameType>>,
        callback: EncodedImageCallback,
    ) -> Result<i32, CodecError> {
        let context = InputFrameContext::from_native(&frame)?;
        let types = types
            .unwrap_or_default()
            .into_iter()
            .map(|value| match value {
                crate::EncodedImageFrameType::Key => Ok(VideoFrameType::Key),
                crate::EncodedImageFrameType::Delta => Ok(VideoFrameType::Delta),
                _ => Err(CodecError::InvalidFrame),
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.encode_pending(context, &types, callback)
            .map(|()| crate::codec::CODEC_OK)
    }
    fn encode_with_presentation_token(
        &mut self,
        frame: VideoFrame,
        frame_types: &[VideoFrameType],
        token: Option<i64>,
        callback: EncodedImageCallback,
    ) -> Result<(), CodecError> {
        self.encode_pending(
            InputFrameContext {
                width: frame.width,
                height: frame.height,
                rtp_timestamp: frame.rtp_timestamp,
                presentation_token: token,
            },
            frame_types,
            callback,
        )
    }
    fn set_rates(&mut self, rates: VideoRateControl) -> Result<(), CodecError> {
        self.rates = Some(rates);
        // The native encoder is associated with a source by its most recent
        // presentation token. Later native rate callbacks update that source
        // directly, without requiring another access unit or retaining it.
        if let Some(feedback) = self.active_feedback.upgrade() {
            feedback.set_rates(rates);
        }
        Ok(())
    }
    fn release(&mut self) -> Result<(), CodecError> {
        self.released = true;
        Ok(())
    }
    fn info_value(&self) -> Result<crate::OwnedVideoEncoderInfo, CodecError> {
        let mut result = self.info().into_native();
        let mut fields = result.fields();
        // The external encoder owns rate control. Native bitrate-based frame
        // dropping cannot safely discard already-encoded reference pictures.
        // Keep native queue/congestion handling and forward SetRates upstream.
        fields.has_trusted_rate_controller = true;
        result.set_fields(fields);
        Ok(result)
    }
    fn info(&self) -> VideoEncoderInfo {
        VideoEncoderInfo {
            implementation_name: "direct encoded video input".into(),
            hardware_accelerated: false,
            supports_native_handle: false,
            supports_simulcast: self.simulcast,
            fps_allocation: if self
                .configuration
                .as_ref()
                .is_some_and(|settings| settings.simulcast_streams.len() == 3)
            {
                let fps = if self.configuration.as_ref().is_some_and(|settings| {
                    settings
                        .simulcast_streams
                        .iter()
                        .all(|stream| stream.temporal_layers == 3)
                }) {
                    self.temporal_fps.map_or_else(Vec::new, |fps| fps.to_vec())
                } else {
                    vec![255]
                };
                Some([fps.clone(), fps.clone(), fps, vec![], vec![]])
            } else if self.configuration.as_ref().is_some_and(|settings| {
                settings
                    .scalability_mode
                    .as_ref()
                    .map(crate::VideoScalabilityMode::as_str)
                    != Some("L1T3")
            }) {
                Some([vec![255], vec![], vec![], vec![], vec![]])
            } else {
                self.temporal_fps
                    .map(|fps| [fps.to_vec(), vec![], vec![], vec![], vec![]])
            },
        }
    }
}

/// Codec-neutral name for the encoded input factory.
pub type EncodedVideoInput = EncodedH264Input;
/// Codec-neutral name for a direct encoded source.
pub type EncodedVideoSource = EncodedH264Source;

#[cfg(test)]
mod tests {
    use super::*;

    fn broker_batch(bytes: usize) -> Vec<EncodedVideoAccessUnit> {
        (0..3)
            .map(|index| EncodedVideoAccessUnit {
                data: vec![0; bytes],
                width: 320 << index,
                height: 180 << index,
                timestamp_us: 0,
                key_frame: true,
                qp: None,
                metadata: EncodedVideoMetadata {
                    codec: EncodedVideoCodec::H264 {
                        base_layer_sync: false,
                    },
                    simulcast_index: Some(index as u8),
                    spatial_index: None,
                    temporal_index: None,
                    end_of_picture: true,
                },
            })
            .collect()
    }

    #[test]
    fn encoded_passthrough_owns_rate_control_without_claiming_native_buffers() {
        for (simulcast, temporal_fps) in
            [(false, None), (true, None), (false, Some([64, 128, 255]))]
        {
            let encoder = InputEncoder {
                broker: Arc::new(Mutex::new(Broker::default())),
                released: false,
                rates: None,
                temporal_fps,
                simulcast,
                configuration: None,
                active_feedback: Weak::new(),
                chains: ReferenceChains::default(),
            };
            let legacy = encoder.info();
            let fields = encoder.info_value().unwrap().fields();
            assert!(fields.has_trusted_rate_controller);
            assert!(!fields.supports_native_handle);
            assert!(fields.scaling_settings.thresholds.is_none());
            assert_eq!(fields.supports_simulcast, legacy.supports_simulcast);
            assert_eq!(
                fields.implementation_name,
                legacy.implementation_name.as_bytes()
            );
            if let Some(fps) = legacy.fps_allocation {
                assert_eq!(fields.fps_allocation, fps);
            }
        }
        assert!(
            !crate::OwnedVideoEncoderInfo::default()
                .fields()
                .has_trusted_rate_controller
        );
    }

    #[test]
    fn native_capture_gaps_hold_deltas_until_a_keyframe_restores_the_chain() {
        // Metadata-only model of source submission and successful callbacks.
        // These units are not passed to a decoder or into an RTC connection.
        let mut chains = ReferenceChains::default();
        for (sequence, keyframe, expected) in [
            (0, false, false),
            (1, true, true),
            (2, false, true),
            (4, false, false), // Capture 3 never reached Encode.
            (5, false, false),
            (6, true, true),
            (7, false, true),
        ] {
            let mut frames = broker_batch(1);
            frames.truncate(1);
            frames[0].key_frame = keyframe;
            let pending = Pending {
                source_id: 1,
                sequence,
                dropped: Arc::new(AtomicU64::new(0)),
                feedback: Arc::new(SourceFeedback::default()),
                frames,
            };
            let previous = chains.begin_capture(pending.source_id, pending.sequence);
            assert_eq!(pending.decodable_units(previous, &[true]), [expected]);
            assert_eq!(
                pending.feedback.keyframe_requested.load(Ordering::Acquire),
                !expected
            );
            if expected {
                chains.intact[0] = true;
            }
        }
    }

    #[test]
    fn simulcast_recovery_is_per_encoding_and_ignores_unfunded_units() {
        let mut frames = broker_batch(1);
        for frame in &mut frames {
            frame.key_frame = false;
        }
        let mut pending = Pending {
            source_id: 1,
            sequence: 1,
            dropped: Arc::new(AtomicU64::new(0)),
            feedback: Arc::new(SourceFeedback::default()),
            frames,
        };
        assert_eq!(
            pending.decodable_units([true, false, false], &[true, true, false]),
            [true, false, false]
        );
        assert_eq!(
            pending.feedback.encoding_keyframes.load(Ordering::Acquire),
            0b010
        );
        pending.frames[1].key_frame = true;
        assert_eq!(
            pending.decodable_units([true, false, false], &[true, true, false]),
            [true, true, false]
        );
    }

    #[test]
    fn source_replacement_rejection_and_partial_callback_failure_break_only_unemitted_chains() {
        let mut chains = ReferenceChains::default();
        assert_eq!(chains.begin_capture(1, 0), [false; 3]);
        // Only the first callback succeeds; later encodings fail or are unfunded.
        chains.intact[0] = true;
        assert_eq!(chains.begin_capture(1, 1), [true, false, false]);
        // This capture is rejected before any publication, so none are marked.
        assert_eq!(chains.begin_capture(1, 2), [false; 3]);
        chains.intact = [true; 3];
        assert_eq!(chains.begin_capture(2, 3), [false; 3]);
        chains.intact = [true; 3];
        // Reordered/duplicate source input is not a continuation either.
        assert_eq!(chains.begin_capture(2, 3), [false; 3]);
    }

    #[test]
    fn submitted_sequences_are_source_local_not_global_broker_tokens() {
        let mut broker = Broker::default();
        let first = Arc::new(SourceFeedback::default());
        let second = Arc::new(SourceFeedback::default());
        let dropped = Arc::new(AtomicU64::new(0));
        let a = broker
            .insert(1, dropped.clone(), first.clone(), broker_batch(1))
            .unwrap();
        let b = broker
            .insert(2, dropped.clone(), second, broker_batch(1))
            .unwrap();
        assert_eq!(
            broker.insert(
                1,
                dropped.clone(),
                first.clone(),
                broker_batch(MAX_PENDING_BYTES)
            ),
            Err(CodecError::InvalidFrame)
        );
        let c = broker.insert(1, dropped, first, broker_batch(1)).unwrap();
        assert_eq!((a, b, c), (1, 2, 3));
        assert_eq!(broker.take(a).unwrap().sequence, 0);
        assert_eq!(broker.take(b).unwrap().sequence, 0);
        assert_eq!(broker.take(c).unwrap().sequence, 1);
    }

    #[test]
    fn all_selected_native_keyframe_requests_survive_first_delta_rejection() {
        // Metadata-only preflight test; these broker values never enter RTC.
        let mut frames = broker_batch(1);
        for frame in &mut frames {
            frame.key_frame = false;
        }
        let pending = Pending {
            source_id: 1,
            sequence: 0,
            dropped: Arc::new(AtomicU64::new(0)),
            feedback: Arc::new(SourceFeedback::default()),
            frames,
        };
        assert_eq!(
            pending.check_keyframe_requests(&[VideoFrameType::Key; 3], &[true; 3]),
            Err(CodecError::InvalidFrame)
        );
        assert_eq!(
            pending.feedback.encoding_keyframes.load(Ordering::Acquire),
            0b111
        );
        assert!(pending.feedback.keyframe_requested.load(Ordering::Acquire));
        assert_eq!(pending.dropped.load(Ordering::Relaxed), 3);
        assert_eq!(
            *pending.feedback.encoder_error.lock().unwrap(),
            Some(CodecError::InvalidFrame)
        );
    }

    #[test]
    fn unfunded_units_do_not_publish_selected_keyframe_feedback() {
        let pending = Pending {
            source_id: 1,
            sequence: 0,
            dropped: Arc::new(AtomicU64::new(0)),
            feedback: Arc::new(SourceFeedback::default()),
            frames: broker_batch(1),
        };
        assert_eq!(
            pending.check_keyframe_requests(&[VideoFrameType::Key; 3], &[true, true, false]),
            Ok(())
        );
        assert_eq!(
            pending.feedback.encoding_keyframes.load(Ordering::Acquire),
            0b011
        );
        assert_eq!(pending.dropped.load(Ordering::Relaxed), 0);
        assert!(pending.feedback.encoder_error.lock().unwrap().is_none());
    }

    #[test]
    fn batch_budget_rejects_atomically_and_evicts_whole_capture_tokens() {
        let mut broker = Broker::default();
        let dropped = Arc::new(AtomicU64::new(0));
        let feedback = Arc::new(SourceFeedback::default());
        let first = broker
            .insert(1, dropped.clone(), feedback.clone(), broker_batch(1))
            .unwrap();
        assert_eq!(broker.bytes, 3);
        assert_eq!(
            broker.insert(
                1,
                dropped.clone(),
                feedback.clone(),
                broker_batch(MAX_PENDING_BYTES / 3 + 1)
            ),
            Err(CodecError::InvalidFrame)
        );
        assert_eq!(broker.order, VecDeque::from([first]));
        assert_eq!(broker.bytes, 3);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        let large = broker
            .insert(
                1,
                dropped.clone(),
                feedback.clone(),
                broker_batch(MAX_PENDING_BYTES / 3),
            )
            .unwrap();
        assert!(!broker.pending.contains_key(&first));
        assert_eq!(broker.pending[&large].frames.len(), 3);
        assert_eq!(dropped.load(Ordering::Relaxed), 3);
        assert_eq!(broker.bytes, (MAX_PENDING_BYTES / 3) * 3);
        let pending = broker.take(large).unwrap();
        assert_eq!(broker.bytes, 0);
        assert!(broker.order.is_empty());
        assert_eq!(
            pending.reject(CodecError::InvalidConfiguration),
            CodecError::InvalidConfiguration
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn batch_fifo_and_source_cleanup_preserve_unit_accounting() {
        let mut broker = Broker::default();
        let dropped = Arc::new(AtomicU64::new(0));
        let feedback = Arc::new(SourceFeedback::default());
        for _ in 0..MAX_PENDING + 1 {
            broker
                .insert(1, dropped.clone(), feedback.clone(), broker_batch(1))
                .unwrap();
        }
        assert_eq!(broker.pending.len(), MAX_PENDING);
        assert_eq!(broker.bytes, MAX_PENDING * 3);
        assert_eq!(dropped.load(Ordering::Relaxed), 3);
        let other = broker
            .insert(2, dropped.clone(), feedback.clone(), broker_batch(1))
            .unwrap();
        assert_eq!(dropped.load(Ordering::Relaxed), 6);
        broker.remove_source(1);
        assert_eq!(broker.bytes, 3);
        assert_eq!(broker.order, VecDeque::from([other]));
        assert_eq!(dropped.load(Ordering::Relaxed), 6);
        broker.remove_source(2);
        assert_eq!(broker.bytes, 0);
        assert!(broker.pending.is_empty());
        assert!(broker.order.is_empty());
    }

    #[test]
    fn encoded_input_context_reads_native_geometry_rtp_and_presentation_token() {
        let legacy = VideoFrame::i420(2, 2, vec![16; 6], 123_456, 90_000).unwrap();
        let mut frame = crate::RawVideoFrame::from_legacy(&legacy).unwrap();
        assert_eq!(
            InputFrameContext::from_native(&frame).unwrap(),
            InputFrameContext {
                width: 2,
                height: 2,
                rtp_timestamp: 90_000,
                presentation_token: None,
            }
        );
        let mut metadata = frame.metadata();
        metadata.presentation_timestamp = Some(crate::Timestamp::from_microseconds(7).unwrap());
        frame.set_metadata(&metadata).unwrap();
        let context = InputFrameContext::from_native(&frame).unwrap();
        assert_eq!(context.presentation_token, Some(7));
        assert_ne!(context.presentation_token, Some(frame.timestamp_us()));
        assert_eq!(context.rtp_timestamp, legacy.rtp_timestamp);
        assert_eq!(
            (context.width, context.height),
            (legacy.width, legacy.height)
        );
        metadata.presentation_timestamp = None;
        frame.set_metadata(&metadata).unwrap();
        assert_eq!(
            InputFrameContext::from_native(&frame)
                .unwrap()
                .presentation_token,
            None
        );
    }

    #[test]
    fn native_rate_updates_follow_the_active_source_without_another_frame() {
        let first = Arc::new(SourceFeedback::default());
        let replacement = Arc::new(SourceFeedback::default());
        let mut encoder = InputEncoder {
            broker: Arc::new(Mutex::new(Broker::default())),
            released: false,
            rates: None,
            temporal_fps: None,
            simulcast: false,
            configuration: None,
            active_feedback: Arc::downgrade(&first),
            chains: ReferenceChains::default(),
        };
        let rate = VideoRateControl {
            bitrate_bps: 100_000,
            target_bitrate_bps: 120_000,
            framerate_fps: 30.0,
            bandwidth_bps: 200_000,
            bandwidth_limited: false,
            target_bandwidth_limited: false,
            layer_bitrates_bps: [[None; 4]; 5],
            target_layer_bitrates_bps: [[None; 4]; 5],
        };
        encoder.set_rates(rate).unwrap();
        assert_eq!(*first.rates.lock().unwrap(), Some(rate));
        assert_eq!(*replacement.rates.lock().unwrap(), None);
        encoder.chains.intact = [true; 3];
        encoder
            .initialize(VideoEncoderSettings {
                width: 320,
                height: 180,
                start_bitrate_bps: 100_000,
                max_bitrate_bps: 200_000,
                min_bitrate_bps: 10_000,
                max_framerate: 30,
                cores: 1,
                max_payload_size: 1200,
                scalability_mode: None,
                h264_temporal_layers: Some(1),
                simulcast_temporal_layers: Vec::new(),
                simulcast_streams: Vec::new(),
            })
            .unwrap();
        assert_eq!(encoder.rates, None);
        assert!(encoder.active_feedback.upgrade().is_none());
        assert_eq!(encoder.chains.intact, [false; 3]);
        let updated = VideoRateControl {
            bitrate_bps: 50_000,
            ..rate
        };
        // Native SetRates can arrive after reinitialization but before another
        // presentation token associates this encoder with a source.
        encoder.set_rates(updated).unwrap();
        assert_eq!(*first.rates.lock().unwrap(), Some(rate));
        assert_eq!(*replacement.rates.lock().unwrap(), None);
        encoder.active_feedback = Arc::downgrade(&replacement);
        encoder.set_rates(updated).unwrap();
        assert_eq!(*first.rates.lock().unwrap(), Some(rate));
        assert_eq!(*replacement.rates.lock().unwrap(), Some(updated));
        drop(replacement);
        encoder.set_rates(rate).unwrap();
        assert!(encoder.active_feedback.upgrade().is_none());
    }

    #[test]
    fn changed_source_feedback_wakes_the_actor_and_coalesces_snapshots() {
        use std::task::{Context, Wake, Waker};

        #[derive(Default)]
        struct Counter(AtomicU64);
        impl Wake for Counter {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let readiness = Arc::new(crate::readiness::Readiness::default());
        let feedback = SourceFeedback {
            readiness: Some(readiness.clone()),
            ..SourceFeedback::default()
        };
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(readiness.poll(&mut cx).is_pending());
        let rates = VideoRateControl {
            bitrate_bps: 1,
            target_bitrate_bps: 1,
            framerate_fps: 30.0,
            bandwidth_bps: 2,
            bandwidth_limited: false,
            target_bandwidth_limited: false,
            layer_bitrates_bps: [[None; 4]; 5],
            target_layer_bitrates_bps: [[None; 4]; 5],
        };
        feedback.set_rates(rates);
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
        assert!(readiness.poll(&mut cx).is_ready());
        assert!(readiness.poll(&mut cx).is_pending());
        feedback.set_rates(rates);
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
        assert!(readiness.poll(&mut cx).is_pending());
        feedback.request_keyframe();
        feedback.request_keyframe();
        assert_eq!(counter.0.load(Ordering::Relaxed), 2);
        assert!(readiness.poll(&mut cx).is_ready());
        assert!(readiness.poll(&mut cx).is_pending());
        feedback.record_error(CodecError::InvalidConfiguration);
        assert_eq!(counter.0.load(Ordering::Relaxed), 3);
        assert!(readiness.poll(&mut cx).is_ready());
        assert_eq!(
            *feedback.encoder_error.lock().unwrap(),
            Some(CodecError::InvalidConfiguration)
        );
        assert!(feedback.keyframe_requested.load(Ordering::Acquire));
        assert_eq!(*feedback.rates.lock().unwrap(), Some(rates));
    }

    #[test]
    fn h264_input_uses_native_profile_level_identity() {
        for profile_level_id in ["42e01f", "42c028", "42e028"] {
            let requested = h264_format(profile_level_id);
            assert!(same_codec_format(&requested, &format()));
            assert!(EncodedH264Input::new_for_format(requested.clone()).is_ok());
            assert!(EncodedH264Input::new_l1t3_for_format(requested, [64, 128, 255]).is_ok());
        }
        for profile_level_id in ["640028", "42e0ff"] {
            assert!(!same_codec_format(
                &h264_format(profile_level_id),
                &format()
            ));
            assert!(EncodedH264Input::new_for_format(h264_format(profile_level_id)).is_err());
        }
        // SPS header validation is not a decoder proof. The genuine level-4.0
        // VBR corpus is exercised by the consuming application's SFU witness.
        assert!(valid_annex_b(
            &[
                0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x28, 0, 0, 1, 0x68, 1, 0, 0, 1, 0x65, 1
            ],
            true
        ));
        assert!(!valid_annex_b(
            &[
                0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 1, 0x68, 1, 0, 0, 1, 0x65, 1
            ],
            true
        ));
    }

    #[test]
    fn rejects_malformed_access_units() {
        assert!(!valid_annex_b(&[], true));
        assert!(!valid_annex_b(&[0, 0, 0, 1, 0x65], false));
        assert!(!valid_annex_b(&[0, 0, 0, 1, 0x65], true));
        assert!(valid_annex_b(
            &[
                0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, 0, 0, 1, 0x68, 1, 0, 0, 1, 0x65, 1
            ],
            true
        ));
    }
}
