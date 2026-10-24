//! Owned, validated CPU audio frames for injected media.
//!
//! Input is interleaved signed 16-bit PCM in native `rate / 100` sample blocks.
//! Unsupported rates, layouts, or partial blocks are rejected before crossing
//! the native bridge. Source-specific processing and device APIs are separate.

use std::{cell::Cell, fmt, rc::Rc};

use crate::{
    CodecParameter, ffi,
    peer::{FactoryInner, PeerError, PeerErrorKind, PeerInner},
};

/// An audio RTP capability reported by the selected peer factory. Opus uses
/// SDP channel count 2 even for mono; its `stereo` fmtp selects send channels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioCodecCapability {
    name: String,
    parameters: Vec<CodecParameter>,
    clock_rate: Option<u32>,
    channels: Option<u8>,
}

impl AudioCodecCapability {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn parameters(&self) -> &[CodecParameter] {
        &self.parameters
    }
    pub fn clock_rate(&self) -> Option<u32> {
        self.clock_rate
    }
    pub fn channels(&self) -> Option<u8> {
        self.channels
    }
    pub(crate) fn from_ffi(value: ffi::FfiAudioCodecCapability) -> Self {
        Self {
            name: value.format.name,
            parameters: value
                .format
                .parameters
                .into_iter()
                .map(|item| CodecParameter {
                    key: item.key,
                    value: item.value,
                })
                .collect(),
            clock_rate: u32::try_from(value.clock_rate).ok(),
            channels: u8::try_from(value.channels).ok(),
        }
    }
    pub(crate) fn ffi_format(&self) -> ffi::FfiCodecFormat {
        ffi::FfiCodecFormat {
            name: self.name.clone(),
            scalability_modes: Vec::new(),
            parameters: self
                .parameters
                .iter()
                .map(|item| ffi::FfiCodecParameter {
                    key: item.key.clone(),
                    value: item.value.clone(),
                })
                .collect(),
        }
    }
}

/// A platform audio device reported by the native artifact. Device lists can
/// change; re-enumerate before using an index, and handle selection failure.
#[cfg(feature = "native")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDevice {
    pub index: u16,
    pub name: String,
    pub id: String,
}

/// The layout and representation of a CPU audio frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioSampleFormat {
    /// Signed 16-bit, native-endian, interleaved samples.
    I16Interleaved,
}

/// An invalid or unsupported PCM frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFrameError {
    InvalidSampleRate,
    InvalidChannels,
    InvalidSampleCount,
    InvalidTimestamp,
    Released,
    /// Actual native ProcessStream error. No frame is delivered on failure.
    ProcessingFailed(i32),
    /// Binding admission or callback failure. No PCM reached the source sinks.
    ProcessingAdmission(crate::AudioProcessorError),
    /// The custom capture path lacks required echo or microphone feedback.
    UnsupportedProcessing,
    /// Same-source callback recursion was refused before acquiring source locks.
    ReentrantCall,
}

impl fmt::Display for AudioFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for AudioFrameError {}

/// Owned, timestamped PCM ready for injection into an audio source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioPcmFrame {
    pub format: AudioSampleFormat,
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub samples_per_channel: u32,
    /// Caller capture timestamp in microseconds. The current raw source does
    /// not forward this value as an absolute capture clock or RTP timestamp.
    pub timestamp_us: i64,
    pub samples: Vec<i16>,
}

impl AudioPcmFrame {
    /// Constructs one native PCM input block of `sample_rate_hz / 100`
    /// samples per channel. Rates 100..=48000 Hz and native input layouts of
    /// 1, 2, 4, 6 or 8 channels are supported. No sample padding or truncation
    /// occurs, including for rates such as 44100 Hz.
    pub fn i16_interleaved(
        sample_rate_hz: u32,
        channels: u8,
        timestamp_us: i64,
        samples: Vec<i16>,
    ) -> Result<Self, AudioFrameError> {
        let samples_per_channel =
            Self::validate(sample_rate_hz, channels, timestamp_us, samples.len())?;
        Ok(Self {
            format: AudioSampleFormat::I16Interleaved,
            sample_rate_hz,
            channels,
            samples_per_channel,
            timestamp_us,
            samples,
        })
    }

    fn validate(
        sample_rate_hz: u32,
        channels: u8,
        timestamp_us: i64,
        sample_count: usize,
    ) -> Result<u32, AudioFrameError> {
        if !(100..=48_000).contains(&sample_rate_hz) {
            return Err(AudioFrameError::InvalidSampleRate);
        }
        if !matches!(channels, 1 | 2 | 4 | 6 | 8) {
            return Err(AudioFrameError::InvalidChannels);
        }
        if timestamp_us < 0 {
            return Err(AudioFrameError::InvalidTimestamp);
        }
        let samples_per_channel = sample_rate_hz / 100;
        if sample_count != samples_per_channel as usize * channels as usize {
            return Err(AudioFrameError::InvalidSampleCount);
        }
        Ok(samples_per_channel)
    }
}

/// An owning PCM block observed on a native audio track. Remote tracks supply
/// decoded audio; local tracks expose their actual source handoff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedAudioFrame {
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub samples_per_channel: u32,
    /// Exact optional capture value from the native sink callback, in the
    /// upstream TimeMillis domain. Zero and negative values remain present.
    pub capture_time_ms: Option<i64>,
    /// Microsecond projection, absent when native capture time is absent or
    /// cannot be represented in this unit. `capture_time_ms` remains exact.
    pub capture_time_us: Option<i64>,
    /// Signed 16-bit, native-endian, interleaved samples.
    pub samples: Vec<i16>,
}

impl DecodedAudioFrame {
    fn from_native(frame: ffi::FfiReceivedAudioFrame) -> Option<Self> {
        frame.valid.then(|| Self {
            sample_rate_hz: frame.sample_rate_hz,
            channels: frame.channels,
            samples_per_channel: frame.samples_per_channel,
            capture_time_ms: frame.has_capture_time.then_some(frame.capture_time_ms),
            capture_time_us: frame
                .has_capture_time
                .then_some(frame.capture_time_ms)
                .and_then(|value| value.checked_mul(1000)),
            samples: frame.samples,
        })
    }
}

/// Owned decoded PCM with its recipient and receiver identity. `observed_at`
/// is the controlled timeline at the explicit playout/poll invocation, not a
/// wall-clock profiling value. Capture timing remains in `frame` when supplied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedAudioFrame {
    pub peer_id: Option<u64>,
    pub receiver_id: String,
    pub observed_at: Option<std::time::Duration>,
    pub frame: DecodedAudioFrame,
}

/// A bounded decoded PCM receiver queue. Closing removes the native sink on
/// the signaling thread before releasing callback memory. Dropped frames are
/// accounted for; at most eight blocks are retained. On headless peers,
/// `try_next_frame` pulls one 10 ms playout block when the queue is empty;
/// it does not open an operating-system audio output device.
pub struct AudioSink {
    native: cxx::UniquePtr<ffi::NativeAudioSink>,
    _peer: Rc<PeerInner>,
    receiver_id: String,
}

impl AudioSink {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeAudioSink>,
        peer: Rc<PeerInner>,
        receiver_id: String,
    ) -> Self {
        Self {
            native,
            _peer: peer,
            receiver_id,
        }
    }

    pub fn try_next_received_frame(&self) -> Option<ReceivedAudioFrame> {
        self.try_next_frame().map(|frame| ReceivedAudioFrame {
            peer_id: self._peer.controlled_id,
            receiver_id: self.receiver_id.clone(),
            observed_at: self._peer.controlled_time(),
            frame,
        })
    }

    pub fn try_next_frame(&self) -> Option<DecodedAudioFrame> {
        if self._peer.closed.get() {
            return None;
        }
        DecodedAudioFrame::from_native(ffi::audio_sink_take_frame(self.native()))
    }

    pub fn dropped_frames(&self) -> u64 {
        ffi::audio_sink_dropped_frames(self.native())
    }

    /// Detach on the signaling thread and release the receiver reservation.
    /// Either audio receive mode may then be attached again. Repeated calls
    /// and dropping this closed handle do not affect a replacement sink.
    pub fn close(&mut self) -> Result<(), PeerError> {
        ffi::close_audio_sink(self.native())
            .then_some(())
            .ok_or_else(|| PeerError {
                kind: PeerErrorKind::Internal,
                message: "failed to detach audio sink".into(),
            })
    }

    fn native(&self) -> &ffi::NativeAudioSink {
        self.native.as_ref().expect("validated audio sink")
    }
}

/// Bounded owning observation of a local audio track's actual native OnData.
/// No headless playout or RTP receiver identity is invented for a local track.
/// The track, its source and its factory outlive native sink detachment.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::AudioTrackSink>();
/// ```
pub struct AudioTrackSink {
    native: cxx::UniquePtr<ffi::NativeAudioSink>,
    _track: Rc<TrackInner>,
}
impl AudioTrackSink {
    fn native(&self) -> &ffi::NativeAudioSink {
        self.native.as_ref().expect("local native audio sink")
    }
    pub fn try_next_frame(&self) -> Option<DecodedAudioFrame> {
        DecodedAudioFrame::from_native(ffi::audio_sink_take_frame(self.native()))
    }
    pub fn dropped_frames(&self) -> u64 {
        ffi::audio_sink_dropped_frames(self.native())
    }
    pub fn close(&mut self) -> Result<(), PeerError> {
        ffi::close_audio_sink(self.native())
            .then_some(())
            .ok_or_else(|| PeerError {
                kind: PeerErrorKind::Internal,
                message: "failed to detach local audio sink".into(),
            })
    }
}

/// A complete encoded Opus packet after RTP depacketization. `data` holds
/// exactly the Opus packet (RFC 6716), without RTP headers. Duration is
/// `samples_per_channel` at the 48 kHz Opus clock. A negotiated RED or
/// non-Opus payload is not exposed as an Opus packet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedAudioFrame {
    pub data: Vec<u8>,
    pub rtp_timestamp: u32,
    pub ssrc: u32,
    pub payload_type: u8,
    /// Sequence number supplied by the native audio-frame callback, when known.
    pub sequence_number: Option<u16>,
    /// Native received RFC6464 level in -dBov, from 0 to 127. Missing means the
    /// extension was not present.
    pub audio_level_dbov: Option<u8>,
    /// Received RFC6464 V bit, when the audio-level extension is present.
    /// The pinned native incoming frame projects this bit through its `Type`
    /// getter. This is not inferred from PCM or codec speech classification.
    pub voice_activity: Option<bool>,
    pub samples_per_channel: u32,
    /// Native capture microseconds; i64::MIN/i64::MAX retain infinity sentinels.
    pub capture_time_us: Option<i64>,
    /// Native receive microseconds; i64::MIN/i64::MAX retain infinity sentinels.
    pub receive_time_us: Option<i64>,
}

/// Bounded, decoder-free Opus receiver. Attaching this sink selects encoded
/// delivery for its receiver and excludes simultaneous decoded delivery.
pub struct EncodedAudioSink {
    native: cxx::UniquePtr<ffi::NativeEncodedAudioSink>,
    _peer: Rc<PeerInner>,
}

impl EncodedAudioSink {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeEncodedAudioSink>,
        peer: Rc<PeerInner>,
    ) -> Self {
        Self {
            native,
            _peer: peer,
        }
    }

    pub fn try_next_frame(&self) -> Option<EncodedAudioFrame> {
        if self._peer.closed.get() {
            return None;
        }
        let frame = ffi::encoded_audio_sink_take_frame(self.native());
        frame.available.then_some(EncodedAudioFrame {
            data: frame.data,
            rtp_timestamp: frame.rtp_timestamp,
            ssrc: frame.ssrc,
            payload_type: frame.payload_type,
            sequence_number: frame.has_sequence_number.then_some(frame.sequence_number),
            audio_level_dbov: frame.has_audio_level.then_some(frame.audio_level_dbov),
            voice_activity: frame.has_voice_activity.then_some(frame.voice_activity),
            samples_per_channel: frame.samples_per_channel,
            capture_time_us: frame.has_capture_time.then_some(frame.capture_time_us),
            receive_time_us: frame.has_receive_time.then_some(frame.receive_time_us),
        })
    }

    pub fn dropped_frames(&self) -> u64 {
        ffi::encoded_audio_sink_dropped_frames(self.native())
    }

    /// Stop encoded delivery, discard queued output, and restore native
    /// decoding for future packets. Either receive mode may be attached again.
    /// Stop reception before closing if opaque packets must never be decoded.
    /// Idempotent; dropping this closed handle does not close a replacement.
    pub fn close(&mut self) -> Result<(), PeerError> {
        ffi::close_encoded_audio_sink(self.native())
            .then_some(())
            .ok_or_else(|| PeerError {
                kind: PeerErrorKind::Internal,
                message: "failed to close encoded audio sink".into(),
            })
    }

    fn native(&self) -> &ffi::NativeEncodedAudioSink {
        self.native.as_ref().expect("validated encoded audio sink")
    }
}

/// A caller-fed, sequence-bound raw audio source.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::AudioSource>();
/// ```
#[derive(Clone)]
pub struct AudioSource {
    inner: Rc<SourceInner>,
}

struct SourceInner {
    native: cxx::UniquePtr<ffi::NativeAudioSource>,
    _factory: Rc<FactoryInner>,
    closed: Cell<bool>,
}

impl AudioSource {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeAudioSource>,
        factory: Rc<FactoryInner>,
    ) -> Self {
        Self {
            inner: Rc::new(SourceInner {
                native,
                _factory: factory,
                closed: Cell::new(false),
            }),
        }
    }

    /// Actual native source state, including after the producer is closed.
    pub fn state(&self) -> crate::AudioSourceState {
        crate::AudioSourceState::from_native(ffi::audio_source_state(self.native()))
    }
    pub fn is_remote(&self) -> bool {
        ffi::audio_source_remote(self.native())
    }
    /// Current native source options, not the effective shared engine config.
    pub fn options(&self) -> crate::AudioSourceOptions {
        crate::AudioSourceOptions::from_native(ffi::audio_source_options(self.native()))
    }
    /// Invoke native SetOptions on signaling. All input is admitted before
    /// source mutation. Success means the void call returned, not that every
    /// voice channel accepted it or that processing became effective. Use
    /// `options()` for stored source state and factory processing diagnostics
    /// for effective processing. Native channel merge/default behavior remains
    /// distinct from the caller-fed source's replacement of its option value.
    pub fn set_options(
        &self,
        options: &crate::AudioSourceOptions,
    ) -> Result<(), crate::AudioSourceControlError> {
        crate::AudioSourceControlError::result(ffi::audio_source_set_options(
            self.native(),
            &options.ffi(),
        ))
    }
    /// Invoke the native void volume setter. The caller-fed source's pinned
    /// implementation is a no-op: it still reports volume 1.0 and delivers
    /// unchanged PCM. This does not synthesize sample attenuation.
    pub fn set_volume(&self, volume: f64) -> Result<(), crate::AudioSourceControlError> {
        crate::AudioSourceControlError::result(ffi::audio_source_set_volume(self.native(), volume))
    }
    /// Native source volume. The caller-fed source uses the SDK default 1.0.
    pub fn volume(&self) -> f64 {
        ffi::audio_source_volume(self.native())
    }

    /// Read the native capture timeline without advancing time or pumping work.
    /// Custom non-controlled clocks have no established TimeMillis mapping.
    pub fn capture_time_now(&self) -> Result<std::time::Duration, AudioFrameError> {
        if self.inner.closed.get() {
            return Err(AudioFrameError::Released);
        }
        self.inner
            ._factory
            .media_capture_time()
            .ok_or(AudioFrameError::InvalidTimestamp)
    }

    /// Preserve the legacy unstamped source handoff. `frame.timestamp_us` is
    /// not interpreted as an absolute native capture clock.
    pub fn push_frame(&self, frame: &AudioPcmFrame) -> Result<(), AudioFrameError> {
        self.push_frame_with_capture_time(frame, None)
    }

    /// Supply the actual capture instant mapped to `capture_time_now`'s domain.
    /// The native handoff has millisecond precision; wire timing and native
    /// rejection remain engine-owned. No enqueue time is substituted.
    pub fn push_frame_at(
        &self,
        frame: &AudioPcmFrame,
        capture_time: std::time::Duration,
    ) -> Result<(), AudioFrameError> {
        self.capture_time_now()?;
        self.push_frame_with_capture_time(frame, Some(capture_time))
    }

    fn push_frame_with_capture_time(
        &self,
        frame: &AudioPcmFrame,
        capture_time: Option<std::time::Duration>,
    ) -> Result<(), AudioFrameError> {
        if self.inner.closed.get() {
            return Err(AudioFrameError::Released);
        }
        let samples = AudioPcmFrame::validate(
            frame.sample_rate_hz,
            frame.channels,
            frame.timestamp_us,
            frame.samples.len(),
        )?;
        if frame.samples_per_channel != samples {
            return Err(AudioFrameError::InvalidSampleCount);
        }
        let capture_us = capture_time.map_or(Ok(-1), |value| {
            let micros =
                i64::try_from(value.as_micros()).map_err(|_| AudioFrameError::InvalidTimestamp)?;
            // ChannelSend's resume path multiplies the difference between two
            // capture milliseconds by up to 48000 before dividing. All admitted
            // capture values must keep that reached signed product in range.
            if micros / 1000 > i64::MAX / 48000 {
                return Err(AudioFrameError::InvalidTimestamp);
            }
            Ok(micros)
        })?;
        let (mut failure, mut processing_error, mut wrapper_error) = (0, 0, 0);
        if ffi::audio_source_push_pcm(
            self.native(),
            &frame.samples,
            frame.sample_rate_hz,
            frame.channels,
            frame.timestamp_us,
            capture_us,
            &mut failure,
            &mut processing_error,
            &mut wrapper_error,
        ) {
            Ok(())
        } else {
            Err(match failure {
                1 => AudioFrameError::ProcessingFailed(processing_error),
                2 => AudioFrameError::UnsupportedProcessing,
                3 => AudioFrameError::ReentrantCall,
                4 => AudioFrameError::ProcessingAdmission(
                    crate::AudioProcessingModule::admission(wrapper_error)
                        .expect_err("nonzero binding admission code"),
                ),
                _ => AudioFrameError::Released,
            })
        }
    }

    pub fn close(&mut self) -> Result<(), AudioFrameError> {
        if ffi::audio_source_capture_active(self.native()) {
            return Err(AudioFrameError::ReentrantCall);
        }
        if !self.inner.closed.replace(true) && !ffi::close_audio_source(self.native()) {
            return Err(AudioFrameError::Released);
        }
        Ok(())
    }

    pub(crate) fn belongs_to(&self, factory: &Rc<FactoryInner>) -> bool {
        Rc::ptr_eq(&self.inner._factory, factory)
    }

    pub(crate) fn native(&self) -> &ffi::NativeAudioSource {
        self.inner.native.as_ref().expect("validated audio source")
    }
}

/// One RFC 6716 mono Opus packet, without RTP headers. The RTP clock is
/// 48 kHz; supported packet durations are 10, 20, 30, 40, 50, or 60 ms.
/// The packet's TOC duration must agree with `samples_per_channel`.
/// The payload is at most 1200 bytes and must fit in 960 bytes per 10 ms
/// slot, less 36 internal envelope bytes per slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpusInputFrame {
    pub data: Vec<u8>,
    pub rtp_timestamp: u32,
    pub samples_per_channel: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpusInputError {
    InvalidPacket,
    InvalidDuration,
    InvalidTimestamp,
    InvalidAudioLevel,
    Backpressure,
    Released,
}
impl fmt::Display for OpusInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for OpusInputError {}

/// Explicit producer RFC6464 level and voice-activity bit, not derived from
/// packet bytes, carrier PCM, or decoder speech classification. The native
/// sender serializes these only when the audio-level extension is negotiated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpusAudioLevel {
    level_dbov: u8,
    voice_activity: bool,
}
impl OpusAudioLevel {
    /// Level is the positive magnitude in -dBov: 0 is full scale, 127 silence.
    /// Voice activity is independently declared, not inferred from this value.
    pub fn new(level_dbov: u8, voice_activity: bool) -> Result<Self, OpusInputError> {
        if level_dbov > 127 {
            return Err(OpusInputError::InvalidAudioLevel);
        }
        Ok(Self {
            level_dbov,
            voice_activity,
        })
    }

    pub fn level_dbov(self) -> u8 {
        self.level_dbov
    }
    pub fn voice_activity(self) -> bool {
        self.voice_activity
    }
}

/// A sequence-bound Opus input source. Create its factory with
/// [`crate::AudioEncoderFactory::with_opus_frames`]. Frames bypass Opus
/// encoding; each source preserves its configured mono or stereo Opus bytes.
/// Source and passthrough encoder share fixed, private 48 kHz two-channel
/// input blocks, independently of the Opus payload's channel count or the
/// receiver's SDP stereo preference. No PCM channel remix touches the bytes.
/// The factory accepts Opus-only tracks, not raw PCM tracks. At most 24
/// in-flight 10 ms input copies are admitted per source, counting every attached
/// sender. Retry on `Backpressure`. Detachment does not count as consumption;
/// if native input is discarded before encoding, capacity is conservatively
/// retained until source close. No frame is queued without an attached sender.
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::EncodedAudioSource>();
/// ```
pub struct EncodedAudioSource {
    source: AudioSource,
    channels: u8,
    last_timestamp: Cell<Option<u32>>,
}
impl EncodedAudioSource {
    pub(crate) fn from_source(source: AudioSource, channels: u8) -> Self {
        Self {
            source,
            channels,
            last_timestamp: Cell::new(None),
        }
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Read the supported capture timeline without pumping or changing time.
    /// Controlled media uses its world clock; default system-clock production
    /// uses the retained native environment's TimeMillis-compatible clock.
    /// This is a clock read, not evidence of when a packet was captured. Map the
    /// actual capture instant into this domain, not an unrelated clock's epoch.
    /// Custom non-controlled clocks lack a qualified mapping and are rejected.
    pub fn capture_time_now(&self) -> Result<std::time::Duration, OpusInputError> {
        if self.source.inner.closed.get() {
            return Err(OpusInputError::Released);
        }
        self.source
            .inner
            ._factory
            .media_capture_time()
            .ok_or(OpusInputError::InvalidTimestamp)
    }

    /// Supply the capture instant in the timeline returned by capture_time_now.
    /// Controlled media and default system-clock production are supported;
    /// custom non-controlled clock domains remain rejected. This does not map
    /// arbitrary source-clock values or substitute enqueue time for capture.
    /// Native capture metadata is forwarded at millisecond precision; RTP
    /// timestamps remain caller-supplied 48 kHz ticks.
    pub fn push_opus_at(
        &self,
        frame: &OpusInputFrame,
        capture_time: std::time::Duration,
    ) -> Result<(), OpusInputError> {
        self.capture_time_now()?;
        self.push_opus_with_time(frame, Some(capture_time), None)
    }

    /// Use caller-controlled capture time when the selected runtime provides
    /// that mapping; otherwise preserve the timestamp-free native carrier API.
    pub fn push_opus(&self, frame: &OpusInputFrame) -> Result<(), OpusInputError> {
        let capture_time = self.source.inner._factory.controlled_time();
        self.push_opus_with_time(frame, capture_time, None)
    }

    /// Submit an independent producer level/V pair alongside an unchanged Opus
    /// packet. Native encoder speech and the supported outgoing level setter
    /// carry it to native RTP extension serialization. Capture timing follows
    /// the same rules as `push_opus`. Missing negotiation produces no extension.
    pub fn push_opus_with_audio_level(
        &self,
        frame: &OpusInputFrame,
        level: OpusAudioLevel,
    ) -> Result<(), OpusInputError> {
        let capture_time = self.source.inner._factory.controlled_time();
        self.push_opus_with_time(frame, capture_time, Some(level))
    }

    /// Combine a supported explicit capture instant with independent level/V.
    /// See capture_time_now and push_opus_at for clock-domain requirements.
    pub fn push_opus_at_with_audio_level(
        &self,
        frame: &OpusInputFrame,
        capture_time: std::time::Duration,
        level: OpusAudioLevel,
    ) -> Result<(), OpusInputError> {
        self.capture_time_now()?;
        self.push_opus_with_time(frame, Some(capture_time), Some(level))
    }

    fn push_opus_with_time(
        &self,
        frame: &OpusInputFrame,
        capture_time: Option<std::time::Duration>,
        audio_level: Option<OpusAudioLevel>,
    ) -> Result<(), OpusInputError> {
        if self.source.inner.closed.get() {
            return Err(OpusInputError::Released);
        }
        let bytes = &frame.data;
        if bytes.is_empty() || bytes.len() > 1200 || (bytes[0] & 4 != 0) != (self.channels == 2) {
            return Err(OpusInputError::InvalidPacket);
        }
        let config = bytes[0] >> 3;
        let count = match bytes[0] & 3 {
            0 => 1,
            3 => bytes.get(1).map(|b| b & 63).unwrap_or(0) as u32,
            _ => 2,
        };
        let samples = (if config < 12 {
            if config & 3 == 3 {
                2880
            } else {
                480u32 << (config & 3)
            }
        } else if config < 16 {
            480u32 << (config & 1)
        } else {
            120u32 << (config & 3)
        }) * count;
        if samples != frame.samples_per_channel
            || !matches!(samples, 480 | 960 | 1440 | 1920 | 2400 | 2880)
            || bytes.len() > (samples as usize / 480) * (960 * self.channels as usize - 36)
        {
            return Err(OpusInputError::InvalidDuration);
        }
        if let Some(last) = self.last_timestamp.get()
            && (frame.rtp_timestamp.wrapping_sub(last) == 0
                || frame.rtp_timestamp.wrapping_sub(last) >= (1 << 31))
        {
            return Err(OpusInputError::InvalidTimestamp);
        }
        let capture_us = if let Some(time) = capture_time {
            if time > self.capture_time_now()? {
                return Err(OpusInputError::InvalidTimestamp);
            }
            let duration = std::time::Duration::from_micros(
                u64::from(frame.samples_per_channel) * 1_000_000 / 48_000,
            );
            if time
                .checked_add(duration)
                .is_none_or(|end| end > crate::MAX_CONTROLLED_TIME)
            {
                return Err(OpusInputError::InvalidTimestamp);
            }
            i64::try_from(time.as_micros()).map_err(|_| OpusInputError::InvalidTimestamp)?
        } else {
            -1
        };
        if !ffi::audio_source_push_opus_with_level_at(
            self.source.native(),
            bytes,
            frame.rtp_timestamp,
            frame.samples_per_channel,
            capture_us,
            audio_level.is_some(),
            audio_level.map_or(0, OpusAudioLevel::level_dbov),
            audio_level.is_some_and(OpusAudioLevel::voice_activity),
        ) {
            return Err(OpusInputError::Backpressure);
        }
        self.last_timestamp.set(Some(frame.rtp_timestamp));
        Ok(())
    }

    pub fn close(&mut self) -> Result<(), AudioFrameError> {
        self.source.close()
    }

    pub(crate) fn source(&self) -> &AudioSource {
        &self.source
    }
}

/// A sequence-bound local audio track retaining its source and factory.
#[derive(Clone)]
pub struct AudioTrack {
    inner: Rc<TrackInner>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioInputKind {
    Pcm,
    OpusFrames,
}

struct TrackInner {
    native: cxx::UniquePtr<ffi::NativeAudioTrack>,
    _source: Option<Rc<SourceInner>>,
    _peer: Option<Rc<PeerInner>>,
    _factory: Rc<FactoryInner>,
    input_kind: AudioInputKind,
}

impl AudioTrack {
    pub(crate) fn factory_context(&self) -> &Rc<FactoryInner> {
        &self.inner._factory
    }
    pub(crate) fn media_track_owner(&self) -> crate::media_track::MediaTrackOwner {
        // General SDK track references retain source/factory roles, never a
        // received PeerInner that could create a selecting/sending peer cycle.
        // Typed AudioTrack aliases continue to retain their received origin.
        if self.inner._peer.is_some() {
            crate::media_track::MediaTrackOwner::Factory {
                _factory: self.inner._factory.clone(),
            }
        } else {
            crate::media_track::MediaTrackOwner::Audio {
                _track: self.clone(),
            }
        }
    }
    pub fn as_media_track(&self) -> crate::MediaTrack {
        crate::MediaTrack::from_native(
            ffi::audio_track_reference(self.native()),
            self.media_track_owner(),
        )
    }
    pub(crate) fn local(
        native: cxx::UniquePtr<ffi::NativeAudioTrack>,
        factory: Rc<FactoryInner>,
        source: &AudioSource,
    ) -> Self {
        Self {
            inner: Rc::new(TrackInner {
                native,
                _source: Some(source.inner.clone()),
                _peer: None,
                _factory: factory,
                input_kind: AudioInputKind::Pcm,
            }),
        }
    }

    pub(crate) fn received(
        native: cxx::UniquePtr<ffi::NativeAudioTrack>,
        peer: Rc<PeerInner>,
    ) -> Self {
        Self {
            inner: Rc::new(TrackInner {
                native,
                _source: None,
                _factory: peer._factory.clone(),
                _peer: Some(peer),
                input_kind: AudioInputKind::Pcm,
            }),
        }
    }

    #[cfg(feature = "native")]
    pub(crate) fn microphone(
        native: cxx::UniquePtr<ffi::NativeAudioTrack>,
        factory: Rc<FactoryInner>,
    ) -> Self {
        Self {
            inner: Rc::new(TrackInner {
                native,
                _source: None,
                _peer: None,
                _factory: factory,
                input_kind: AudioInputKind::Pcm,
            }),
        }
    }

    pub(crate) fn mark_opus_frames(mut self) -> Self {
        Rc::get_mut(&mut self.inner)
            .expect("new audio track has one owner")
            .input_kind = AudioInputKind::OpusFrames;
        self
    }

    pub(crate) fn is_opus_frames(&self) -> bool {
        self.inner.input_kind == AudioInputKind::OpusFrames
    }

    /// Request upstream AEC, NS and AGC choices for a local PCM track. The
    /// engine is factory-wide: requests from different tracks can conflict.
    /// A stored request does not prove an effect is active; inspect the
    /// factory's `audio_processing_state` after starting capture.
    pub fn set_audio_processing_options(
        &self,
        options: crate::AudioProcessingOptions,
    ) -> Result<(), crate::PeerError> {
        if self.is_opus_frames() {
            return Err(crate::PeerError {
                kind: crate::PeerErrorKind::InvalidParameter,
                message: "encoded Opus bypasses PCM audio processing".into(),
            });
        }
        let mut error = String::new();
        if !ffi::audio_track_set_processing_options(
            self.native(),
            options.echo_cancellation as u8,
            options.noise_suppression as u8,
            options.gain_control as u8,
            &mut error,
        ) {
            return Err(crate::PeerError {
                kind: crate::PeerErrorKind::InvalidParameter,
                message: error,
            });
        }
        Ok(())
    }

    /// Observe actual source samples, rate, channel geometry and optional
    /// capture milliseconds through an owning native sink queue.
    pub fn attach_sink(&self) -> Result<AudioTrackSink, PeerError> {
        if self.is_opus_frames() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "encoded Opus tracks do not expose their private carrier as PCM".into(),
            });
        }
        let native = ffi::audio_track_attach_sink(self.native());
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "audio track has no live native sink attachment".into(),
            });
        }
        Ok(AudioTrackSink {
            native,
            _track: self.inner.clone(),
        })
    }

    pub fn kind(&self) -> String {
        ffi::audio_track_kind(self.native())
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::audio_tracks_same(self.native(), other.native())
    }
    pub fn state(&self) -> crate::AudioTrackState {
        crate::AudioTrackState::from_native(ffi::audio_track_state(self.native()))
    }
    /// The actual native per-track processor, including a native null result.
    pub fn processor(&self) -> Option<crate::AudioTrackProcessor> {
        crate::AudioTrackProcessor::from_native(
            ffi::audio_track_processor(self.native()),
            self.clone(),
        )
    }
    pub fn signal_level(&self) -> Option<i32> {
        let value = ffi::audio_track_signal_level(self.native());
        value.present.then_some(value.value)
    }
    pub fn volume(&self) -> f64 {
        ffi::audio_track_volume(self.native())
    }
    /// Native void setter; successful invocation does not imply that the
    /// underlying source implements gain or changed its reported volume.
    pub fn set_volume(&self, volume: f64) -> Result<(), crate::AudioSourceControlError> {
        crate::AudioSourceControlError::result(ffi::audio_track_set_volume(self.native(), volume))
    }
    pub fn status(&self) -> crate::AudioTrackStatus {
        crate::AudioTrackStatus::from_native(ffi::audio_track_status(self.native()))
            .expect("an owning audio track is present")
    }
    pub fn id(&self) -> String {
        ffi::audio_track_id(self.native())
    }

    pub fn enabled(&self) -> bool {
        ffi::audio_track_enabled(self.native())
    }

    pub fn set_enabled(&self, enabled: bool) -> bool {
        ffi::audio_track_set_enabled(self.native(), enabled)
    }

    // Received tracks retain their peer. Reject actual ownership cycles before
    // native attachment rather than leaking the native peer and role threads.
    pub(crate) fn check_peer_dependency(&self, target: &Rc<PeerInner>) -> Result<(), PeerError> {
        fn reaches(
            peer: &Rc<PeerInner>,
            target: &Rc<PeerInner>,
            seen: &mut std::collections::HashSet<usize>,
        ) -> bool {
            if Rc::ptr_eq(peer, target) {
                return true;
            }
            if !seen.insert(Rc::as_ptr(peer) as usize) {
                return false;
            }
            peer.audio_sender_tracks.borrow().values().any(|track| {
                track
                    .track
                    .inner
                    ._peer
                    .as_ref()
                    .is_some_and(|origin| reaches(origin, target, seen))
            })
        }
        if self
            .inner
            ._peer
            .as_ref()
            .is_some_and(|origin| reaches(origin, target, &mut std::collections::HashSet::new()))
        {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "audio track attachment would create an owning peer dependency cycle"
                    .into(),
            });
        }
        Ok(())
    }

    pub(crate) fn belongs_to(&self, factory: &Rc<FactoryInner>) -> bool {
        Rc::ptr_eq(&self.inner._factory, factory)
    }

    pub(crate) fn native(&self) -> &ffi::NativeAudioTrack {
        self.inner.native.as_ref().expect("validated audio track")
    }
}

impl crate::RtpSender {
    /// Actual native installed audio track, retaining the exact known origin
    /// when its native identity matches, rather than trusting cached state.
    pub fn audio_track(&self) -> Option<AudioTrack> {
        let native = ffi::rtp_sender_audio_track(self.peer.native(), self.native());
        if native.is_null() {
            return None;
        }
        if let Some(track) = self
            .peer
            .audio_sender_tracks
            .borrow()
            .get(&self.resource_key())
            && ffi::audio_tracks_same(
                track.track.native(),
                native.as_ref().expect("native audio track"),
            )
        {
            return Some(track.track.clone());
        }
        if let Some(lease) = self.peer.stream_inputs.borrow().get(&self.resource_key())
            && let Some(track) = lease.track.local_audio()
            && ffi::audio_tracks_same(track.native(), native.as_ref().expect("native audio track"))
        {
            return Some(track.clone());
        }
        Some(AudioTrack::received(native, self.peer.clone()))
    }
    /// Replace or detach the actual native audio track without rebuilding its
    /// sender/transceiver. The accepted track retains its source and factory;
    /// renegotiate separately if the remote track identity must change.
    pub fn set_audio_track(&self, track: Option<&AudioTrack>) -> Result<(), PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        if self.media_type() != crate::MediaType::Audio {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "sender is not audio".into(),
            });
        }
        if let Some(track) = track {
            if !track.belongs_to(&self.peer._factory) {
                return Err(PeerError {
                    kind: PeerErrorKind::InvalidParameter,
                    message: "audio replacement track belongs to another factory".into(),
                });
            }
            track.check_peer_dependency(&self.peer)?;
        }
        let accepted = match track {
            Some(track) => ffi::rtp_sender_set_audio_track(self.native(), track.native()),
            None => ffi::rtp_sender_clear_track(self.native()),
        };
        if !accepted {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "sender rejected the audio track change".into(),
            });
        }
        let mut retained = self.peer.audio_sender_tracks.borrow_mut();
        match track {
            Some(track) => {
                retained.insert(
                    self.resource_key(),
                    crate::peer::SenderTrackLease::new(self, track.clone()),
                );
            }
            None => {
                retained.remove(&self.resource_key());
            }
        }
        drop(retained);
        self.peer.refresh_stream_inputs();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_pcm_capture_milliseconds_preserve_the_full_signed_optional_domain() {
        for capture in [None, Some(0), Some(-1), Some(i64::MIN), Some(i64::MAX)] {
            let frame = DecodedAudioFrame::from_native(ffi::FfiReceivedAudioFrame {
                valid: true,
                sample_rate_hz: 8000,
                channels: 1,
                samples_per_channel: 1,
                has_capture_time: capture.is_some(),
                capture_time_ms: capture.unwrap_or(13),
                samples: vec![7],
            })
            .unwrap();
            assert_eq!(frame.capture_time_ms, capture);
            assert_eq!(
                frame.capture_time_us,
                capture.and_then(|value| value.checked_mul(1000))
            );
            assert_eq!(frame.samples, [7]);
        }
    }

    #[test]
    fn pcm_frame_requires_exact_ten_millisecond_interleaved_block() {
        for rate in [100, 8_000, 16_000, 32_000, 44_100, 48_000] {
            for channels in [1, 2, 4, 6, 8] {
                let data = vec![0; (rate / 100 * channels as u32) as usize];
                let frame =
                    AudioPcmFrame::i16_interleaved(rate, channels, 42, data.clone()).unwrap();
                assert_eq!(frame.samples_per_channel, rate / 100);
                assert_eq!(frame.samples, data);
            }
        }
        assert_eq!(
            AudioPcmFrame::i16_interleaved(48_001, 1, 0, vec![]),
            Err(AudioFrameError::InvalidSampleRate)
        );
        assert_eq!(
            AudioPcmFrame::i16_interleaved(48_000, 3, 0, vec![]),
            Err(AudioFrameError::InvalidChannels)
        );
        assert_eq!(
            AudioPcmFrame::i16_interleaved(48_000, 1, -1, vec![0; 480]),
            Err(AudioFrameError::InvalidTimestamp)
        );
        assert_eq!(
            AudioPcmFrame::i16_interleaved(48_000, 1, 0, vec![0; 479]),
            Err(AudioFrameError::InvalidSampleCount)
        );
        assert_eq!(
            AudioPcmFrame::i16_interleaved(48_000, 1, 0, vec![0; 481]),
            Err(AudioFrameError::InvalidSampleCount)
        );
    }
}
