//! Complete owning SDK encoder information, separate from legacy provider DTOs.
use crate::{RawVideoBufferType, ffi};
use std::fmt;

/// Full native initialization settings. Size and signed core/thread values are
/// preserved; native product consumption may reject an unsupported configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoEncoderSettingsValue {
    pub loss_notification: bool,
    pub number_of_cores: i32,
    pub max_payload_size: usize,
    pub encoder_thread_limit: Option<i32>,
}
impl VideoEncoderSettingsValue {
    pub(crate) fn native(self) -> ffi::FfiOwnedEncoderSettings {
        ffi::FfiOwnedEncoderSettings {
            loss_notification: self.loss_notification,
            number_of_cores: self.number_of_cores,
            max_payload_size: self.max_payload_size,
            has_thread_limit: self.encoder_thread_limit.is_some(),
            thread_limit: self.encoder_thread_limit.unwrap_or(0),
        }
    }
    pub(crate) fn from_native(v: ffi::FfiOwnedEncoderSettings) -> Self {
        Self {
            loss_notification: v.loss_notification,
            number_of_cores: v.number_of_cores,
            max_payload_size: v.max_payload_size,
            encoder_thread_limit: v.has_thread_limit.then_some(v.thread_limit),
        }
    }
}
/// Owning native VideoCodec with full, byte-preserving layer formats.
pub struct OwnedVideoCodec {
    native: cxx::UniquePtr<ffi::NativeVideoCodec>,
}
// SAFETY: independent value storage, immutable reads and exclusive mutation.
unsafe impl Send for OwnedVideoCodec {}
unsafe impl Sync for OwnedVideoCodec {}
impl Default for OwnedVideoCodec {
    fn default() -> Self {
        Self {
            native: ffi::video_codec_new(),
        }
    }
}
impl Clone for OwnedVideoCodec {
    fn clone(&self) -> Self {
        Self {
            native: ffi::video_codec_clone(self.native()),
        }
    }
}
impl fmt::Debug for OwnedVideoCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fields().fmt(f)
    }
}
impl OwnedVideoCodec {
    pub(crate) fn native(&self) -> &ffi::NativeVideoCodec {
        self.native.as_ref().expect("owning codec")
    }
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeVideoCodec>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub fn fields(&self) -> crate::VideoCodecConfiguration {
        crate::VideoCodecConfiguration::from_native(ffi::video_codec_fields(self.native()))
    }
    pub fn set_fields(
        &mut self,
        fields: &crate::VideoCodecConfiguration,
    ) -> Result<(), crate::CodecError> {
        if ffi::video_codec_set_fields(self.native.pin_mut(), fields.native()) {
            Ok(())
        } else {
            Err(crate::CodecError::InvalidConfiguration)
        }
    }
}
/// Owning native allocation with SDK operations. An unset layer and an
/// explicitly zero layer remain distinct. Overflow returns native false.
pub struct OwnedVideoBitrateAllocation {
    native: cxx::UniquePtr<ffi::NativeVideoBitrateAllocation>,
}
// SAFETY: independently owned scalar/optional arrays, no engine references.
unsafe impl Send for OwnedVideoBitrateAllocation {}
unsafe impl Sync for OwnedVideoBitrateAllocation {}
impl Default for OwnedVideoBitrateAllocation {
    fn default() -> Self {
        Self {
            native: ffi::video_allocation_new(),
        }
    }
}
impl Clone for OwnedVideoBitrateAllocation {
    fn clone(&self) -> Self {
        Self {
            native: ffi::video_allocation_clone(self.native()),
        }
    }
}
impl PartialEq for OwnedVideoBitrateAllocation {
    fn eq(&self, other: &Self) -> bool {
        ffi::video_allocation_equal(self.native(), other.native())
    }
}
impl Eq for OwnedVideoBitrateAllocation {}
impl fmt::Debug for OwnedVideoBitrateAllocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fields().fmt(f)
    }
}
impl fmt::Display for OwnedVideoBitrateAllocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&ffi::video_allocation_string(self.native()))
    }
}
impl OwnedVideoBitrateAllocation {
    fn native(&self) -> &ffi::NativeVideoBitrateAllocation {
        self.native.as_ref().expect("owning native allocation")
    }
    pub fn fields(&self) -> crate::VideoBitrateAllocation {
        crate::VideoBitrateAllocation::from_native(ffi::video_allocation_fields(self.native()))
    }
    pub fn set_fields(
        &mut self,
        fields: crate::VideoBitrateAllocation,
    ) -> Result<(), crate::CodecError> {
        let fields = fields
            .native()
            .ok_or(crate::CodecError::InvalidConfiguration)?;
        if ffi::video_allocation_set_fields(self.native.pin_mut(), fields) {
            Ok(())
        } else {
            Err(crate::CodecError::InvalidConfiguration)
        }
    }
    fn indices(spatial: usize, temporal: usize) -> Result<(), crate::CodecError> {
        if spatial < 5 && temporal < 4 {
            Ok(())
        } else {
            Err(crate::CodecError::InvalidConfiguration)
        }
    }
    pub fn set_bitrate(
        &mut self,
        spatial: usize,
        temporal: usize,
        bits: u32,
    ) -> Result<bool, crate::CodecError> {
        Self::indices(spatial, temporal)?;
        Ok(ffi::video_allocation_set(
            self.native.pin_mut(),
            spatial,
            temporal,
            bits,
        ))
    }
    pub fn bitrate(
        &self,
        spatial: usize,
        temporal: usize,
    ) -> Result<Option<u32>, crate::CodecError> {
        Self::indices(spatial, temporal)?;
        Ok(self.fields().layer_bitrates_bps[spatial][temporal])
    }
    pub fn is_spatial_layer_used(&self, spatial: usize) -> Result<bool, crate::CodecError> {
        Self::indices(spatial, 0)?;
        Ok(self.fields().layer_bitrates_bps[spatial]
            .iter()
            .any(Option::is_some))
    }
    pub fn spatial_layer_sum(&self, spatial: usize) -> Result<u32, crate::CodecError> {
        Self::indices(spatial, 0)?;
        Ok(ffi::video_allocation_sum(self.native(), spatial, 0, 2))
    }
    pub fn temporal_layer_sum(
        &self,
        spatial: usize,
        temporal: usize,
    ) -> Result<u32, crate::CodecError> {
        Self::indices(spatial, temporal)?;
        Ok(ffi::video_allocation_sum(
            self.native(),
            spatial,
            temporal,
            3,
        ))
    }
    pub fn temporal_layer_allocation(&self, spatial: usize) -> Result<Vec<u32>, crate::CodecError> {
        Self::indices(spatial, 0)?;
        Ok(ffi::video_allocation_temporal(self.native(), spatial))
    }
    pub fn simulcast_allocations(&self) -> [Option<Self>; 5] {
        std::array::from_fn(|i| {
            let native = ffi::video_allocation_simulcast(self.native(), i);
            (!native.is_null()).then_some(Self { native })
        })
    }
    pub fn sum_bps(&self) -> u32 {
        ffi::video_allocation_sum(self.native(), 0, 0, 0)
    }
    pub fn sum_kbps(&self) -> u32 {
        ffi::video_allocation_sum(self.native(), 0, 0, 1)
    }
    pub fn bandwidth_limited(&self) -> bool {
        self.fields().bandwidth_limited
    }
    pub fn set_bandwidth_limited(&mut self, limited: bool) {
        let mut fields = self.fields();
        fields.bandwidth_limited = limited;
        self.set_fields(fields).expect("native allocation snapshot");
    }
}
/// Exact native allocations, including unset versus explicit zero cells and
/// independently preserved target/effective bandwidth-limited flags.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoRateControlFields {
    pub target: crate::VideoBitrateAllocation,
    pub effective: crate::VideoBitrateAllocation,
    pub framerate_fps: f64,
    pub bandwidth_allocation: crate::DataRate,
}
fn rate_parts(rate: crate::DataRate) -> (i64, i8) {
    match rate {
        crate::DataRate::BitsPerSecond(v) => (v, 0),
        crate::DataRate::PositiveInfinity => (0, 1),
        crate::DataRate::NegativeInfinity => (0, -1),
    }
}
/// Owning SDK RateControlParameters. Constructors use the actual SDK defaults
/// and overloads; updates are transactional if an allocation sum is too large.
pub struct OwnedVideoRateControl {
    native: cxx::UniquePtr<ffi::NativeVideoRateControl>,
}
// SAFETY: independent value copies, immutable reads and exclusive mutation only.
unsafe impl Send for OwnedVideoRateControl {}
unsafe impl Sync for OwnedVideoRateControl {}
impl Default for OwnedVideoRateControl {
    fn default() -> Self {
        Self {
            native: ffi::video_rate_control_new(),
        }
    }
}
impl Clone for OwnedVideoRateControl {
    fn clone(&self) -> Self {
        Self {
            native: ffi::video_rate_control_clone(self.native()),
        }
    }
}
impl PartialEq for OwnedVideoRateControl {
    fn eq(&self, other: &Self) -> bool {
        ffi::video_rate_control_equal(self.native(), other.native())
    }
}
impl fmt::Debug for OwnedVideoRateControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fields().fmt(f)
    }
}
impl OwnedVideoRateControl {
    pub(crate) fn native(&self) -> &ffi::NativeVideoRateControl {
        self.native.as_ref().expect("owning rate parameters")
    }
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeVideoRateControl>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub fn new(
        allocation: crate::VideoBitrateAllocation,
        framerate_fps: f64,
        bandwidth: Option<crate::DataRate>,
    ) -> Result<Self, crate::CodecError> {
        let allocation = allocation
            .native()
            .ok_or(crate::CodecError::InvalidConfiguration)?;
        let (bits, kind) = rate_parts(bandwidth.unwrap_or(crate::DataRate::BitsPerSecond(0)));
        let native = ffi::video_rate_control_construct(
            allocation,
            framerate_fps,
            bandwidth.is_some(),
            bits,
            kind,
        );
        if native.is_null() {
            Err(crate::CodecError::InvalidConfiguration)
        } else {
            Ok(Self { native })
        }
    }
    pub fn fields(&self) -> VideoRateControlFields {
        let v = ffi::video_rate_control_fields(self.native());
        VideoRateControlFields {
            target: crate::VideoBitrateAllocation::from_native(v.target),
            effective: crate::VideoBitrateAllocation::from_native(v.effective),
            framerate_fps: v.framerate_fps,
            bandwidth_allocation: crate::DataRate::from_native(v.bandwidth_bps, v.bandwidth_kind),
        }
    }
    pub fn set_fields(&mut self, fields: VideoRateControlFields) -> Result<(), crate::CodecError> {
        let (bandwidth_bps, bandwidth_kind) = rate_parts(fields.bandwidth_allocation);
        let fields = ffi::FfiOwnedVideoRateControl {
            target: fields
                .target
                .native()
                .ok_or(crate::CodecError::InvalidConfiguration)?,
            effective: fields
                .effective
                .native()
                .ok_or(crate::CodecError::InvalidConfiguration)?,
            framerate_fps: fields.framerate_fps,
            bandwidth_bps,
            bandwidth_kind,
        };
        if ffi::video_rate_control_set_fields(self.native.pin_mut(), fields) {
            Ok(())
        } else {
            Err(crate::CodecError::InvalidConfiguration)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoEncoderQpThresholds {
    pub low: i32,
    pub high: i32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoEncoderScalingSettings {
    pub thresholds: Option<VideoEncoderQpThresholds>,
    pub min_pixels_per_frame: i32,
}
#[derive(Clone, Copy, Debug)]
pub struct VideoResolutionBitrateLimits {
    pub frame_size_pixels: i32,
    pub min_start_bitrate_bps: i32,
    pub min_bitrate_bps: i32,
    pub max_bitrate_bps: i32,
}
impl VideoResolutionBitrateLimits {
    fn native(self) -> ffi::FfiVideoResolutionBitrateLimits {
        ffi::FfiVideoResolutionBitrateLimits {
            frame_size_pixels: self.frame_size_pixels,
            min_start_bitrate_bps: self.min_start_bitrate_bps,
            min_bitrate_bps: self.min_bitrate_bps,
            max_bitrate_bps: self.max_bitrate_bps,
        }
    }
    fn from_native(v: ffi::FfiVideoResolutionBitrateLimits) -> Self {
        Self {
            frame_size_pixels: v.frame_size_pixels,
            min_start_bitrate_bps: v.min_start_bitrate_bps,
            min_bitrate_bps: v.min_bitrate_bps,
            max_bitrate_bps: v.max_bitrate_bps,
        }
    }
}
impl PartialEq for VideoResolutionBitrateLimits {
    fn eq(&self, other: &Self) -> bool {
        ffi::video_resolution_bitrate_limits_equal(self.native(), other.native())
    }
}
impl Eq for VideoResolutionBitrateLimits {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoEncoderMappedResolution {
    pub width: i32,
    pub height: i32,
}
/// Fixed-int native enum. Unknown storage values are safe inert metadata, but
/// cannot be passed to the native fatal naming operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoEncoderPixelFormat {
    Known(RawVideoBufferType),
    NativeValue(i32),
}
impl From<RawVideoBufferType> for VideoEncoderPixelFormat {
    fn from(value: RawVideoBufferType) -> Self {
        Self::Known(value)
    }
}
impl VideoEncoderPixelFormat {
    fn native(self) -> i32 {
        match self {
            Self::Known(value) => value as i32,
            Self::NativeValue(value) => value,
        }
    }
    fn from_native(value: i32) -> Self {
        use RawVideoBufferType as B;
        match value {
            0 => Self::Known(B::Native),
            1 => Self::Known(B::I420),
            2 => Self::Known(B::I420A),
            3 => Self::Known(B::I422),
            4 => Self::Known(B::I444),
            5 => Self::Known(B::I010),
            6 => Self::Known(B::I210),
            7 => Self::Known(B::I410),
            8 => Self::Known(B::Nv12),
            value => Self::NativeValue(value),
        }
    }
    pub fn native_name(self) -> Option<String> {
        match Self::from_native(self.native()) {
            Self::Known(value) => Some(value.native_name()),
            Self::NativeValue(_) => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoEncoderInfoFields {
    pub scaling_settings: VideoEncoderScalingSettings,
    pub requested_resolution_alignment: u32,
    pub apply_alignment_to_all_simulcast_layers: bool,
    pub supports_native_handle: bool,
    pub implementation_name: Vec<u8>,
    pub has_trusted_rate_controller: bool,
    pub is_hardware_accelerated: bool,
    pub enable_cpu_overuse_detection: bool,
    /// InlinedVector's inline capacity is not a length restriction. Preserve
    /// every byte and the complete vector length for each spatial layer.
    pub fps_allocation: [Vec<u8>; 5],
    pub resolution_bitrate_limits: Vec<VideoResolutionBitrateLimits>,
    pub supports_simulcast: bool,
    pub preferred_pixel_formats: Vec<VideoEncoderPixelFormat>,
    pub is_qp_trusted: Option<bool>,
    pub min_qp: Option<i32>,
    pub mapped_resolution: Option<VideoEncoderMappedResolution>,
}
impl VideoEncoderInfoFields {
    fn native(self) -> ffi::FfiOwnedVideoEncoderInfo {
        let thresholds = self.scaling_settings.thresholds;
        let resolution = self.mapped_resolution;
        ffi::FfiOwnedVideoEncoderInfo {
            has_thresholds: thresholds.is_some(),
            qp_low: thresholds.map_or(0, |v| v.low),
            qp_high: thresholds.map_or(0, |v| v.high),
            min_pixels_per_frame: self.scaling_settings.min_pixels_per_frame,
            requested_resolution_alignment: self.requested_resolution_alignment,
            apply_alignment_to_all_simulcast_layers: self.apply_alignment_to_all_simulcast_layers,
            supports_native_handle: self.supports_native_handle,
            implementation_name: self.implementation_name,
            has_trusted_rate_controller: self.has_trusted_rate_controller,
            is_hardware_accelerated: self.is_hardware_accelerated,
            enable_cpu_overuse_detection: self.enable_cpu_overuse_detection,
            fps_allocation: self
                .fps_allocation
                .into_iter()
                .map(|fractions| ffi::FfiVideoEncoderFpsAllocation { fractions })
                .collect(),
            resolution_bitrate_limits: self
                .resolution_bitrate_limits
                .into_iter()
                .map(VideoResolutionBitrateLimits::native)
                .collect(),
            supports_simulcast: self.supports_simulcast,
            preferred_pixel_formats: self
                .preferred_pixel_formats
                .into_iter()
                .map(VideoEncoderPixelFormat::native)
                .collect(),
            has_is_qp_trusted: self.is_qp_trusted.is_some(),
            is_qp_trusted: self.is_qp_trusted.unwrap_or(false),
            has_min_qp: self.min_qp.is_some(),
            min_qp: self.min_qp.unwrap_or(0),
            has_mapped_resolution: resolution.is_some(),
            mapped_width: resolution.map_or(0, |v| v.width),
            mapped_height: resolution.map_or(0, |v| v.height),
        }
    }
    fn from_native(v: ffi::FfiOwnedVideoEncoderInfo) -> Self {
        Self {
            scaling_settings: VideoEncoderScalingSettings {
                thresholds: v.has_thresholds.then_some(VideoEncoderQpThresholds {
                    low: v.qp_low,
                    high: v.qp_high,
                }),
                min_pixels_per_frame: v.min_pixels_per_frame,
            },
            requested_resolution_alignment: v.requested_resolution_alignment,
            apply_alignment_to_all_simulcast_layers: v.apply_alignment_to_all_simulcast_layers,
            supports_native_handle: v.supports_native_handle,
            implementation_name: v.implementation_name,
            has_trusted_rate_controller: v.has_trusted_rate_controller,
            is_hardware_accelerated: v.is_hardware_accelerated,
            enable_cpu_overuse_detection: v.enable_cpu_overuse_detection,
            fps_allocation: v
                .fps_allocation
                .into_iter()
                .map(|layer| layer.fractions)
                .collect::<Vec<_>>()
                .try_into()
                .expect("pinned SDK spatial width"),
            resolution_bitrate_limits: v
                .resolution_bitrate_limits
                .into_iter()
                .map(VideoResolutionBitrateLimits::from_native)
                .collect(),
            supports_simulcast: v.supports_simulcast,
            preferred_pixel_formats: v
                .preferred_pixel_formats
                .into_iter()
                .map(VideoEncoderPixelFormat::from_native)
                .collect(),
            is_qp_trusted: v.has_is_qp_trusted.then_some(v.is_qp_trusted),
            min_qp: v.has_min_qp.then_some(v.min_qp),
            mapped_resolution: v
                .has_mapped_resolution
                .then_some(VideoEncoderMappedResolution {
                    width: v.mapped_width,
                    height: v.mapped_height,
                }),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoEncoderInfoValueError {
    UnsafePixelFormatNaming,
    ReachedBitrateLimitsAssertion,
}
impl fmt::Display for VideoEncoderInfoValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for VideoEncoderInfoValueError {}
pub struct OwnedVideoEncoderInfo {
    native: cxx::UniquePtr<ffi::NativeVideoEncoderInfo>,
}
// SAFETY: SDK info owns strings, vectors and plain optional/scalar values. It
// contains no buffer, callback, queue or engine reference. Writes are exclusive.
unsafe impl Send for OwnedVideoEncoderInfo {}
unsafe impl Sync for OwnedVideoEncoderInfo {}
impl Default for OwnedVideoEncoderInfo {
    fn default() -> Self {
        Self {
            native: ffi::video_encoder_info_new(),
        }
    }
}
impl Clone for OwnedVideoEncoderInfo {
    fn clone(&self) -> Self {
        Self {
            native: ffi::video_encoder_info_clone(self.native()),
        }
    }
    fn clone_from(&mut self, other: &Self) {
        self.assign(other);
    }
}
/// Native equality intentionally does not compare every public field in this
/// pinned SDK. Compare fields() snapshots when complete field equality is needed.
impl PartialEq for OwnedVideoEncoderInfo {
    fn eq(&self, other: &Self) -> bool {
        ffi::video_encoder_info_equal(self.native(), other.native())
    }
}
impl Eq for OwnedVideoEncoderInfo {}
impl fmt::Debug for OwnedVideoEncoderInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fields().fmt(f)
    }
}
impl OwnedVideoEncoderInfo {
    pub(crate) fn native(&self) -> &ffi::NativeVideoEncoderInfo {
        self.native.as_ref().expect("owning encoder info")
    }
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeVideoEncoderInfo>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn into_native(self) -> cxx::UniquePtr<ffi::NativeVideoEncoderInfo> {
        self.native
    }
    pub fn assign(&mut self, other: &Self) {
        ffi::video_encoder_info_assign(self.native.pin_mut(), other.native());
    }
    pub fn fields(&self) -> VideoEncoderInfoFields {
        VideoEncoderInfoFields::from_native(ffi::video_encoder_info_fields(self.native()))
    }
    /// Inert metadata admits all representable values. Assertion-prone lookup,
    /// fatal naming and engine consumption are independent boundaries.
    pub fn set_fields(&mut self, fields: VideoEncoderInfoFields) {
        assert!(
            ffi::video_encoder_info_set_fields(self.native.pin_mut(), fields.native()),
            "pinned SDK spatial width"
        );
    }
    pub fn to_string_bytes(&self) -> Result<Vec<u8>, VideoEncoderInfoValueError> {
        let mut valid = false;
        let result = ffi::video_encoder_info_string(self.native(), &mut valid);
        if valid {
            Ok(result)
        } else {
            Err(VideoEncoderInfoValueError::UnsafePixelFormatNaming)
        }
    }
    pub fn bitrate_limits_for_resolution(
        &self,
        frame_size_pixels: i32,
    ) -> Result<Option<VideoResolutionBitrateLimits>, VideoEncoderInfoValueError> {
        let mut valid = false;
        let mut found = false;
        let value = ffi::video_encoder_info_limits(
            self.native(),
            frame_size_pixels,
            &mut valid,
            &mut found,
        );
        if valid {
            Ok(found.then(|| VideoResolutionBitrateLimits::from_native(value)))
        } else {
            Err(VideoEncoderInfoValueError::ReachedBitrateLimitsAssertion)
        }
    }
}
