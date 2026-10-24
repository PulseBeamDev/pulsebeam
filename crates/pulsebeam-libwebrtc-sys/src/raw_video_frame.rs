use crate::{
    ColorSpace, RtpReceivedPacketInfos, Timestamp, VideoFrame, VideoRotation, VideoSourceView, ffi,
};
use std::{fmt, marker::PhantomData, rc::Rc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawVideoError {
    InvalidGeometry,
    InvalidPlane,
    Uninitialized,
    ConversionFailed,
    NotNativeStorage,
    InvalidMetadata,
}
impl fmt::Display for RawVideoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "raw video operation failed: {self:?}")
    }
}
impl std::error::Error for RawVideoError {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoUpdateRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
impl VideoUpdateRect {
    pub(crate) fn native(self) -> ffi::FfiRawUpdateRect {
        ffi::FfiRawUpdateRect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
    fn from_native(r: ffi::FfiRawUpdateRect) -> Self {
        Self {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
    pub fn is_empty(self) -> bool {
        self.width == 0 && self.height == 0
    }
    pub fn make_empty(&mut self) {
        *self = Self::default();
    }
    fn operation(self, other: Self, op: u8, geometry: &[i32]) -> Result<Self, RawVideoError> {
        let mut valid = false;
        let result =
            ffi::raw_update_rect_operation(self.native(), other.native(), op, geometry, &mut valid);
        if valid {
            Ok(Self::from_native(result))
        } else {
            Err(RawVideoError::InvalidGeometry)
        }
    }
    pub fn union(self, other: Self) -> Result<Self, RawVideoError> {
        self.operation(other, 0, &[])
    }
    pub fn intersect(self, other: Self) -> Result<Self, RawVideoError> {
        self.operation(other, 1, &[])
    }
    pub fn scale_with_frame(
        self,
        width: i32,
        height: i32,
        crop: VideoUpdateRect,
        scaled_width: i32,
        scaled_height: i32,
    ) -> Result<Self, RawVideoError> {
        self.operation(
            Self::default(),
            3,
            &[
                width,
                height,
                crop.x,
                crop.y,
                crop.width,
                crop.height,
                scaled_width,
                scaled_height,
            ],
        )
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoFrameProcessingTime {
    pub start: Timestamp,
    pub finish: Timestamp,
}
impl VideoFrameProcessingTime {
    pub fn elapsed(self) -> Result<crate::TimeDelta, crate::TimeValueError> {
        self.finish.duration_since(self.start)
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoFrameRenderParameters {
    pub use_low_latency_rendering: bool,
    pub max_composition_delay_in_frames: Option<i32>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoFrameContentType {
    Unspecified,
    Screenshare,
    Native(u8),
}
impl VideoFrameContentType {
    pub(crate) fn native(self) -> u8 {
        match self {
            Self::Unspecified => 0,
            Self::Screenshare => 1,
            Self::Native(v) => v,
        }
    }
    pub(crate) fn from_native(v: u8) -> Self {
        match v {
            0 => Self::Unspecified,
            1 => Self::Screenshare,
            v => Self::Native(v),
        }
    }
}
/// Owning complete SDK frame metadata. Read from a frame for its native
/// defaults; replacing metadata rebuilds a real SDK frame with the same buffer.
#[derive(Clone, Debug, PartialEq)]
pub struct RawVideoFrameMetadata {
    pub id: u16,
    pub timestamp_us: i64,
    pub rtp_timestamp: u32,
    pub ntp_time_ms: i64,
    pub presentation_timestamp: Option<Timestamp>,
    pub reference_time: Option<Timestamp>,
    pub rotation: VideoRotation,
    pub color_space: Option<ColorSpace>,
    pub render_parameters: VideoFrameRenderParameters,
    pub update_rect: Option<VideoUpdateRect>,
    pub packet_infos: RtpReceivedPacketInfos,
    pub processing_time: Option<VideoFrameProcessingTime>,
    pub is_repeat_frame: bool,
    pub content_type: Option<VideoFrameContentType>,
}
impl RawVideoFrameMetadata {
    /// Deprecated SDK capture-time-identifier query aliases presentation time.
    pub fn capture_time_identifier(&self) -> Option<Timestamp> {
        self.presentation_timestamp
    }
    fn from_native(v: ffi::FfiRawFrameMetadata) -> Self {
        Self {
            id: v.id,
            timestamp_us: v.timestamp_us,
            rtp_timestamp: v.rtp_timestamp,
            ntp_time_ms: v.ntp_time_ms,
            presentation_timestamp: v
                .has_presentation_timestamp
                .then(|| Timestamp::from_native_microsecond_bits(v.presentation_timestamp_us)),
            reference_time: v
                .has_reference_time
                .then(|| Timestamp::from_native_microsecond_bits(v.reference_time_us)),
            rotation: VideoRotation::from_degrees(v.rotation),
            color_space: v
                .has_color_space
                .then(|| ColorSpace::from_native(v.color_space)),
            render_parameters: VideoFrameRenderParameters {
                use_low_latency_rendering: v.low_latency,
                max_composition_delay_in_frames: v
                    .has_max_composition_delay
                    .then_some(v.max_composition_delay),
            },
            update_rect: v
                .has_update_rect
                .then(|| VideoUpdateRect::from_native(v.update_rect)),
            packet_infos: RtpReceivedPacketInfos::from_native(v.packet_infos),
            processing_time: v.has_processing_time.then(|| VideoFrameProcessingTime {
                start: Timestamp::from_native_microsecond_bits(v.processing_start_us),
                finish: Timestamp::from_native_microsecond_bits(v.processing_finish_us),
            }),
            is_repeat_frame: v.repeat_frame,
            content_type: v
                .has_content_type
                .then(|| VideoFrameContentType::from_native(v.content_type)),
        }
    }
    fn native(&self) -> ffi::FfiRawFrameMetadata {
        ffi::FfiRawFrameMetadata {
            id: self.id,
            timestamp_us: self.timestamp_us,
            rtp_timestamp: self.rtp_timestamp,
            ntp_time_ms: self.ntp_time_ms,
            has_presentation_timestamp: self.presentation_timestamp.is_some(),
            presentation_timestamp_us: self
                .presentation_timestamp
                .map_or(0, Timestamp::native_microsecond_bits),
            has_reference_time: self.reference_time.is_some(),
            reference_time_us: self
                .reference_time
                .map_or(0, Timestamp::native_microsecond_bits),
            rotation: self.rotation as u16,
            has_color_space: self.color_space.is_some(),
            color_space: self.color_space.clone().unwrap_or_default().to_native(),
            low_latency: self.render_parameters.use_low_latency_rendering,
            has_max_composition_delay: self
                .render_parameters
                .max_composition_delay_in_frames
                .is_some(),
            max_composition_delay: self
                .render_parameters
                .max_composition_delay_in_frames
                .unwrap_or(0),
            has_update_rect: self.update_rect.is_some(),
            update_rect: self.update_rect.unwrap_or_default().native(),
            packet_infos: self.packet_infos.to_native(),
            has_processing_time: self.processing_time.is_some(),
            processing_start_us: self
                .processing_time
                .map_or(0, |p| p.start.native_microsecond_bits()),
            processing_finish_us: self
                .processing_time
                .map_or(0, |p| p.finish.native_microsecond_bits()),
            repeat_frame: self.is_repeat_frame,
            has_content_type: self.content_type.is_some(),
            content_type: self.content_type.map_or(0, VideoFrameContentType::native),
        }
    }
}

/// SDK frame copy ownership, independent metadata and a shared immutable SDK
/// buffer. Local handles remain sequence-bound. Acquisition retains its source
/// and factory even after outer track, source or actor identities are retired.
pub struct RawVideoFrame {
    pub(crate) native: cxx::UniquePtr<ffi::NativeRawVideoFrame>,
    origin: Option<VideoSourceView>,
    _local: PhantomData<Rc<()>>,
}
impl Clone for RawVideoFrame {
    fn clone(&self) -> Self {
        Self {
            native: ffi::raw_frame_clone(self.native()),
            origin: self.origin.clone(),
            _local: PhantomData,
        }
    }
}
impl RawVideoFrame {
    // Closed decoder producers retain CPU buffers by their actual SDK refs;
    // VP9/H264/dav1d wrapped buffers own their backing frame/picture lifetimes.
    pub(crate) fn from_decoded_native(native: cxx::UniquePtr<ffi::NativeRawVideoFrame>) -> Self {
        assert!(!native.is_null());
        Self {
            native,
            origin: None,
            _local: PhantomData,
        }
    }
    pub(crate) fn into_native(self) -> cxx::UniquePtr<ffi::NativeRawVideoFrame> {
        self.native
    }
    pub(crate) fn acquired(
        native: cxx::UniquePtr<ffi::NativeRawVideoFrame>,
        source: VideoSourceView,
    ) -> Option<Self> {
        (!native.is_null()).then(|| Self {
            native,
            origin: Some(source),
            _local: PhantomData,
        })
    }
    pub(crate) fn native(&self) -> &ffi::NativeRawVideoFrame {
        self.native.as_ref().expect("owning raw frame")
    }
    pub fn from_buffer(buffer: &RawVideoBuffer) -> Self {
        Self {
            native: ffi::raw_frame_from_buffer(buffer.native()),
            origin: buffer.origin.clone(),
            _local: PhantomData,
        }
    }
    /// The deprecated SDK render-time constructor, with checked millisecond
    /// multiplication before native entry. Its RTP timestamp is independent.
    pub fn from_render_time_ms(
        buffer: &RawVideoBuffer,
        rtp_timestamp: u32,
        milliseconds: i64,
        rotation: VideoRotation,
    ) -> Result<Self, RawVideoError> {
        let native = ffi::raw_frame_from_render_time(
            buffer.native(),
            rtp_timestamp,
            milliseconds,
            rotation as u16,
        );
        if native.is_null() {
            return Err(RawVideoError::InvalidMetadata);
        }
        Ok(Self {
            native,
            origin: buffer.origin.clone(),
            _local: PhantomData,
        })
    }
    pub fn from_legacy(frame: &VideoFrame) -> Result<Self, RawVideoError> {
        let bytes = ffi::video_source_i420_allocation_size(frame.width, frame.height);
        if bytes == 0 || bytes != frame.buffer.as_bytes().len() {
            return Err(RawVideoError::InvalidGeometry);
        }
        let mut writer = WritableI420Buffer::new(frame.width as i32, frame.height as i32)?;
        let w = frame.width as usize;
        let h = frame.height as usize;
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        let data = frame.buffer.as_bytes();
        writer.write_plane(RawVideoPlaneKind::Y, &data[..w * h], w as i32)?;
        writer.write_plane(
            RawVideoPlaneKind::U,
            &data[w * h..w * h + cw * ch],
            cw as i32,
        )?;
        writer.write_plane(RawVideoPlaneKind::V, &data[w * h + cw * ch..], cw as i32)?;
        let mut result = Self::from_buffer(&writer.freeze()?);
        let mut metadata = result.metadata();
        metadata.timestamp_us = frame.timestamp_us;
        metadata.rtp_timestamp = frame.rtp_timestamp;
        metadata.rotation = frame.rotation;
        result.set_metadata(&metadata)?;
        Ok(result)
    }
    pub fn metadata(&self) -> RawVideoFrameMetadata {
        RawVideoFrameMetadata::from_native(ffi::raw_frame_metadata(self.native()))
    }
    pub fn set_metadata(&mut self, value: &RawVideoFrameMetadata) -> Result<(), RawVideoError> {
        if ffi::raw_frame_set_metadata(self.native.pin_mut(), &value.native()) {
            Ok(())
        } else {
            Err(RawVideoError::InvalidMetadata)
        }
    }
    pub fn buffer(&self) -> RawVideoBuffer {
        RawVideoBuffer {
            native: ffi::raw_frame_buffer(self.native()),
            origin: self.origin.clone(),
            _local: PhantomData,
        }
    }
    pub fn set_buffer(&mut self, value: &RawVideoBuffer) -> Result<(), RawVideoError> {
        if !ffi::raw_frame_set_buffer(self.native.pin_mut(), value.native()) {
            return Err(RawVideoError::InvalidGeometry);
        }
        self.origin = value.origin.clone();
        Ok(())
    }
    pub fn timestamp_us(&self) -> i64 {
        ffi::raw_frame_timestamp_us(self.native())
    }
    pub fn width(&self) -> i32 {
        ffi::raw_frame_width(self.native())
    }
    pub fn height(&self) -> i32 {
        ffi::raw_frame_height(self.native())
    }
    pub fn size(&self) -> u32 {
        ffi::raw_frame_size(self.native())
    }
    pub fn is_texture(&self) -> bool {
        ffi::raw_frame_is_texture(self.native())
    }
    pub fn render_time_ms(&self) -> i64 {
        ffi::raw_frame_render_time_ms(self.native())
    }
    pub fn update_rect(&self) -> VideoUpdateRect {
        VideoUpdateRect::from_native(ffi::raw_frame_update_rect(self.native()))
    }
    /// Explicit conversion to the legacy packed-I420 value; extra metadata is
    /// intentionally not represented by that older value type.
    pub fn to_legacy(&self) -> Result<VideoFrame, RawVideoError> {
        let frame = ffi::raw_frame_legacy(self.native());
        let frame = frame.as_ref().ok_or(RawVideoError::ConversionFailed)?;
        VideoFrame::i420(
            ffi::native_video_frame_width(frame),
            ffi::native_video_frame_height(frame),
            ffi::native_video_frame_i420(frame),
            ffi::native_video_frame_timestamp_us(frame),
            ffi::native_video_frame_rtp_timestamp(frame),
        )
        .map(|value| {
            value.with_rotation(VideoRotation::from_degrees(
                ffi::native_video_frame_rotation(frame),
            ))
        })
        .map_err(|_| RawVideoError::ConversionFailed)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RawVideoBufferType {
    Native,
    I420,
    I420A,
    I422,
    I444,
    I010,
    I210,
    I410,
    Nv12,
}
impl RawVideoBufferType {
    pub fn native_name(self) -> String {
        ffi::raw_buffer_type_name(self as u8)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawVideoBufferInfo {
    pub buffer_type: RawVideoBufferType,
    pub width: i32,
    pub height: i32,
    pub chroma_width: i32,
    pub chroma_height: i32,
    pub stride_y: i32,
    pub stride_u: i32,
    pub stride_v: i32,
    pub stride_a: i32,
    pub stride_uv: i32,
}
impl RawVideoBufferInfo {
    fn from_native(v: ffi::FfiRawBufferInfo) -> Self {
        let buffer_type = match v.buffer_type {
            0 => RawVideoBufferType::Native,
            1 => RawVideoBufferType::I420,
            2 => RawVideoBufferType::I420A,
            3 => RawVideoBufferType::I422,
            4 => RawVideoBufferType::I444,
            5 => RawVideoBufferType::I010,
            6 => RawVideoBufferType::I210,
            7 => RawVideoBufferType::I410,
            8 => RawVideoBufferType::Nv12,
            _ => unreachable!("closed pinned SDK buffer type"),
        };
        Self {
            buffer_type,
            width: v.width,
            height: v.height,
            chroma_width: v.chroma_width,
            chroma_height: v.chroma_height,
            stride_y: v.stride_y,
            stride_u: v.stride_u,
            stride_v: v.stride_v,
            stride_a: v.stride_a,
            stride_uv: v.stride_uv,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RawVideoPlaneKind {
    Y,
    U,
    V,
    A,
    Uv,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawVideoSamples {
    U8(Vec<u8>),
    U16(Vec<u16>),
}
/// Visible rows copied without native padding. Stride remains the SDK's stride
/// in sample elements, not the stride of this packed owning sample vector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawVideoPlane {
    pub width: i32,
    pub height: i32,
    pub native_stride: i32,
    pub samples: RawVideoSamples,
}
impl RawVideoPlane {
    fn from_native(v: ffi::FfiRawVideoPlane) -> Result<Self, RawVideoError> {
        if !v.valid {
            return Err(RawVideoError::InvalidPlane);
        }
        Ok(Self {
            width: v.width,
            height: v.height,
            native_stride: v.stride,
            samples: if v.bits == 16 {
                RawVideoSamples::U16(v.data16)
            } else {
                RawVideoSamples::U8(v.data8)
            },
        })
    }
}
pub struct RawVideoBuffer {
    native: cxx::UniquePtr<ffi::NativeRawVideoBuffer>,
    origin: Option<VideoSourceView>,
    _local: PhantomData<Rc<()>>,
}
impl Clone for RawVideoBuffer {
    fn clone(&self) -> Self {
        Self {
            native: ffi::raw_buffer_clone(self.native()),
            origin: self.origin.clone(),
            _local: PhantomData,
        }
    }
}
impl RawVideoBuffer {
    pub(crate) fn native(&self) -> &ffi::NativeRawVideoBuffer {
        self.native.as_ref().expect("owning raw buffer")
    }
    fn derived(&self, native: cxx::UniquePtr<ffi::NativeRawVideoBuffer>) -> Option<Self> {
        (!native.is_null()).then(|| Self {
            native,
            origin: self.origin.clone(),
            _local: PhantomData,
        })
    }
    pub fn info(&self) -> RawVideoBufferInfo {
        RawVideoBufferInfo::from_native(ffi::raw_buffer_info(self.native()))
    }
    pub fn storage_representation_bytes(&self) -> Vec<u8> {
        ffi::raw_buffer_storage(self.native())
    }
    pub fn to_i420(&self) -> Result<Self, RawVideoError> {
        self.derived(ffi::raw_buffer_to_i420(self.native()))
            .ok_or(RawVideoError::ConversionFailed)
    }
    pub fn get_i420(&self) -> Option<Self> {
        self.derived(ffi::raw_buffer_get_i420(self.native()))
    }
    pub fn plane(&self, plane: RawVideoPlaneKind) -> Result<RawVideoPlane, RawVideoError> {
        RawVideoPlane::from_native(ffi::raw_buffer_plane(self.native(), plane as u8))
    }
    pub fn crop_and_scale(
        &self,
        crop: VideoUpdateRect,
        width: i32,
        height: i32,
    ) -> Result<Self, RawVideoError> {
        self.derived(ffi::raw_buffer_crop_scale(
            self.native(),
            crop.x,
            crop.y,
            crop.width,
            crop.height,
            width,
            height,
        ))
        .ok_or(RawVideoError::InvalidGeometry)
    }
    pub fn scale(&self, width: i32, height: i32) -> Result<Self, RawVideoError> {
        let info = self.info();
        self.crop_and_scale(
            VideoUpdateRect {
                x: 0,
                y: 0,
                width: info.width,
                height: info.height,
            },
            width,
            height,
        )
    }
    pub fn mapped_buffer(
        &self,
        types: &[RawVideoBufferType],
    ) -> Result<Option<Self>, RawVideoError> {
        if self.info().buffer_type != RawVideoBufferType::Native {
            return Err(RawVideoError::NotNativeStorage);
        }
        Ok(self.derived(ffi::raw_buffer_mapped(
            self.native(),
            &types.iter().map(|t| *t as u8).collect::<Vec<_>>(),
        )))
    }
    pub fn prepare_mapped_buffer(
        &self,
        width: usize,
        height: usize,
        identifier: usize,
    ) -> VideoBufferPreparation {
        VideoBufferPreparation {
            native: ffi::raw_buffer_prepare(self.native(), width, height, identifier),
            _buffer: self.clone(),
        }
    }
}
/// Completion ownership for the actual SDK preparation callback. Polling does
/// not invoke work or advance controlled time. Drop retires future delivery.
pub struct VideoBufferPreparation {
    native: cxx::UniquePtr<ffi::NativeVideoBufferPreparation>,
    _buffer: RawVideoBuffer,
}
impl VideoBufferPreparation {
    pub fn try_next_prepared(&self) -> Option<usize> {
        let mut identifier = 0;
        ffi::raw_buffer_prepared(
            self.native.as_ref().expect("native preparation"),
            &mut identifier,
        )
        .then_some(identifier)
    }
    pub fn close(&self) {
        ffi::raw_buffer_preparation_close(self.native.as_ref().expect("native preparation"));
    }
}
impl Drop for VideoBufferPreparation {
    fn drop(&mut self) {
        self.close();
    }
}

/// Exclusive SDK I420 storage. Native Create leaves pixels uninitialized;
/// reading an unfilled plane and publishing incomplete storage are rejected.
/// Freeze consumes mutable ownership, so no mutable alias can race an encoder.
pub struct WritableI420Buffer {
    native: cxx::UniquePtr<ffi::NativeWritableI420>,
    _local: PhantomData<Rc<()>>,
}
impl WritableI420Buffer {
    fn from_native(native: cxx::UniquePtr<ffi::NativeWritableI420>) -> Result<Self, RawVideoError> {
        if native.is_null() {
            Err(RawVideoError::InvalidGeometry)
        } else {
            Ok(Self {
                native,
                _local: PhantomData,
            })
        }
    }
    fn native(&self) -> &ffi::NativeWritableI420 {
        self.native.as_ref().expect("exclusive native I420")
    }
    pub fn new(width: i32, height: i32) -> Result<Self, RawVideoError> {
        let chroma = width.checked_add(1).ok_or(RawVideoError::InvalidGeometry)? / 2;
        Self::with_strides(width, height, width, chroma, chroma)
    }
    pub fn with_strides(
        width: i32,
        height: i32,
        y: i32,
        u: i32,
        v: i32,
    ) -> Result<Self, RawVideoError> {
        Self::from_native(ffi::raw_i420_create(width, height, y, u, v))
    }
    /// Copy visible strided source planes into owning native I420 storage.
    /// Source padding is not copied, matching the SDK raw-plane Copy overload.
    pub fn copy_from_planes(
        width: i32,
        height: i32,
        y: &crate::VideoPlane,
        u: &crate::VideoPlane,
        v: &crate::VideoPlane,
    ) -> Result<Self, RawVideoError> {
        let mut result = Self::new(width, height)?;
        for (plane, source) in [
            (RawVideoPlaneKind::Y, y),
            (RawVideoPlaneKind::U, u),
            (RawVideoPlaneKind::V, v),
        ] {
            result.write_plane(
                plane,
                &source.data,
                i32::try_from(source.stride).map_err(|_| RawVideoError::InvalidPlane)?,
            )?;
        }
        Ok(result)
    }
    pub fn copy_from(source: &RawVideoBuffer) -> Result<Self, RawVideoError> {
        Self::from_native(ffi::raw_i420_copy(source.native()))
    }
    pub fn rotated(
        source: &RawVideoBuffer,
        rotation: VideoRotation,
    ) -> Result<Self, RawVideoError> {
        Self::from_native(ffi::raw_i420_rotate(source.native(), rotation as u16))
    }
    pub fn info(&self) -> RawVideoBufferInfo {
        RawVideoBufferInfo::from_native(ffi::raw_i420_info(self.native()))
    }
    pub fn is_initialized(&self) -> bool {
        ffi::raw_i420_initialized(self.native())
    }
    pub fn plane(&self, plane: RawVideoPlaneKind) -> Result<RawVideoPlane, RawVideoError> {
        RawVideoPlane::from_native(ffi::raw_i420_plane(self.native(), plane as u8))
    }
    pub fn write_plane(
        &mut self,
        plane: RawVideoPlaneKind,
        bytes: &[u8],
        stride: i32,
    ) -> Result<(), RawVideoError> {
        if ffi::raw_i420_write_plane(self.native.pin_mut(), plane as u8, bytes, stride) {
            Ok(())
        } else {
            Err(RawVideoError::InvalidPlane)
        }
    }
    pub fn set_black(&mut self) {
        ffi::raw_i420_initialize(self.native.pin_mut(), true);
    }
    pub fn initialize_data(&mut self) {
        ffi::raw_i420_initialize(self.native.pin_mut(), false);
    }
    pub fn crop_and_scale_from(
        &mut self,
        source: &RawVideoBuffer,
        crop: VideoUpdateRect,
    ) -> Result<(), RawVideoError> {
        if ffi::raw_i420_crop_from(
            self.native.pin_mut(),
            source.native(),
            crop.x,
            crop.y,
            crop.width,
            crop.height,
        ) {
            Ok(())
        } else {
            Err(RawVideoError::InvalidGeometry)
        }
    }
    pub fn scale_from(&mut self, source: &RawVideoBuffer) -> Result<(), RawVideoError> {
        if ffi::raw_i420_scale_from(self.native.pin_mut(), source.native(), false) {
            Ok(())
        } else {
            Err(RawVideoError::InvalidGeometry)
        }
    }
    pub fn center_crop_and_scale_from(
        &mut self,
        source: &RawVideoBuffer,
    ) -> Result<(), RawVideoError> {
        if ffi::raw_i420_scale_from(self.native.pin_mut(), source.native(), true) {
            Ok(())
        } else {
            Err(RawVideoError::InvalidGeometry)
        }
    }
    pub fn freeze(self) -> Result<RawVideoBuffer, RawVideoError> {
        if !self.is_initialized() {
            return Err(RawVideoError::Uninitialized);
        }
        let native = ffi::raw_i420_freeze(self.native);
        if native.is_null() {
            return Err(RawVideoError::Uninitialized);
        }
        Ok(RawVideoBuffer {
            native,
            origin: None,
            _local: PhantomData,
        })
    }
}
