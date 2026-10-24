use std::fmt;

use crate::{RtpVideoHeaderCodecSpecifics, VideoCodecType, ffi};

/// Complete signed-int metadata domain of the SDK's scoped frame-type enum.
/// Native string/packetization consumers have their own narrower contracts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoMetadataFrameType(pub i32);
impl VideoMetadataFrameType {
    pub const EMPTY: Self = Self(0);
    pub const KEY: Self = Self(3);
    pub const DELTA: Self = Self(4);
}

/// Native rotation metadata, including representable unnamed enum values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoMetadataRotation(pub i32);
impl VideoMetadataRotation {
    pub const NONE: Self = Self(0);
    pub const CLOCKWISE_90: Self = Self(90);
    pub const CLOCKWISE_180: Self = Self(180);
    pub const CLOCKWISE_270: Self = Self(270);
}

/// Full byte domain of the SDK's fixed-underlying content-type enum.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoMetadataContentType(pub u8);
impl VideoMetadataContentType {
    pub const UNSPECIFIED: Self = Self(0);
    pub const SCREENSHARE: Self = Self(1);
}

/// Full signed-int metadata domain of the SDK's scoped indication enum.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoMetadataDecodeTargetIndication(pub i32);
impl VideoMetadataDecodeTargetIndication {
    pub const NOT_PRESENT: Self = Self(0);
    pub const DISCARDABLE: Self = Self(1);
    pub const SWITCH: Self = Self(2);
    pub const REQUIRED: Self = Self(3);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoMetadataError {
    /// The value is outside the non-fixed C++ enum's representable range.
    /// This is distinct from an unnamed but representable enum value.
    NativeEnumRange(&'static str),
    /// A specific native operation would index beyond physical SDK storage.
    NativeConsumption(&'static str),
    /// The compiled SDK inline operation would trigger a fatal assertion.
    NativeAssertion(&'static str),
}
impl fmt::Display for VideoMetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NativeEnumRange(field) => write!(f, "{field} is outside its native enum range"),
            Self::NativeConsumption(sink) => write!(f, "unsafe native consumption: {sink}"),
            Self::NativeAssertion(sink) => write!(f, "native assertion: {sink}"),
        }
    }
}
impl std::error::Error for VideoMetadataError {}

/// Independent snapshot of the common metadata fields, not codec-specific
/// headers. Absence of dependencies differs from a present empty list.
/// This snapshot does not establish that its values are safe for transmission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoFrameMetadataValues {
    pub frame_type: VideoMetadataFrameType,
    pub width: u16,
    pub height: u16,
    pub rotation: VideoMetadataRotation,
    pub content_type: VideoMetadataContentType,
    pub frame_id: Option<i64>,
    pub spatial_index: i32,
    pub temporal_index: i32,
    pub dependencies: Option<Vec<i64>>,
    pub decode_target_indications: Vec<VideoMetadataDecodeTargetIndication>,
    pub is_last_frame_in_picture: bool,
    pub simulcast_idx: u8,
    pub codec: VideoCodecType,
    pub ssrc: u32,
    pub csrcs: Vec<u32>,
}
impl Default for VideoFrameMetadataValues {
    fn default() -> Self {
        VideoFrameMetadata::new().values()
    }
}
impl VideoFrameMetadataValues {
    fn from_native(value: ffi::FfiVideoFrameMetadataValues) -> Self {
        Self {
            frame_type: VideoMetadataFrameType(value.frame_type),
            width: value.width,
            height: value.height,
            rotation: VideoMetadataRotation(value.rotation),
            content_type: VideoMetadataContentType(value.content_type),
            frame_id: value.has_frame_id.then_some(value.frame_id),
            spatial_index: value.spatial_index,
            temporal_index: value.temporal_index,
            dependencies: value.has_dependencies.then_some(value.dependencies),
            decode_target_indications: value
                .decode_target_indications
                .into_iter()
                .map(VideoMetadataDecodeTargetIndication)
                .collect(),
            is_last_frame_in_picture: value.is_last_frame_in_picture,
            simulcast_idx: value.simulcast_idx,
            codec: VideoCodecType::from_native(value.codec),
            ssrc: value.ssrc,
            csrcs: value.csrcs,
        }
    }
    fn to_native(&self) -> ffi::FfiVideoFrameMetadataValues {
        ffi::FfiVideoFrameMetadataValues {
            frame_type: self.frame_type.0,
            width: self.width,
            height: self.height,
            rotation: self.rotation.0,
            content_type: self.content_type.0,
            has_frame_id: self.frame_id.is_some(),
            frame_id: self.frame_id.unwrap_or(0),
            spatial_index: self.spatial_index,
            temporal_index: self.temporal_index,
            has_dependencies: self.dependencies.is_some(),
            dependencies: self.dependencies.clone().unwrap_or_default(),
            decode_target_indications: self
                .decode_target_indications
                .iter()
                .map(|value| value.0)
                .collect(),
            is_last_frame_in_picture: self.is_last_frame_in_picture,
            simulcast_idx: self.simulcast_idx,
            codec: match self.codec {
                VideoCodecType::Generic => 0,
                VideoCodecType::Vp8 => 1,
                VideoCodecType::Vp9 => 2,
                VideoCodecType::Av1 => 3,
                VideoCodecType::H264 => 4,
                VideoCodecType::H265 => 5,
                VideoCodecType::Native(value) => value,
            },
            ssrc: self.ssrc,
            csrcs: self.csrcs.clone(),
        }
    }
}

/// Owns the actual SDK VideoFrameMetadata. Copy/assignment retain all native
/// fields. values() snapshots common fields; codec_specifics() snapshots the
/// entire codec-specific alternative including inactive physical storage.
pub struct VideoFrameMetadata {
    pub(crate) native: cxx::UniquePtr<ffi::NativeVideoFrameMetadata>,
}
// SAFETY: exclusively owned native scalars/vectors/variant. Read-only shared
// access, exclusive mutation, and no engine, callback or thread-affine lifetime.
unsafe impl Send for VideoFrameMetadata {}
// SAFETY: no mutable reference escapes, including through common snapshots.
unsafe impl Sync for VideoFrameMetadata {}

impl VideoFrameMetadata {
    pub fn new() -> Self {
        Self {
            native: ffi::video_frame_metadata_new(),
        }
    }
    pub(crate) fn native(&self) -> &ffi::NativeVideoFrameMetadata {
        self.native.as_ref().expect("allocated SDK metadata")
    }
    pub fn assign_from(&mut self, source: &Self) {
        ffi::video_frame_metadata_assign(self.native.pin_mut(), source.native());
    }
    /// Delegates to SDK equality, including its conditional VP9/GOF prefixes.
    /// Rejects only an array read the selected equality path would overrun.
    /// A prior native short circuit preserves its ordinary false result.
    pub fn equals(&self, other: &Self) -> Result<bool, VideoMetadataError> {
        crate::video_frame_metadata_headers::equality_status(ffi::video_frame_metadata_equals(
            self.native(),
            other.native(),
        ))
    }
    pub fn codec_specifics(&self) -> RtpVideoHeaderCodecSpecifics {
        RtpVideoHeaderCodecSpecifics::from_native(ffi::video_frame_metadata_specifics(
            self.native(),
        ))
    }
    /// Replaces only codec-specific storage. Codec and variant need not agree
    /// for metadata ownership, matching the SDK. Invalid counts remain metadata
    /// values; their consuming methods apply their own safety checks.
    pub fn set_codec_specifics(
        &mut self,
        value: &RtpVideoHeaderCodecSpecifics,
    ) -> Result<(), VideoMetadataError> {
        RtpVideoHeaderCodecSpecifics::check_set_status(ffi::video_frame_metadata_set_specifics(
            self.native.pin_mut(),
            value.to_native(),
        ))
    }
    pub fn values(&self) -> VideoFrameMetadataValues {
        VideoFrameMetadataValues::from_native(ffi::video_frame_metadata_values(self.native()))
    }
    /// Updates common fields, preserving codec-specific header storage.
    /// Enum-range rejection leaves every native field unchanged.
    pub fn set_values(
        &mut self,
        values: &VideoFrameMetadataValues,
    ) -> Result<(), VideoMetadataError> {
        match ffi::video_frame_metadata_set_values(self.native.pin_mut(), values.to_native()) {
            0 => Ok(()),
            1 => Err(VideoMetadataError::NativeEnumRange("rotation")),
            2 => Err(VideoMetadataError::NativeEnumRange("codec")),
            _ => unreachable!("native metadata setter error code"),
        }
    }

    pub fn frame_type(&self) -> VideoMetadataFrameType {
        self.values().frame_type
    }
    pub fn set_frame_type(&mut self, value: VideoMetadataFrameType) {
        let mut fields = self.values();
        fields.frame_type = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn width(&self) -> u16 {
        self.values().width
    }
    pub fn set_width(&mut self, value: u16) {
        let mut fields = self.values();
        fields.width = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn height(&self) -> u16 {
        self.values().height
    }
    pub fn set_height(&mut self, value: u16) {
        let mut fields = self.values();
        fields.height = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn content_type(&self) -> VideoMetadataContentType {
        self.values().content_type
    }
    pub fn set_content_type(&mut self, value: VideoMetadataContentType) {
        let mut fields = self.values();
        fields.content_type = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn frame_id(&self) -> Option<i64> {
        self.values().frame_id
    }
    pub fn set_frame_id(&mut self, value: Option<i64>) {
        let mut fields = self.values();
        fields.frame_id = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn spatial_index(&self) -> i32 {
        self.values().spatial_index
    }
    pub fn set_spatial_index(&mut self, value: i32) {
        let mut fields = self.values();
        fields.spatial_index = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn temporal_index(&self) -> i32 {
        self.values().temporal_index
    }
    pub fn set_temporal_index(&mut self, value: i32) {
        let mut fields = self.values();
        fields.temporal_index = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn dependencies(&self) -> Option<Vec<i64>> {
        self.values().dependencies
    }
    pub fn set_dependencies(&mut self, value: Option<Vec<i64>>) {
        let mut fields = self.values();
        fields.dependencies = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn decode_target_indications(&self) -> Vec<VideoMetadataDecodeTargetIndication> {
        self.values().decode_target_indications
    }
    pub fn set_decode_target_indications(
        &mut self,
        value: Vec<VideoMetadataDecodeTargetIndication>,
    ) {
        let mut fields = self.values();
        fields.decode_target_indications = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn is_last_frame_in_picture(&self) -> bool {
        self.values().is_last_frame_in_picture
    }
    pub fn set_is_last_frame_in_picture(&mut self, value: bool) {
        let mut fields = self.values();
        fields.is_last_frame_in_picture = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn simulcast_idx(&self) -> u8 {
        self.values().simulcast_idx
    }
    pub fn set_simulcast_idx(&mut self, value: u8) {
        let mut fields = self.values();
        fields.simulcast_idx = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn ssrc(&self) -> u32 {
        self.values().ssrc
    }
    pub fn set_ssrc(&mut self, value: u32) {
        let mut fields = self.values();
        fields.ssrc = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn csrcs(&self) -> Vec<u32> {
        self.values().csrcs
    }
    pub fn set_csrcs(&mut self, value: Vec<u32>) {
        let mut fields = self.values();
        fields.csrcs = value;
        self.set_values(&fields)
            .expect("existing native enums remain representable");
    }

    pub fn rotation(&self) -> VideoMetadataRotation {
        self.values().rotation
    }
    pub fn set_rotation(&mut self, value: VideoMetadataRotation) -> Result<(), VideoMetadataError> {
        let mut fields = self.values();
        fields.rotation = value;
        self.set_values(&fields)
    }

    pub fn codec(&self) -> VideoCodecType {
        self.values().codec
    }
    pub fn set_codec(&mut self, value: VideoCodecType) -> Result<(), VideoMetadataError> {
        let mut fields = self.values();
        fields.codec = value;
        self.set_values(&fields)
    }

    /// Legacy query maps absent dependencies to an empty list.
    pub fn frame_dependencies(&self) -> Vec<i64> {
        self.dependencies().unwrap_or_default()
    }
    /// Legacy setter always stores a present list, including an empty one.
    pub fn set_frame_dependencies(&mut self, value: &[i64]) {
        self.set_dependencies(Some(value.to_vec()));
    }
}
impl Default for VideoFrameMetadata {
    fn default() -> Self {
        Self::new()
    }
}
impl Clone for VideoFrameMetadata {
    fn clone(&self) -> Self {
        Self {
            native: ffi::video_frame_metadata_clone(self.native()),
        }
    }
}
impl fmt::Debug for VideoFrameMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoFrameMetadata")
            .field("common_fields", &self.values())
            .finish_non_exhaustive()
    }
}
