//! Complete owning SDK encoded images and initialized encoded-byte storage.
use crate::{
    ColorSpace, RtpReceivedPacketInfos, Timestamp, VideoFrameContentType, VideoPlayoutDelay,
    VideoRotation, ffi,
};
use std::{cell::Cell, collections::BTreeMap, fmt, marker::PhantomData};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodedImageError {
    InvalidMetadata,
    InvalidLayerIndex,
    InvalidSize,
    Uninitialized,
    UnsafeCaptureTime,
    UnsafeFrameTypeFormatting,
}
impl fmt::Display for EncodedImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "encoded image operation failed: {self:?}")
    }
}
impl std::error::Error for EncodedImageError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodedImageFrameType {
    Empty,
    Key,
    Delta,
    Native(i32),
}
impl EncodedImageFrameType {
    pub(crate) fn native(self) -> i32 {
        match self {
            Self::Empty => 0,
            Self::Key => 3,
            Self::Delta => 4,
            Self::Native(v) => v,
        }
    }
    pub(crate) fn from_native(v: i32) -> Self {
        match v {
            0 => Self::Empty,
            3 => Self::Key,
            4 => Self::Delta,
            v => Self::Native(v),
        }
    }
    pub fn native_name(self) -> Result<String, EncodedImageError> {
        let mut name = String::new();
        if ffi::encoded_image_frame_type_name(self.native(), &mut name) {
            Ok(name)
        } else {
            Err(EncodedImageError::UnsafeFrameTypeFormatting)
        }
    }
}
/// Full representable native rotation enum, distinct from named pixel rotation
/// operations. Consumption may require one of the four named rotations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodedImageRotation {
    None,
    Clockwise90,
    Clockwise180,
    Clockwise270,
    Native(u16),
}
impl EncodedImageRotation {
    fn native(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Clockwise90 => 90,
            Self::Clockwise180 => 180,
            Self::Clockwise270 => 270,
            Self::Native(v) => i32::from(v),
        }
    }
    fn from_native(v: i32) -> Self {
        match v {
            0 => Self::None,
            90 => Self::Clockwise90,
            180 => Self::Clockwise180,
            270 => Self::Clockwise270,
            v => Self::Native(v as u16),
        }
    }
}
impl From<VideoRotation> for EncodedImageRotation {
    fn from(v: VideoRotation) -> Self {
        Self::from_native(v as i32)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EncodedImagePsnr {
    pub y: f64,
    pub u: f64,
    pub v: f64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CorruptionDetectionFilterSettings {
    pub standard_deviation: f64,
    pub luma_error_threshold: i32,
    pub chroma_error_threshold: i32,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EncodedImageTiming {
    pub flags: u8,
    pub encode_start_ms: i64,
    pub encode_finish_ms: i64,
    pub packetization_finish_ms: i64,
    pub pacer_exit_ms: i64,
    pub network_timestamp_ms: i64,
    pub network2_timestamp_ms: i64,
    pub receive_start_ms: i64,
    pub receive_finish_ms: i64,
}
/// Complete independent metadata. Raw capture milliseconds are not a Timestamp:
/// the native CaptureTime getter maps nonpositive storage to minus infinity.
/// Spatial sizes include entries retained after lowering the current index.
#[derive(Clone, Debug, PartialEq)]
pub struct EncodedImageMetadata {
    pub width: u32,
    pub height: u32,
    pub rtp_timestamp: u32,
    pub ntp_time_ms: i64,
    pub capture_time_ms: i64,
    pub frame_type: EncodedImageFrameType,
    pub rotation: EncodedImageRotation,
    pub content_type: VideoFrameContentType,
    pub qp: i32,
    pub simulcast_index: Option<i32>,
    pub spatial_index: Option<i32>,
    pub temporal_index: Option<i32>,
    pub spatial_layer_sizes: BTreeMap<i32, usize>,
    pub presentation_timestamp: Option<Timestamp>,
    pub tracking_id: Option<u16>,
    pub color_space: Option<ColorSpace>,
    pub playout_delay: Option<VideoPlayoutDelay>,
    pub packet_infos: RtpReceivedPacketInfos,
    pub retransmission_allowed: bool,
    pub steady_state_refresh: bool,
    pub timing: EncodedImageTiming,
    pub corruption_filter: Option<CorruptionDetectionFilterSettings>,
    pub psnr: Option<EncodedImagePsnr>,
    pub end_of_temporal_unit: Option<bool>,
}
impl EncodedImageMetadata {
    fn from_native(v: ffi::FfiEncodedImageMetadata) -> Self {
        let optional = |v: ffi::FfiOptionalI32| v.present.then_some(v.value);
        Self {
            width: v.width,
            height: v.height,
            rtp_timestamp: v.rtp_timestamp,
            ntp_time_ms: v.ntp_time_ms,
            capture_time_ms: v.capture_time_ms,
            frame_type: EncodedImageFrameType::from_native(v.frame_type),
            rotation: EncodedImageRotation::from_native(v.rotation),
            content_type: VideoFrameContentType::from_native(v.content_type),
            qp: v.qp,
            simulcast_index: optional(v.simulcast_index),
            spatial_index: optional(v.spatial_index),
            temporal_index: optional(v.temporal_index),
            spatial_layer_sizes: v
                .spatial_layer_sizes
                .into_iter()
                .map(|v| (v.index, v.bytes))
                .collect(),
            presentation_timestamp: v
                .has_presentation_time
                .then(|| Timestamp::from_native_microsecond_bits(v.presentation_time_us)),
            tracking_id: v.has_tracking_id.then_some(v.tracking_id),
            color_space: v
                .has_color_space
                .then(|| ColorSpace::from_native(v.color_space)),
            playout_delay: v
                .has_playout_delay
                .then(|| VideoPlayoutDelay::from_native(v.playout_delay)),
            packet_infos: RtpReceivedPacketInfos::from_native(v.packet_infos),
            retransmission_allowed: v.retransmission_allowed,
            steady_state_refresh: v.steady_state_refresh,
            timing: EncodedImageTiming {
                flags: v.timing_flags,
                encode_start_ms: v.timing_ms[0],
                encode_finish_ms: v.timing_ms[1],
                packetization_finish_ms: v.timing_ms[2],
                pacer_exit_ms: v.timing_ms[3],
                network_timestamp_ms: v.timing_ms[4],
                network2_timestamp_ms: v.timing_ms[5],
                receive_start_ms: v.timing_ms[6],
                receive_finish_ms: v.timing_ms[7],
            },
            corruption_filter: v.has_corruption_filter.then_some(
                CorruptionDetectionFilterSettings {
                    standard_deviation: v.filter_std_dev,
                    luma_error_threshold: v.filter_luma_threshold,
                    chroma_error_threshold: v.filter_chroma_threshold,
                },
            ),
            psnr: v.has_psnr.then_some(EncodedImagePsnr {
                y: v.psnr[0],
                u: v.psnr[1],
                v: v.psnr[2],
            }),
            end_of_temporal_unit: v.has_end_of_temporal_unit.then_some(v.end_of_temporal_unit),
        }
    }
    fn native(&self) -> ffi::FfiEncodedImageMetadata {
        let optional = |v: Option<i32>| ffi::FfiOptionalI32 {
            present: v.is_some(),
            value: v.unwrap_or(0),
        };
        let filter = self.corruption_filter.unwrap_or_default();
        let psnr = self.psnr.unwrap_or_default();
        let t = self.timing;
        ffi::FfiEncodedImageMetadata {
            width: self.width,
            height: self.height,
            rtp_timestamp: self.rtp_timestamp,
            ntp_time_ms: self.ntp_time_ms,
            capture_time_ms: self.capture_time_ms,
            frame_type: self.frame_type.native(),
            rotation: self.rotation.native(),
            content_type: self.content_type.native(),
            qp: self.qp,
            simulcast_index: optional(self.simulcast_index),
            spatial_index: optional(self.spatial_index),
            temporal_index: optional(self.temporal_index),
            spatial_layer_sizes: self
                .spatial_layer_sizes
                .iter()
                .map(|(&index, &bytes)| ffi::FfiEncodedLayerSize { index, bytes })
                .collect(),
            has_presentation_time: self.presentation_timestamp.is_some(),
            presentation_time_us: self
                .presentation_timestamp
                .map_or(0, Timestamp::native_microsecond_bits),
            has_tracking_id: self.tracking_id.is_some(),
            tracking_id: self.tracking_id.unwrap_or(0),
            has_color_space: self.color_space.is_some(),
            color_space: self.color_space.clone().unwrap_or_default().to_native(),
            has_playout_delay: self.playout_delay.is_some(),
            playout_delay: self.playout_delay.unwrap_or_default().to_native(),
            packet_infos: self.packet_infos.to_native(),
            retransmission_allowed: self.retransmission_allowed,
            steady_state_refresh: self.steady_state_refresh,
            timing_flags: t.flags,
            timing_ms: [
                t.encode_start_ms,
                t.encode_finish_ms,
                t.packetization_finish_ms,
                t.pacer_exit_ms,
                t.network_timestamp_ms,
                t.network2_timestamp_ms,
                t.receive_start_ms,
                t.receive_finish_ms,
            ],
            has_corruption_filter: self.corruption_filter.is_some(),
            filter_std_dev: filter.standard_deviation,
            filter_luma_threshold: filter.luma_error_threshold,
            filter_chroma_threshold: filter.chroma_error_threshold,
            has_psnr: self.psnr.is_some(),
            psnr: [psnr.y, psnr.u, psnr.v],
            has_end_of_temporal_unit: self.end_of_temporal_unit.is_some(),
            end_of_temporal_unit: self.end_of_temporal_unit.unwrap_or(false),
        }
    }
}
/// SDK EncodedImage value. Copies have independent metadata and share immutable
/// ref-counted encoded storage. No borrowed SDK callback or codec is retained.
pub struct EncodedImage {
    native: cxx::UniquePtr<ffi::NativeEncodedImage>,
    _exclusive: PhantomData<Cell<()>>,
}
// SAFETY: closed producers use owning CPU EncodedImageBuffer storage, immutable
// after sharing. Metadata is owned; mutation requires exclusive Rust access.
// New opaque/custom-release producers require a separate destruction proof.
unsafe impl Send for EncodedImage {}
impl Default for EncodedImage {
    fn default() -> Self {
        Self::from_native(ffi::encoded_image_new())
    }
}
impl Clone for EncodedImage {
    fn clone(&self) -> Self {
        Self::from_native(ffi::encoded_image_clone(self.native()))
    }
}
impl fmt::Debug for EncodedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedImage")
            .field("metadata", &self.metadata())
            .field("size", &self.len())
            .finish()
    }
}
impl EncodedImage {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeEncodedImage>) -> Self {
        assert!(!native.is_null());
        Self {
            native,
            _exclusive: PhantomData,
        }
    }
    pub(crate) fn native(&self) -> &ffi::NativeEncodedImage {
        self.native.as_ref().expect("owning encoded image")
    }
    pub fn assign(&mut self, other: &Self) {
        ffi::encoded_image_assign(self.native.pin_mut(), other.native());
    }
    pub fn metadata(&self) -> EncodedImageMetadata {
        EncodedImageMetadata::from_native(ffi::encoded_image_metadata(self.native()))
    }
    /// Atomic value replacement, retaining encoded storage and logical byte
    /// length. Native copy assignment permits clearing every optional field.
    pub fn set_metadata(&mut self, value: &EncodedImageMetadata) -> Result<(), EncodedImageError> {
        if ffi::encoded_image_set_metadata(self.native.pin_mut(), &value.native()) {
            Ok(())
        } else {
            Err(EncodedImageError::InvalidMetadata)
        }
    }
    pub fn capture_time(&self) -> Result<Timestamp, EncodedImageError> {
        let mut bits = 0;
        if ffi::encoded_image_capture_time(self.native(), &mut bits) {
            Ok(Timestamp::from_native_microsecond_bits(bits))
        } else {
            Err(EncodedImageError::UnsafeCaptureTime)
        }
    }
    pub fn set_encode_time(&mut self, start_ms: i64, finish_ms: i64) {
        ffi::encoded_image_set_encode_time(self.native.pin_mut(), start_ms, finish_ms);
    }
    pub fn len(&self) -> usize {
        ffi::encoded_image_size(self.native())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn set_len(&mut self, bytes: usize) -> Result<(), EncodedImageError> {
        if ffi::encoded_image_set_size(self.native.pin_mut(), bytes) {
            Ok(())
        } else {
            Err(EncodedImageError::InvalidSize)
        }
    }
    pub fn to_vec(&self) -> Vec<u8> {
        ffi::encoded_image_data(self.native())
    }
    pub fn buffer(&self) -> Option<EncodedVideoBuffer> {
        let native = ffi::encoded_image_buffer(self.native());
        (!native.is_null()).then_some(EncodedVideoBuffer { native })
    }
    /// Native SetEncodedData resets logical image length to buffer length.
    pub fn set_buffer(&mut self, buffer: &EncodedVideoBuffer) {
        ffi::encoded_image_set_buffer(self.native.pin_mut(), buffer.native());
    }
    pub fn clear_buffer(&mut self) {
        ffi::encoded_image_clear_buffer(self.native.pin_mut());
    }
    pub fn spatial_layer_frame_size(&self, index: i32) -> Result<Option<usize>, EncodedImageError> {
        let mut size = 0;
        let mut present = false;
        if ffi::encoded_image_layer_size(self.native(), index, &mut size, &mut present) {
            Ok(present.then_some(size))
        } else {
            Err(EncodedImageError::InvalidLayerIndex)
        }
    }
    pub fn set_spatial_layer_frame_size(
        &mut self,
        index: i32,
        bytes: usize,
    ) -> Result<(), EncodedImageError> {
        if ffi::encoded_image_set_layer_size(self.native.pin_mut(), index, bytes) {
            Ok(())
        } else {
            Err(EncodedImageError::InvalidLayerIndex)
        }
    }
    pub fn is_key(&self) -> bool {
        ffi::encoded_image_is_key(self.native())
    }
    pub fn is_delta(&self) -> bool {
        ffi::encoded_image_is_delta(self.native())
    }
}
/// Immutable initialized SDK storage. An empty buffer is present storage, not
/// an EncodedImage's absent buffer. Cloning retains the actual native allocation.
pub struct EncodedVideoBuffer {
    native: cxx::UniquePtr<ffi::NativeEncodedVideoBuffer>,
}
// SAFETY: only immutable owning CPU buffers are admitted; no Rust mutation alias
// or external final-release callback is exposed by the closed producer graph.
unsafe impl Send for EncodedVideoBuffer {}
unsafe impl Sync for EncodedVideoBuffer {}
impl Default for EncodedVideoBuffer {
    fn default() -> Self {
        Self {
            native: ffi::encoded_buffer_empty(),
        }
    }
}
impl Clone for EncodedVideoBuffer {
    fn clone(&self) -> Self {
        Self {
            native: ffi::encoded_buffer_clone(self.native()),
        }
    }
}
impl fmt::Debug for EncodedVideoBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedVideoBuffer")
            .field("size", &self.len())
            .finish()
    }
}
impl EncodedVideoBuffer {
    fn native(&self) -> &ffi::NativeEncodedVideoBuffer {
        self.native.as_ref().expect("owning encoded buffer")
    }
    pub fn from_bytes(data: &[u8]) -> Self {
        Self {
            native: ffi::encoded_buffer_new(data),
        }
    }
    /// Consume caller bytes through the SDK's owning Buffer overload. Bytes
    /// cross allocators by copy; native Buffer ownership then moves into storage.
    pub fn from_owned_bytes(data: Vec<u8>) -> Self {
        Self {
            native: ffi::encoded_buffer_new_owned(data),
        }
    }
    pub fn len(&self) -> usize {
        ffi::encoded_buffer_size(self.native())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn to_vec(&self) -> Vec<u8> {
        ffi::encoded_buffer_data(self.native())
    }
}
/// Exclusive SDK buffer; allocation/Realloc do not initialize newly exposed
/// bytes. Disjoint writes are tracked until every byte is initialized. Freeze
/// consumes the sole mutable native owner, so published aliases stay immutable.
pub struct WritableEncodedVideoBuffer {
    native: cxx::UniquePtr<ffi::NativeWritableEncodedBuffer>,
}
impl fmt::Debug for WritableEncodedVideoBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WritableEncodedVideoBuffer")
            .field("size", &self.len())
            .finish()
    }
}
impl WritableEncodedVideoBuffer {
    pub fn new(size: usize) -> Self {
        Self {
            native: ffi::encoded_writer_new(size),
        }
    }
    fn native(&self) -> &ffi::NativeWritableEncodedBuffer {
        self.native.as_ref().expect("exclusive encoded buffer")
    }
    pub fn len(&self) -> usize {
        ffi::encoded_writer_size(self.native())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn reallocate(&mut self, size: usize) {
        ffi::encoded_writer_resize(self.native.pin_mut(), size);
    }
    pub fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<(), EncodedImageError> {
        if ffi::encoded_writer_write(self.native.pin_mut(), offset, bytes) {
            Ok(())
        } else {
            Err(EncodedImageError::InvalidSize)
        }
    }
    pub fn to_vec(&self) -> Result<Vec<u8>, EncodedImageError> {
        let value = ffi::encoded_writer_read(self.native());
        if value.initialized {
            Ok(value.data)
        } else {
            Err(EncodedImageError::Uninitialized)
        }
    }
    pub fn freeze(self) -> Result<EncodedVideoBuffer, EncodedImageError> {
        let native = ffi::encoded_writer_freeze(self.native);
        if native.is_null() {
            Err(EncodedImageError::Uninitialized)
        } else {
            Ok(EncodedVideoBuffer { native })
        }
    }
}
