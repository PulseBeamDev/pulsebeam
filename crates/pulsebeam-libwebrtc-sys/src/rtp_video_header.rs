//! Owning RTP video header metadata, not a raw-RTP media source.
use crate::{
    AbsoluteCaptureTime, ColorSpace, FrameInstrumentationData, RtpVideoHeaderCodecSpecifics,
    RtpVideoHeaderH264, RtpVideoHeaderVp8, RtpVideoHeaderVp9, VideoCodecType, VideoFrameMetadata,
    VideoMetadataContentType, VideoMetadataDecodeTargetIndication, VideoMetadataError,
    VideoMetadataFrameType, VideoMetadataRotation, VideoPlayoutDelay, VideoSendTiming, ffi,
};

/// Complete generic descriptor storage, independent of its wire consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpVideoGenericDescriptorInfo {
    pub frame_id: i64,
    pub spatial_index: i32,
    pub temporal_index: i32,
    pub decode_target_indications: Vec<VideoMetadataDecodeTargetIndication>,
    pub dependencies: Vec<i64>,
    pub chain_diffs: Vec<i32>,
    pub active_decode_targets: u32,
}
impl Default for RtpVideoGenericDescriptorInfo {
    fn default() -> Self {
        Self {
            frame_id: 0,
            spatial_index: 0,
            temporal_index: 0,
            decode_target_indications: vec![],
            dependencies: vec![],
            chain_diffs: vec![],
            active_decode_targets: u32::MAX,
        }
    }
}
impl RtpVideoGenericDescriptorInfo {
    fn to_native(&self) -> ffi::FfiRtpVideoGenericDescriptorInfo {
        ffi::FfiRtpVideoGenericDescriptorInfo {
            frame_id: self.frame_id,
            spatial_index: self.spatial_index,
            temporal_index: self.temporal_index,
            decode_target_indications: self.decode_target_indications.iter().map(|v| v.0).collect(),
            dependencies: self.dependencies.clone(),
            chain_diffs: self.chain_diffs.clone(),
            active_decode_targets: self.active_decode_targets,
        }
    }
    fn from_native(v: ffi::FfiRtpVideoGenericDescriptorInfo) -> Self {
        Self {
            frame_id: v.frame_id,
            spatial_index: v.spatial_index,
            temporal_index: v.temporal_index,
            decode_target_indications: v
                .decode_target_indications
                .into_iter()
                .map(VideoMetadataDecodeTargetIndication)
                .collect(),
            dependencies: v.dependencies,
            chain_diffs: v.chain_diffs,
            active_decode_targets: v.active_decode_targets,
        }
    }
}

/// The outer header has a legacy-generic alternative absent from VideoFrameMetadata.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum RtpVideoTypeHeader {
    #[default]
    None,
    Vp8(RtpVideoHeaderVp8),
    Vp9(Box<RtpVideoHeaderVp9>),
    H264(RtpVideoHeaderH264),
    LegacyGeneric {
        picture_id: u16,
    },
}
impl RtpVideoTypeHeader {
    fn to_native(&self) -> (ffi::FfiVideoHeaderCodecSpecifics, u16) {
        let (value, legacy) = match self {
            Self::None => (RtpVideoHeaderCodecSpecifics::None, 0),
            Self::Vp8(v) => (RtpVideoHeaderCodecSpecifics::Vp8(v.clone()), 0),
            Self::Vp9(v) => (RtpVideoHeaderCodecSpecifics::Vp9(v.clone()), 0),
            Self::H264(v) => (RtpVideoHeaderCodecSpecifics::H264(v.clone()), 0),
            Self::LegacyGeneric { picture_id } => {
                let mut value = RtpVideoHeaderCodecSpecifics::None.to_native();
                value.tag = 4;
                return (value, *picture_id);
            }
        };
        (value.to_native(), legacy)
    }
    fn from_native(v: ffi::FfiVideoHeaderCodecSpecifics, picture_id: u16) -> Self {
        if v.tag == 4 {
            return Self::LegacyGeneric { picture_id };
        }
        match RtpVideoHeaderCodecSpecifics::from_native(v) {
            RtpVideoHeaderCodecSpecifics::None => Self::None,
            RtpVideoHeaderCodecSpecifics::Vp8(v) => Self::Vp8(v),
            RtpVideoHeaderCodecSpecifics::Vp9(v) => Self::Vp9(v),
            RtpVideoHeaderCodecSpecifics::H264(v) => Self::H264(v),
        }
    }
}

/// Every outer header field, including physical codec storage and optional values.
/// Codec selection and variant are independent until a specific consumer uses them.
#[derive(Clone, Debug)]
pub struct RtpVideoHeaderValues {
    pub generic: Option<RtpVideoGenericDescriptorInfo>,
    pub frame_type: VideoMetadataFrameType,
    pub width: u16,
    pub height: u16,
    pub rotation: VideoMetadataRotation,
    pub content_type: VideoMetadataContentType,
    pub is_first_packet_in_frame: bool,
    pub is_last_packet_in_frame: bool,
    pub is_last_frame_in_picture: bool,
    pub simulcast_idx: u8,
    pub codec: VideoCodecType,
    pub playout_delay: Option<VideoPlayoutDelay>,
    pub video_timing: VideoSendTiming,
    pub color_space: Option<ColorSpace>,
    pub video_frame_tracking_id: Option<u16>,
    pub video_type_header: RtpVideoTypeHeader,
    pub absolute_capture_time: Option<AbsoluteCaptureTime>,
    pub frame_instrumentation_data: Option<FrameInstrumentationData>,
}
impl Default for RtpVideoHeaderValues {
    fn default() -> Self {
        RtpVideoHeader::new().values()
    }
}
impl RtpVideoHeaderValues {
    fn to_native(&self) -> ffi::FfiRtpVideoHeaderValues {
        let (video_type_header, legacy_generic_picture_id) = self.video_type_header.to_native();
        ffi::FfiRtpVideoHeaderValues {
            has_generic: self.generic.is_some(),
            generic: self
                .generic
                .as_ref()
                .cloned()
                .unwrap_or_default()
                .to_native(),
            frame_type: self.frame_type.0,
            width: self.width,
            height: self.height,
            rotation: self.rotation.0,
            content_type: self.content_type.0,
            is_first_packet_in_frame: self.is_first_packet_in_frame,
            is_last_packet_in_frame: self.is_last_packet_in_frame,
            is_last_frame_in_picture: self.is_last_frame_in_picture,
            simulcast_idx: self.simulcast_idx,
            codec: match self.codec {
                VideoCodecType::Generic => 0,
                VideoCodecType::Vp8 => 1,
                VideoCodecType::Vp9 => 2,
                VideoCodecType::Av1 => 3,
                VideoCodecType::H264 => 4,
                VideoCodecType::H265 => 5,
                VideoCodecType::Native(v) => v,
            },
            has_playout_delay: self.playout_delay.is_some(),
            playout_delay: self.playout_delay.unwrap_or_default().to_native(),
            video_timing: ffi::FfiMediaVideoSendTiming {
                encode_start_delta_ms: self.video_timing.encode_start_delta_ms,
                encode_finish_delta_ms: self.video_timing.encode_finish_delta_ms,
                packetization_finish_delta_ms: self.video_timing.packetization_finish_delta_ms,
                pacer_exit_delta_ms: self.video_timing.pacer_exit_delta_ms,
                network_timestamp_delta_ms: self.video_timing.network_timestamp_delta_ms,
                network2_timestamp_delta_ms: self.video_timing.network2_timestamp_delta_ms,
                flags: self.video_timing.flags,
            },
            has_color_space: self.color_space.is_some(),
            color_space: self
                .color_space
                .as_ref()
                .cloned()
                .unwrap_or_default()
                .to_native(),
            has_video_frame_tracking_id: self.video_frame_tracking_id.is_some(),
            video_frame_tracking_id: self.video_frame_tracking_id.unwrap_or(0),
            video_type_header,
            legacy_generic_picture_id,
            has_absolute_capture_time: self.absolute_capture_time.is_some(),
            absolute_capture_time: self.absolute_capture_time.unwrap_or_default().to_native(),
            frame_instrumentation_data: self
                .frame_instrumentation_data
                .as_ref()
                .map(|v| ffi::frame_instrumentation_clone(v.native()))
                .unwrap_or_else(cxx::UniquePtr::null),
        }
    }
    fn from_native(v: ffi::FfiRtpVideoHeaderValues) -> Self {
        Self {
            generic: v
                .has_generic
                .then(|| RtpVideoGenericDescriptorInfo::from_native(v.generic)),
            frame_type: VideoMetadataFrameType(v.frame_type),
            width: v.width,
            height: v.height,
            rotation: VideoMetadataRotation(v.rotation),
            content_type: VideoMetadataContentType(v.content_type),
            is_first_packet_in_frame: v.is_first_packet_in_frame,
            is_last_packet_in_frame: v.is_last_packet_in_frame,
            is_last_frame_in_picture: v.is_last_frame_in_picture,
            simulcast_idx: v.simulcast_idx,
            codec: VideoCodecType::from_native(v.codec),
            playout_delay: v
                .has_playout_delay
                .then(|| VideoPlayoutDelay::from_native(v.playout_delay)),
            video_timing: VideoSendTiming {
                encode_start_delta_ms: v.video_timing.encode_start_delta_ms,
                encode_finish_delta_ms: v.video_timing.encode_finish_delta_ms,
                packetization_finish_delta_ms: v.video_timing.packetization_finish_delta_ms,
                pacer_exit_delta_ms: v.video_timing.pacer_exit_delta_ms,
                network_timestamp_delta_ms: v.video_timing.network_timestamp_delta_ms,
                network2_timestamp_delta_ms: v.video_timing.network2_timestamp_delta_ms,
                flags: v.video_timing.flags,
            },
            color_space: v
                .has_color_space
                .then(|| ColorSpace::from_native(v.color_space)),
            video_frame_tracking_id: v
                .has_video_frame_tracking_id
                .then_some(v.video_frame_tracking_id),
            video_type_header: RtpVideoTypeHeader::from_native(
                v.video_type_header,
                v.legacy_generic_picture_id,
            ),
            absolute_capture_time: v
                .has_absolute_capture_time
                .then(|| AbsoluteCaptureTime::from_native(v.absolute_capture_time)),
            frame_instrumentation_data: (!v.frame_instrumentation_data.is_null()).then(|| {
                FrameInstrumentationData {
                    native: v.frame_instrumentation_data,
                }
            }),
        }
    }
}

pub struct RtpVideoHeader {
    pub(crate) native: cxx::UniquePtr<ffi::NativeRtpVideoHeader>,
}
// SAFETY: independent SDK metadata, with owned vectors/color/instrumentation,
// no callbacks, host dependency or role-bound destruction. Mutation is exclusive.
unsafe impl Send for RtpVideoHeader {}
// SAFETY: shared operations only read/copy the independent value.
unsafe impl Sync for RtpVideoHeader {}
impl RtpVideoHeader {
    pub fn new() -> Self {
        Self {
            native: ffi::rtp_video_header_new(),
        }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpVideoHeader {
        self.native.as_ref().expect("allocated SDK header")
    }
    pub fn values(&self) -> RtpVideoHeaderValues {
        RtpVideoHeaderValues::from_native(ffi::rtp_video_header_values(self.native()))
    }
    pub fn set_values(&mut self, v: &RtpVideoHeaderValues) -> Result<(), VideoMetadataError> {
        check_status(ffi::rtp_video_header_set_values(
            self.native.pin_mut(),
            v.to_native(),
        ))
    }
    pub fn assign_from(&mut self, other: &Self) {
        ffi::rtp_video_header_assign(self.native.pin_mut(), other.native());
    }
    /// Native subset conversion; only the three selected codecs require their
    /// matching variant. Other codecs ignore the variant without removing it.
    pub fn as_metadata(&self) -> Result<VideoFrameMetadata, VideoMetadataError> {
        let mut status = 0;
        let native = ffi::rtp_video_header_as_metadata(self.native(), &mut status);
        check_status(status)?;
        Ok(VideoFrameMetadata { native })
    }
    /// Native conversion resets/recreates generic descriptor state but preserves
    /// outer-only fields and unsupported codec variants exactly as the SDK does.
    pub fn set_from_metadata(
        &mut self,
        metadata: &VideoFrameMetadata,
    ) -> Result<(), VideoMetadataError> {
        check_status(ffi::rtp_video_header_set_metadata(
            self.native.pin_mut(),
            metadata.native(),
        ))
    }
    pub fn from_metadata(metadata: &VideoFrameMetadata) -> Result<Self, VideoMetadataError> {
        let mut status = 0;
        let native = ffi::rtp_video_header_from_metadata(metadata.native(), &mut status);
        check_status(status)?;
        Ok(Self { native })
    }
}
fn check_status(status: i32) -> Result<(), VideoMetadataError> {
    if status == 9 {
        return Err(VideoMetadataError::NativeConsumption(
            "RTP video header codec/variant std::get association",
        ));
    }
    match status {
        1 => Err(VideoMetadataError::NativeEnumRange("rotation")),
        2 => Err(VideoMetadataError::NativeEnumRange("codec")),
        status => RtpVideoHeaderCodecSpecifics::check_set_status(status),
    }
}
impl Default for RtpVideoHeader {
    fn default() -> Self {
        Self::new()
    }
}
impl Clone for RtpVideoHeader {
    fn clone(&self) -> Self {
        Self {
            native: ffi::rtp_video_header_clone(self.native()),
        }
    }
}
impl std::fmt::Debug for RtpVideoHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RtpVideoHeader")
            .field(&self.values())
            .finish()
    }
}
