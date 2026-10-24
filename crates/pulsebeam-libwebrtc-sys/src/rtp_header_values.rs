//! Lossless native RTP header values, not an RTP wire encoder.
use crate::{AbsoluteCaptureTime, TimeDelta, Timestamp, ffi};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtpValueError {
    InvalidAudioLevel,
    InvalidAbsoluteSendTime,
    /// Unscoped C++ rotation enums cannot represent arbitrary integers safely.
    InvalidRotation,
}
impl std::fmt::Display for RtpValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidAudioLevel => "RTP audio level must be in 0..=127",
            Self::InvalidAbsoluteSendTime => {
                "absolute send time must be present and less than 2^24"
            }
            Self::InvalidRotation => "rotation exceeds the native enum representation",
        })
    }
}
impl std::error::Error for RtpValueError {}

/// Checked native header audio level. Receive-packet info instead has an
/// optional unrestricted uint8 level, so the two values are intentionally distinct.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioLevel {
    voice_activity: bool,
    level: i32,
}
impl AudioLevel {
    pub fn new(voice_activity: bool, level: i32) -> Result<Self, RtpValueError> {
        if !(0..=127).contains(&level) {
            return Err(RtpValueError::InvalidAudioLevel);
        }
        Ok(Self {
            voice_activity,
            level,
        })
    }
    pub fn voice_activity(self) -> bool {
        self.voice_activity
    }
    pub fn level(self) -> i32 {
        self.level
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpFeedbackRequest {
    pub include_timestamps: bool,
    pub sequence_count: i32,
}

/// All timing fields, including values when the enclosing presence flag is false.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoSendTiming {
    pub encode_start_delta_ms: u16,
    pub encode_finish_delta_ms: u16,
    pub packetization_finish_delta_ms: u16,
    pub pacer_exit_delta_ms: u16,
    pub network_timestamp_delta_ms: u16,
    pub network2_timestamp_delta_ms: u16,
    pub flags: u8,
}
impl VideoSendTiming {
    /// Native capped millisecond difference. Only signed subtraction overflow
    /// is rejected; negative and oversized differences retain native saturation.
    pub fn delta_capped_ms(base_ms: i64, time_ms: i64) -> Result<u16, crate::TimeValueError> {
        time_ms
            .checked_sub(base_ms)
            .ok_or(crate::TimeValueError::OutOfRange)?;
        Ok(ffi::media_video_timing_difference(base_ms, time_ms))
    }
    /// Native duration overload, including pinned millisecond rounding.
    pub fn duration_capped_ms(delta: TimeDelta) -> Result<u16, crate::TimeValueError> {
        delta.millis()?;
        Ok(ffi::media_video_timing_duration(
            delta.native_microsecond_bits(),
        ))
    }
}
impl Default for VideoSendTiming {
    fn default() -> Self {
        Self {
            encode_start_delta_ms: 0,
            encode_finish_delta_ms: 0,
            packetization_finish_delta_ms: 0,
            pacer_exit_delta_ms: 0,
            network_timestamp_delta_ms: 0,
            network2_timestamp_delta_ms: 0,
            flags: u8::MAX,
        }
    }
}

/// Native limits preserve microseconds, not just RTP's 10-ms wire granularity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoPlayoutDelay {
    minimum: TimeDelta,
    maximum: TimeDelta,
}
impl Default for VideoPlayoutDelay {
    fn default() -> Self {
        Self {
            minimum: TimeDelta::zero(),
            maximum: Self::MAXIMUM,
        }
    }
}
impl VideoPlayoutDelay {
    pub const MAXIMUM: TimeDelta = TimeDelta::from_native_microsecond_bits(40_950_000);
    /// Native constructor clamps invalid limits; `set` instead rejects them.
    pub fn new(minimum: TimeDelta, maximum: TimeDelta) -> Self {
        let value = ffi::media_playout_delay_new(
            minimum.native_microsecond_bits(),
            maximum.native_microsecond_bits(),
        );
        Self::from_native(value)
    }
    pub fn minimal() -> Self {
        Self {
            minimum: TimeDelta::zero(),
            maximum: TimeDelta::zero(),
        }
    }
    pub fn minimum(self) -> TimeDelta {
        self.minimum
    }
    pub fn maximum(self) -> TimeDelta {
        self.maximum
    }
    pub fn set(&mut self, minimum: TimeDelta, maximum: TimeDelta) -> bool {
        let mut value = self.to_native();
        if !ffi::media_playout_delay_set(
            &mut value,
            minimum.native_microsecond_bits(),
            maximum.native_microsecond_bits(),
        ) {
            return false;
        }
        *self = Self::from_native(value);
        true
    }
    pub(crate) fn to_native(self) -> ffi::FfiMediaPlayoutDelay {
        ffi::FfiMediaPlayoutDelay {
            minimum_us: self.minimum.native_microsecond_bits(),
            maximum_us: self.maximum.native_microsecond_bits(),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiMediaPlayoutDelay) -> Self {
        Self {
            minimum: TimeDelta::from_native_microsecond_bits(value.minimum_us),
            maximum: TimeDelta::from_native_microsecond_bits(value.maximum_us),
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HdrChromaticity {
    pub x: f32,
    pub y: f32,
}
impl HdrChromaticity {
    pub fn validate(self) -> bool {
        self.x >= 0.0 && self.x <= 1.0 && self.y >= 0.0 && self.y <= 1.0
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HdrMasteringMetadata {
    pub primary_r: HdrChromaticity,
    pub primary_g: HdrChromaticity,
    pub primary_b: HdrChromaticity,
    pub white_point: HdrChromaticity,
    pub luminance_max: f32,
    pub luminance_min: f32,
}
impl HdrMasteringMetadata {
    pub fn validate(self) -> bool {
        self.luminance_max >= 0.0
            && self.luminance_max <= 20_000.0
            && self.luminance_min >= 0.0
            && self.luminance_min <= 5.0
            && self.primary_r.validate()
            && self.primary_g.validate()
            && self.primary_b.validate()
            && self.white_point.validate()
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HdrMetadata {
    pub mastering_metadata: HdrMasteringMetadata,
    pub max_content_light_level: i32,
    pub max_frame_average_light_level: i32,
}
impl HdrMetadata {
    pub fn validate(self) -> bool {
        ffi::media_hdr_validate(&self.to_native())
    }
    fn to_native(self) -> ffi::FfiMediaHdrMetadata {
        let m = self.mastering_metadata;
        ffi::FfiMediaHdrMetadata {
            primary_r_x: m.primary_r.x,
            primary_r_y: m.primary_r.y,
            primary_g_x: m.primary_g.x,
            primary_g_y: m.primary_g.y,
            primary_b_x: m.primary_b.x,
            primary_b_y: m.primary_b.y,
            white_point_x: m.white_point.x,
            white_point_y: m.white_point.y,
            luminance_max: m.luminance_max,
            luminance_min: m.luminance_min,
            max_content_light_level: self.max_content_light_level,
            max_frame_average_light_level: self.max_frame_average_light_level,
        }
    }
    fn from_native(v: ffi::FfiMediaHdrMetadata) -> Self {
        Self {
            mastering_metadata: HdrMasteringMetadata {
                primary_r: HdrChromaticity {
                    x: v.primary_r_x,
                    y: v.primary_r_y,
                },
                primary_g: HdrChromaticity {
                    x: v.primary_g_x,
                    y: v.primary_g_y,
                },
                primary_b: HdrChromaticity {
                    x: v.primary_b_x,
                    y: v.primary_b_y,
                },
                white_point: HdrChromaticity {
                    x: v.white_point_x,
                    y: v.white_point_y,
                },
                luminance_max: v.luminance_max,
                luminance_min: v.luminance_min,
            },
            max_content_light_level: v.max_content_light_level,
            max_frame_average_light_level: v.max_frame_average_light_level,
        }
    }
}
/// Complete ColorSpace ownership. Identifier widths follow the native enum
/// underlying types, including int-width range and chroma siting. Raw constructor
/// values and invalid HDR are retained; validated uint8 setters remain separate.
#[derive(Clone, Debug, PartialEq)]
pub struct ColorSpace {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub range: i32,
    pub chroma_siting_horizontal: i32,
    pub chroma_siting_vertical: i32,
    pub hdr_metadata: Option<HdrMetadata>,
}
impl Default for ColorSpace {
    fn default() -> Self {
        Self {
            primaries: 2,
            transfer: 2,
            matrix: 2,
            range: 0,
            chroma_siting_horizontal: 0,
            chroma_siting_vertical: 0,
            hdr_metadata: None,
        }
    }
}
impl ColorSpace {
    pub fn as_string(&self) -> String {
        ffi::media_color_space_string(&self.to_native())
    }
    fn set_from_uint8(&mut self, field: u8, candidate: u8) -> bool {
        let mut value = self.to_native();
        if !ffi::media_color_space_set(&mut value, field, candidate) {
            return false;
        }
        *self = Self::from_native(value);
        true
    }
    pub fn set_primaries_from_uint8(&mut self, candidate: u8) -> bool {
        self.set_from_uint8(0, candidate)
    }
    pub fn set_transfer_from_uint8(&mut self, candidate: u8) -> bool {
        self.set_from_uint8(1, candidate)
    }
    pub fn set_matrix_from_uint8(&mut self, candidate: u8) -> bool {
        self.set_from_uint8(2, candidate)
    }
    pub fn set_range_from_uint8(&mut self, candidate: u8) -> bool {
        self.set_from_uint8(3, candidate)
    }
    pub fn set_chroma_siting_horizontal_from_uint8(&mut self, candidate: u8) -> bool {
        self.set_from_uint8(4, candidate)
    }
    pub fn set_chroma_siting_vertical_from_uint8(&mut self, candidate: u8) -> bool {
        self.set_from_uint8(5, candidate)
    }
    pub(crate) fn to_native(&self) -> ffi::FfiMediaColorSpace {
        ffi::FfiMediaColorSpace {
            primaries: self.primaries,
            transfer: self.transfer,
            matrix: self.matrix,
            range: self.range,
            chroma_siting_horizontal: self.chroma_siting_horizontal,
            chroma_siting_vertical: self.chroma_siting_vertical,
            has_hdr_metadata: self.hdr_metadata.is_some(),
            hdr_metadata: self.hdr_metadata.unwrap_or_default().to_native(),
        }
    }
    pub(crate) fn from_native(v: ffi::FfiMediaColorSpace) -> Self {
        Self {
            primaries: v.primaries,
            transfer: v.transfer,
            matrix: v.matrix,
            range: v.range,
            chroma_siting_horizontal: v.chroma_siting_horizontal,
            chroma_siting_vertical: v.chroma_siting_vertical,
            hdr_metadata: v
                .has_hdr_metadata
                .then(|| HdrMetadata::from_native(v.hdr_metadata)),
        }
    }
}

/// Native presence flags remain independent of their stored values. Strings
/// retain arbitrary bytes, not only UTF-8; this is not a wire-format model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RtpHeaderExtensionValues {
    pub has_transmission_time_offset: bool,
    pub transmission_time_offset: i32,
    pub has_absolute_send_time: bool,
    pub absolute_send_time: u32,
    pub absolute_capture_time: Option<AbsoluteCaptureTime>,
    pub has_transport_sequence_number: bool,
    pub transport_sequence_number: u16,
    pub feedback_request: Option<RtpFeedbackRequest>,
    pub audio_level: Option<AudioLevel>,
    pub has_video_rotation: bool,
    /// Native enum representation, not a closed four-rotation Rust enum.
    pub video_rotation: i32,
    pub has_video_content_type: bool,
    pub video_content_type: u8,
    pub has_video_timing: bool,
    pub video_timing: VideoSendTiming,
    pub playout_delay: VideoPlayoutDelay,
    pub stream_id: Vec<u8>,
    pub repaired_stream_id: Vec<u8>,
    pub mid: Vec<u8>,
    pub color_space: Option<ColorSpace>,
}
impl RtpHeaderExtensionValues {
    pub fn absolute_send_timestamp(&self) -> Result<Timestamp, RtpValueError> {
        if !self.has_absolute_send_time || self.absolute_send_time >= 1 << 24 {
            return Err(RtpValueError::InvalidAbsoluteSendTime);
        }
        Ok(Timestamp::from_native_microsecond_bits(
            ffi::media_absolute_send_timestamp(self.absolute_send_time),
        ))
    }
    fn to_native(&self) -> ffi::FfiMediaRtpHeaderExtension {
        ffi::FfiMediaRtpHeaderExtension {
            has_transmission_time_offset: self.has_transmission_time_offset,
            transmission_time_offset: self.transmission_time_offset,
            has_absolute_send_time: self.has_absolute_send_time,
            absolute_send_time: self.absolute_send_time,
            has_absolute_capture_time: self.absolute_capture_time.is_some(),
            absolute_capture_time: self
                .absolute_capture_time
                .unwrap_or(AbsoluteCaptureTime {
                    absolute_capture_timestamp: 0,
                    estimated_capture_clock_offset: None,
                })
                .to_native(),
            has_transport_sequence_number: self.has_transport_sequence_number,
            transport_sequence_number: self.transport_sequence_number,
            has_feedback_request: self.feedback_request.is_some(),
            feedback_include_timestamps: self
                .feedback_request
                .is_some_and(|v| v.include_timestamps),
            feedback_sequence_count: self.feedback_request.map_or(0, |v| v.sequence_count),
            has_audio_level: self.audio_level.is_some(),
            audio_voice_activity: self.audio_level.is_some_and(|v| v.voice_activity),
            audio_level: self.audio_level.map_or(0, |v| v.level),
            has_video_rotation: self.has_video_rotation,
            video_rotation: self.video_rotation,
            has_video_content_type: self.has_video_content_type,
            video_content_type: self.video_content_type,
            has_video_timing: self.has_video_timing,
            video_timing: ffi::FfiMediaVideoSendTiming {
                flags: self.video_timing.flags,
                encode_start_delta_ms: self.video_timing.encode_start_delta_ms,
                encode_finish_delta_ms: self.video_timing.encode_finish_delta_ms,
                packetization_finish_delta_ms: self.video_timing.packetization_finish_delta_ms,
                pacer_exit_delta_ms: self.video_timing.pacer_exit_delta_ms,
                network_timestamp_delta_ms: self.video_timing.network_timestamp_delta_ms,
                network2_timestamp_delta_ms: self.video_timing.network2_timestamp_delta_ms,
            },
            playout_delay: self.playout_delay.to_native(),
            stream_id: self.stream_id.clone(),
            repaired_stream_id: self.repaired_stream_id.clone(),
            mid: self.mid.clone(),
            has_color_space: self.color_space.is_some(),
            color_space: self.color_space.clone().unwrap_or_default().to_native(),
        }
    }
    fn from_native(v: ffi::FfiMediaRtpHeaderExtension) -> Self {
        Self {
            has_transmission_time_offset: v.has_transmission_time_offset,
            transmission_time_offset: v.transmission_time_offset,
            has_absolute_send_time: v.has_absolute_send_time,
            absolute_send_time: v.absolute_send_time,
            absolute_capture_time: v
                .has_absolute_capture_time
                .then(|| AbsoluteCaptureTime::from_native(v.absolute_capture_time)),
            has_transport_sequence_number: v.has_transport_sequence_number,
            transport_sequence_number: v.transport_sequence_number,
            feedback_request: v.has_feedback_request.then_some(RtpFeedbackRequest {
                include_timestamps: v.feedback_include_timestamps,
                sequence_count: v.feedback_sequence_count,
            }),
            audio_level: v.has_audio_level.then_some(AudioLevel {
                voice_activity: v.audio_voice_activity,
                level: v.audio_level,
            }),
            has_video_rotation: v.has_video_rotation,
            video_rotation: v.video_rotation,
            has_video_content_type: v.has_video_content_type,
            video_content_type: v.video_content_type,
            has_video_timing: v.has_video_timing,
            video_timing: VideoSendTiming {
                flags: v.video_timing.flags,
                encode_start_delta_ms: v.video_timing.encode_start_delta_ms,
                encode_finish_delta_ms: v.video_timing.encode_finish_delta_ms,
                packetization_finish_delta_ms: v.video_timing.packetization_finish_delta_ms,
                pacer_exit_delta_ms: v.video_timing.pacer_exit_delta_ms,
                network_timestamp_delta_ms: v.video_timing.network_timestamp_delta_ms,
                network2_timestamp_delta_ms: v.video_timing.network2_timestamp_delta_ms,
            },
            playout_delay: VideoPlayoutDelay::from_native(v.playout_delay),
            stream_id: v.stream_id,
            repaired_stream_id: v.repaired_stream_id,
            mid: v.mid,
            color_space: v
                .has_color_space
                .then(|| ColorSpace::from_native(v.color_space)),
        }
    }
}
/// Fully initialized owned input values, including inactive timing deltas.
/// Default is this initialized input, not an observation of a bare SDK header:
/// the SDK constructor leaves inactive timing deltas uninitialized.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RtpHeaderValues {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub csrc_count: u8,
    pub csrcs: [u32; 15],
    pub padding_length: usize,
    pub header_length: usize,
    pub extension: RtpHeaderExtensionValues,
}
impl RtpHeaderValues {
    pub(crate) fn to_native(&self) -> ffi::FfiMediaRtpHeader {
        ffi::FfiMediaRtpHeader {
            marker: self.marker,
            payload_type: self.payload_type,
            sequence_number: self.sequence_number,
            timestamp: self.timestamp,
            ssrc: self.ssrc,
            csrc_count: self.csrc_count,
            csrcs: self.csrcs,
            padding_length: self.padding_length,
            header_length: self.header_length,
            extension: self.extension.to_native(),
        }
    }
    pub(crate) fn from_native(v: ffi::FfiMediaRtpHeader) -> Self {
        Self {
            marker: v.marker,
            payload_type: v.payload_type,
            sequence_number: v.sequence_number,
            timestamp: v.timestamp,
            ssrc: v.ssrc,
            csrc_count: v.csrc_count,
            csrcs: v.csrcs,
            padding_length: v.padding_length,
            header_length: v.header_length,
            extension: RtpHeaderExtensionValues::from_native(v.extension),
        }
    }
}

#[cfg(test)]
#[path = "rtp_header_values_tests.rs"]
mod tests;
