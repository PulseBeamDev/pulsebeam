//! The complete owning native AudioFrame, separate from PCM convenience values.
use crate::{RtpReceivedPacketInfos, ffi};
use cxx::UniquePtr;
use std::{cell::Cell, fmt, marker::PhantomData};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFrameValueError {
    InvalidShape,
    InvalidSampleRate,
    InvalidEnumRepresentation,
    InvalidLayoutIndex,
    DataTooShort,
    /// A borrowed SDK frame does not prove initialization of the requested span.
    UninitializedStorage,
}
impl fmt::Display for AudioFrameValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidShape => "audio frame dimensions exceed storage or this operation's native shape domain",
            Self::InvalidSampleRate => "sample rate is outside this native operation's domain",
            Self::InvalidEnumRepresentation => "value exceeds the unscoped native enum representation",
            Self::InvalidLayoutIndex => "layout/channel is outside the native lookup domain or disagrees with the channel count",
            Self::DataTooShort => "input does not contain the requested interleaved samples",
            Self::UninitializedStorage => "borrowed native sample storage is not proven initialized",
        })
    }
}
impl std::error::Error for AudioFrameValueError {}

/// Integer-backed values also retain unnamed but representable native states.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct AudioFrameSpeechType(pub i32);
impl AudioFrameSpeechType {
    pub const NORMAL_SPEECH: Self = Self(0);
    pub const PLC: Self = Self(1);
    pub const CNG: Self = Self(2);
    pub const PLC_CNG: Self = Self(3);
    pub const UNDEFINED: Self = Self(4);
    pub const CODEC_PLC: Self = Self(5);
}
impl Default for AudioFrameSpeechType {
    fn default() -> Self {
        Self::UNDEFINED
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct AudioFrameVadActivity(pub i32);
impl AudioFrameVadActivity {
    pub const ACTIVE: Self = Self(0);
    pub const PASSIVE: Self = Self(1);
    pub const UNKNOWN: Self = Self(2);
}
impl Default for AudioFrameVadActivity {
    fn default() -> Self {
        Self::UNKNOWN
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct ChannelLayout(pub i32);
impl ChannelLayout {
    pub const NONE: Self = Self(0);
    pub const UNSUPPORTED: Self = Self(1);
    pub const MONO: Self = Self(2);
    pub const STEREO: Self = Self(3);
    pub const TWO_ONE: Self = Self(4);
    pub const SURROUND: Self = Self(5);
    pub const FOUR_ZERO: Self = Self(6);
    pub const TWO_TWO: Self = Self(7);
    pub const QUAD: Self = Self(8);
    pub const FIVE_ZERO: Self = Self(9);
    pub const FIVE_ONE: Self = Self(10);
    pub const FIVE_ZERO_BACK: Self = Self(11);
    pub const FIVE_ONE_BACK: Self = Self(12);
    pub const SEVEN_ZERO: Self = Self(13);
    pub const SEVEN_ONE: Self = Self(14);
    pub const SEVEN_ONE_WIDE: Self = Self(15);
    pub const STEREO_DOWNMIX: Self = Self(16);
    pub const TWO_POINT_ONE: Self = Self(17);
    pub const THREE_ONE: Self = Self(18);
    pub const FOUR_ONE: Self = Self(19);
    pub const SIX_ZERO: Self = Self(20);
    pub const SIX_ZERO_FRONT: Self = Self(21);
    pub const HEXAGONAL: Self = Self(22);
    pub const SIX_ONE: Self = Self(23);
    pub const SIX_ONE_BACK: Self = Self(24);
    pub const SIX_ONE_FRONT: Self = Self(25);
    pub const SEVEN_ZERO_FRONT: Self = Self(26);
    pub const SEVEN_ONE_WIDE_BACK: Self = Self(27);
    pub const OCTAGONAL: Self = Self(28);
    pub const DISCRETE: Self = Self(29);
    pub const STEREO_AND_KEYBOARD_MIC: Self = Self(30);
    pub const FOUR_ONE_QUAD_SIDE: Self = Self(31);
    pub const BITSTREAM: Self = Self(32);
    pub const MAX: Self = Self::BITSTREAM;
    fn checked_index(self) -> Result<i32, AudioFrameValueError> {
        if !(0..=32).contains(&self.0) {
            return Err(AudioFrameValueError::InvalidLayoutIndex);
        }
        Ok(self.0)
    }
    pub fn channel_count(self) -> Result<usize, AudioFrameValueError> {
        Ok(ffi::media_channel_layout_count(self.checked_index()?) as usize)
    }
    pub fn channel_order(self, channel: AudioChannel) -> Result<i32, AudioFrameValueError> {
        self.checked_index()?;
        if !(0..=10).contains(&channel.0) {
            return Err(AudioFrameValueError::InvalidLayoutIndex);
        }
        Ok(ffi::media_channel_order(self.0, channel.0))
    }
    pub fn as_string(self) -> Result<String, AudioFrameValueError> {
        Ok(ffi::media_channel_layout_string(self.checked_index()?))
    }
    pub fn guess(channels: i32) -> Self {
        Self(ffi::media_channel_layout_guess(channels))
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct AudioChannel(pub i32);
impl AudioChannel {
    pub const LEFT: Self = Self(0);
    pub const RIGHT: Self = Self(1);
    pub const CENTER: Self = Self(2);
    pub const LFE: Self = Self(3);
    pub const BACK_LEFT: Self = Self(4);
    pub const BACK_RIGHT: Self = Self(5);
    pub const LEFT_OF_CENTER: Self = Self(6);
    pub const RIGHT_OF_CENTER: Self = Self(7);
    pub const BACK_CENTER: Self = Self(8);
    pub const SIDE_LEFT: Self = Self(9);
    pub const SIDE_RIGHT: Self = Self(10);
    pub const MAX: Self = Self::SIDE_RIGHT;
}

/// Exact owned snapshot of the SDK's public writable fields. Private layout,
/// mute and absolute-capture timestamp state use their native operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioFrameFields {
    pub timestamp: u32,
    pub elapsed_time_ms: i64,
    pub ntp_time_ms: i64,
    pub samples_per_channel: usize,
    pub sample_rate_hz: i32,
    pub num_channels: usize,
    pub speech_type: AudioFrameSpeechType,
    pub vad_activity: AudioFrameVadActivity,
    pub packet_infos: RtpReceivedPacketInfos,
}
impl Default for AudioFrameFields {
    fn default() -> Self {
        Self {
            timestamp: 0,
            elapsed_time_ms: -1,
            ntp_time_ms: -1,
            samples_per_channel: 0,
            sample_rate_hz: 0,
            num_channels: 0,
            speech_type: AudioFrameSpeechType::UNDEFINED,
            vad_activity: AudioFrameVadActivity::UNKNOWN,
            packet_infos: Default::default(),
        }
    }
}
impl AudioFrameFields {
    pub(crate) fn to_native(&self) -> ffi::FfiAudioFrameFields {
        ffi::FfiAudioFrameFields {
            timestamp: self.timestamp,
            elapsed_time_ms: self.elapsed_time_ms,
            ntp_time_ms: self.ntp_time_ms,
            samples_per_channel: self.samples_per_channel,
            sample_rate_hz: self.sample_rate_hz,
            num_channels: self.num_channels,
            speech_type: self.speech_type.0,
            vad_activity: self.vad_activity.0,
            packet_infos: self.packet_infos.to_native(),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiAudioFrameFields) -> Self {
        Self {
            timestamp: value.timestamp,
            elapsed_time_ms: value.elapsed_time_ms,
            ntp_time_ms: value.ntp_time_ms,
            samples_per_channel: value.samples_per_channel,
            sample_rate_hz: value.sample_rate_hz,
            num_channels: value.num_channels,
            speech_type: AudioFrameSpeechType(value.speech_type),
            vad_activity: AudioFrameVadActivity(value.vad_activity),
            packet_infos: RtpReceivedPacketInfos::from_native(value.packet_infos),
        }
    }
}

/// Independently owned native frame and all 7680 backing samples. The complete
/// backing storage is initialized once, so legal shape changes cannot expose
/// uninitialized memory. Mute, reset and CopyFrom retain native semantics.
///
/// This resource is Send but not Sync. Its mutable native operations require
/// exclusive Rust access; views borrow the owner and cannot survive mutation.
/// A source frame whose public fields do not satisfy the SDK's view domain can
/// still expose its fields and full fixed-capacity data, but not a data_view or
/// CopyFrom. Such failures do not assert valid GetAudio output.
pub struct AudioFrame {
    pub(crate) native: UniquePtr<ffi::NativeAudioFrame>,
    not_sync: PhantomData<Cell<()>>,
}
// No sequence-bound members or shared mutation are exported. Packet metadata
// and the fixed sample storage are owned by this unique frame.
unsafe impl Send for AudioFrame {}
impl Default for AudioFrame {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Debug for AudioFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioFrame")
            .field("fields", &self.fields())
            .field("muted", &self.muted())
            .field("channel_layout", &self.channel_layout())
            .field(
                "absolute_capture_timestamp_ms",
                &self.absolute_capture_timestamp_ms(),
            )
            .finish_non_exhaustive()
    }
}
impl AudioFrame {
    pub const MAX_DATA_SIZE_SAMPLES: usize = 7680;
    pub const MAX_DATA_SIZE_BYTES: usize = 15360;
    pub const MAX_CHANNELS: usize = 16;
    pub const DEFAULT_BUFFER_LENGTH_MS: usize = 10;
    pub const DEFAULT_BUFFERS_PER_SEC: usize = 100;
    pub fn new() -> Self {
        Self::from_native(ffi::media_audio_frame_new())
    }
    pub fn with_format(
        sample_rate_hz: i32,
        num_channels: usize,
        layout: ChannelLayout,
    ) -> Result<Self, AudioFrameValueError> {
        if !(100..=192000).contains(&sample_rate_hz) {
            return Err(AudioFrameValueError::InvalidSampleRate);
        }
        if !(0..=63).contains(&layout.0) {
            return Err(AudioFrameValueError::InvalidEnumRepresentation);
        }
        if num_channels > Self::MAX_CHANNELS {
            return Err(AudioFrameValueError::InvalidShape);
        }
        capacity_shape(sample_rate_hz as usize / 100, num_channels)?;
        let value = ffi::media_audio_frame_with_format(sample_rate_hz, num_channels, layout.0);
        if value.is_null() {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(Self::from_native(value))
    }
    pub fn sample_rate_to_default_channel_size(
        sample_rate: usize,
    ) -> Result<usize, AudioFrameValueError> {
        if sample_rate > 192000 {
            return Err(AudioFrameValueError::InvalidSampleRate);
        }
        Ok(ffi::media_audio_default_channel_size(sample_rate))
    }
    pub(crate) fn from_native(native: UniquePtr<ffi::NativeAudioFrame>) -> Self {
        assert!(
            !native.is_null(),
            "native audio frame owner must not be null"
        );
        Self {
            native,
            not_sync: PhantomData,
        }
    }
    pub(crate) fn inner(&self) -> &ffi::NativeAudioFrame {
        self.native.as_ref().expect("live native audio frame")
    }
    /// Exclusively borrow this initialized owner without transferring it.
    pub fn as_mut(&mut self) -> crate::AudioFrameMut<'_> {
        crate::AudioFrameMut::from_initialized(self.native.pin_mut())
    }
    pub(crate) fn native_mut(&mut self) -> std::pin::Pin<&mut ffi::NativeAudioFrame> {
        self.native.pin_mut()
    }
    pub fn fields(&self) -> AudioFrameFields {
        AudioFrameFields::from_native(ffi::media_audio_frame_fields(self.inner()))
    }
    /// Assigns all public fields, without changing private layout, capture or
    /// mute state. Capacity and enum-representation guards are independent of
    /// view-only checks; no known speech/VAD code whitelist is imposed.
    pub fn set_fields(&mut self, fields: &AudioFrameFields) -> Result<(), AudioFrameValueError> {
        capacity_shape(fields.samples_per_channel, fields.num_channels)?;
        enums(fields.speech_type, fields.vad_activity)?;
        if !ffi::media_audio_frame_set_fields(self.native.pin_mut(), &fields.to_native()) {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(())
    }
    pub fn reset(&mut self) {
        ffi::media_audio_frame_reset(self.native.pin_mut(), true);
    }
    pub fn reset_without_muting(&mut self) {
        ffi::media_audio_frame_reset(self.native.pin_mut(), false);
    }
    /// `None` calls the null-data native overload path, muting the frame. A
    /// supplied slice must cover the requested active samples; extra samples
    /// are not copied. Even Some(empty) remains distinct from None.
    #[allow(clippy::too_many_arguments)]
    pub fn update_frame(
        &mut self,
        timestamp: u32,
        data: Option<&[i16]>,
        samples_per_channel: usize,
        sample_rate_hz: i32,
        speech_type: AudioFrameSpeechType,
        vad_activity: AudioFrameVadActivity,
        num_channels: usize,
    ) -> Result<(), AudioFrameValueError> {
        let length = capacity_shape(samples_per_channel, num_channels)?;
        if num_channels > Self::MAX_CHANNELS {
            return Err(AudioFrameValueError::InvalidShape);
        }
        enums(speech_type, vad_activity)?;
        if data.is_some_and(|value| value.len() < length) {
            return Err(AudioFrameValueError::DataTooShort);
        }
        if !ffi::media_audio_frame_update(
            self.native.pin_mut(),
            timestamp,
            data.unwrap_or(&[]),
            data.is_some(),
            samples_per_channel,
            sample_rate_hz,
            speech_type.0,
            vad_activity.0,
            num_channels,
        ) {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(())
    }
    pub fn copy_from(&mut self, source: &Self) -> Result<(), AudioFrameValueError> {
        if !ffi::media_audio_frame_copy(self.native.pin_mut(), source.inner()) {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(())
    }
    pub fn try_clone(&self) -> Result<Self, AudioFrameValueError> {
        let mut copy = Self::new();
        copy.copy_from(self)?;
        Ok(copy)
    }
    /// Full fixed-capacity native data pointer, safely represented as a slice.
    /// A muted frame returns 7680 zero samples regardless of active dimensions.
    pub fn data(&self) -> &[i16] {
        ffi::media_audio_frame_data(self.inner())
    }
    /// Active interleaved samples. The shape checks protect both the native
    /// span constructor and its development assertions.
    pub fn data_view(&self) -> Result<&[i16], AudioFrameValueError> {
        let mut valid = false;
        let data = ffi::media_audio_frame_data_view(self.inner(), &mut valid);
        if !valid {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(data)
    }
    /// Full writable fixed-capacity backing buffer. Native mute-clearing zeros
    /// the entire buffer on the first write while muted, not just the active shape.
    pub fn mutable_data(&mut self) -> &mut [i16] {
        ffi::media_audio_frame_mutable_data(self.native.pin_mut())
    }
    /// The native shape-taking overload changes only the two shape fields,
    /// zeroes the active prefix if muted, and does not guess a channel layout.
    pub fn mutable_data_with_shape(
        &mut self,
        samples_per_channel: usize,
        num_channels: usize,
    ) -> Result<&mut [i16], AudioFrameValueError> {
        capacity_shape(samples_per_channel, num_channels)?;
        if num_channels > Self::MAX_CHANNELS || num_channels > samples_per_channel {
            return Err(AudioFrameValueError::InvalidShape);
        }
        let mut valid = false;
        let data = ffi::media_audio_frame_mutable_shape(
            self.native.pin_mut(),
            samples_per_channel,
            num_channels,
            &mut valid,
        );
        if !valid {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(data)
    }
    pub fn mute(&mut self) {
        ffi::media_audio_frame_mute(self.native.pin_mut());
    }
    pub fn muted(&self) -> bool {
        ffi::media_audio_frame_muted(self.inner())
    }
    pub fn max_16bit_samples(&self) -> usize {
        ffi::media_audio_frame_max_samples(self.inner())
    }
    pub fn samples_per_channel(&self) -> usize {
        ffi::media_audio_frame_samples_per_channel(self.inner())
    }
    pub fn num_channels(&self) -> usize {
        ffi::media_audio_frame_num_channels(self.inner())
    }
    pub fn channel_layout(&self) -> ChannelLayout {
        ChannelLayout(ffi::media_audio_frame_layout(self.inner()))
    }
    pub fn sample_rate_hz(&self) -> i32 {
        ffi::media_audio_frame_sample_rate(self.inner())
    }
    pub fn set_layout_and_num_channels(
        &mut self,
        layout: ChannelLayout,
        num_channels: usize,
    ) -> Result<(), AudioFrameValueError> {
        let expected = layout.channel_count()?;
        if expected != 0 && expected != num_channels {
            return Err(AudioFrameValueError::InvalidLayoutIndex);
        }
        capacity_shape(self.samples_per_channel(), num_channels)?;
        if !ffi::media_audio_frame_set_layout(self.native.pin_mut(), layout.0, num_channels) {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(())
    }
    pub fn absolute_capture_timestamp_ms(&self) -> Option<i64> {
        let mut present = false;
        let value = ffi::media_audio_frame_capture_timestamp(self.inner(), &mut present);
        present.then_some(value)
    }
    pub fn set_absolute_capture_timestamp_ms(&mut self, timestamp: i64) {
        ffi::media_audio_frame_set_capture_timestamp(self.native.pin_mut(), timestamp);
    }
    pub fn set_sample_rate_and_channel_size(
        &mut self,
        sample_rate: i32,
    ) -> Result<(), AudioFrameValueError> {
        if !(0..=192000).contains(&sample_rate) {
            return Err(AudioFrameValueError::InvalidSampleRate);
        }
        capacity_shape(sample_rate as usize / 100, self.num_channels())?;
        if !ffi::media_audio_frame_set_sample_rate(self.native.pin_mut(), sample_rate) {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(())
    }
}
pub(crate) fn capacity_shape(
    samples: usize,
    channels: usize,
) -> Result<usize, AudioFrameValueError> {
    let size = samples
        .checked_mul(channels)
        .ok_or(AudioFrameValueError::InvalidShape)?;
    if size > AudioFrame::MAX_DATA_SIZE_SAMPLES {
        return Err(AudioFrameValueError::InvalidShape);
    }
    Ok(size)
}
pub(crate) fn enums(
    speech: AudioFrameSpeechType,
    vad: AudioFrameVadActivity,
) -> Result<(), AudioFrameValueError> {
    if !(0..=7).contains(&speech.0) || !(0..=3).contains(&vad.0) {
        return Err(AudioFrameValueError::InvalidEnumRepresentation);
    }
    Ok(())
}
