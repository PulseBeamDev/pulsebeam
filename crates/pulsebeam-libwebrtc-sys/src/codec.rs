use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::ffi;

pub(crate) const CODEC_OK: i32 = 0;
const CODEC_ERROR: i32 = -1;
const CODEC_PARAMETER: i32 = -4;
const CODEC_UNINITIALIZED: i32 = -7;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodecParameter {
    pub key: String,
    pub value: String,
}

/// A scalability mode recognized by the pinned native engine. Recognition is
/// not a codec capability: an encoder must separately advertise support.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoScalabilityMode(String);

impl VideoScalabilityMode {
    pub fn parse(name: &str) -> Result<Self, CodecError> {
        ffi::video_scalability_mode_valid(name)
            .then(|| Self(name.into()))
            .ok_or(CodecError::UnsupportedFormat)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_native(name: String) -> Self {
        // Native serializers emit only names of their declared enum values.
        Self(name)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoCodecFormat {
    pub name: String,
    pub parameters: Vec<CodecParameter>,
    /// Native SdpVideoFormat capability declarations, not a selected mode.
    pub scalability_modes: Vec<VideoScalabilityMode>,
}

impl VideoCodecFormat {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            parameters: Vec::new(),
            scalability_modes: Vec::new(),
        }
    }

    pub fn with_parameter(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.parameters.push(CodecParameter {
            key: key.into(),
            value: value.into(),
        });
        self
    }

    pub fn with_scalability_mode(mut self, mode: VideoScalabilityMode) -> Self {
        self.scalability_modes.push(mode);
        self
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CodecSupport {
    pub supported: bool,
    pub power_efficient: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoResolution {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoFrameType {
    Key,
    Delta,
}

/// An owned plane whose buffer includes row padding, including the final row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoPlane {
    pub stride: usize,
    pub data: Vec<u8>,
}

/// Owned NV12 planes. The UV plane stores interleaved U,V bytes at half
/// resolution, rounding odd dimensions up.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Nv12Planes {
    pub y: VideoPlane,
    pub uv: VideoPlane,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoFrameBuffer {
    data: Vec<u8>,
}

impl VideoFrameBuffer {
    pub fn i420(width: u32, height: u32, data: Vec<u8>) -> Result<Self, CodecError> {
        if width == 0 || height == 0 || data.len() != i420_len(width, height)? {
            return Err(CodecError::InvalidFrame);
        }
        Ok(Self { data })
    }

    /// Convert strided I420 planes to tightly packed I420. Strides must be at
    /// least the visible row width and each plane must have exactly
    /// `stride * rows` bytes, including final-row padding.
    pub fn i420_strided(
        width: u32,
        height: u32,
        y: VideoPlane,
        u: VideoPlane,
        v: VideoPlane,
    ) -> Result<Self, CodecError> {
        let (w, h) = dimensions(width, height)?;
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        let mut data = Vec::new();
        data.try_reserve_exact(i420_len(width, height)?)
            .map_err(|_| CodecError::InvalidFrame)?;
        append_plane(&mut data, &y, w, h)?;
        append_plane(&mut data, &u, cw, ch)?;
        append_plane(&mut data, &v, cw, ch)?;
        Ok(Self { data })
    }

    /// Convert owned, strided NV12 planes into tightly packed I420 for the
    /// native WebRTC encoder. U and V samples are deinterleaved without
    /// scaling or color conversion.
    pub fn nv12_strided(
        width: u32,
        height: u32,
        y: VideoPlane,
        uv: VideoPlane,
    ) -> Result<Self, CodecError> {
        let (w, h) = dimensions(width, height)?;
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        check_plane(&y, w, h)?;
        check_plane(&uv, cw.checked_mul(2).ok_or(CodecError::InvalidFrame)?, ch)?;
        let mut data = Vec::new();
        data.try_reserve_exact(i420_len(width, height)?)
            .map_err(|_| CodecError::InvalidFrame)?;
        append_plane(&mut data, &y, w, h)?;
        for channel in 0..2 {
            for row in uv.data.chunks_exact(uv.stride) {
                for col in 0..cw {
                    data.push(row[2 * col + channel]);
                }
            }
        }
        Ok(Self { data })
    }

    /// Convert the packed I420 buffer to packed NV12 planes. The returned
    /// buffers own their data and may be retained independently of the frame.
    pub fn to_nv12(&self, width: u32, height: u32) -> Result<Nv12Planes, CodecError> {
        let (w, h) = dimensions(width, height)?;
        if self.data.len() != i420_len(width, height)? {
            return Err(CodecError::InvalidFrame);
        }
        let y_len = w.checked_mul(h).ok_or(CodecError::InvalidFrame)?;
        let chroma_len = w
            .div_ceil(2)
            .checked_mul(h.div_ceil(2))
            .ok_or(CodecError::InvalidFrame)?;
        let mut uv = Vec::new();
        uv.try_reserve_exact(chroma_len.checked_mul(2).ok_or(CodecError::InvalidFrame)?)
            .map_err(|_| CodecError::InvalidFrame)?;
        for col in 0..chroma_len {
            uv.push(self.data[y_len + col]);
            uv.push(self.data[y_len + chroma_len + col]);
        }
        Ok(Nv12Planes {
            y: VideoPlane {
                stride: w,
                data: self.data[..y_len].to_vec(),
            },
            uv: VideoPlane {
                stride: w.div_ceil(2) * 2,
                data: uv,
            },
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}

/// Clockwise rotation carried as video frame metadata; pixels are not rotated.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u16)]
pub enum VideoRotation {
    #[default]
    None = 0,
    Clockwise90 = 90,
    Clockwise180 = 180,
    Clockwise270 = 270,
}

impl VideoRotation {
    pub(crate) fn from_degrees(degrees: u16) -> Self {
        match degrees {
            90 => Self::Clockwise90,
            180 => Self::Clockwise180,
            270 => Self::Clockwise270,
            _ => Self::None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub timestamp_us: i64,
    pub rtp_timestamp: u32,
    pub rotation: VideoRotation,
    pub buffer: VideoFrameBuffer,
}

impl VideoFrame {
    pub fn i420(
        width: u32,
        height: u32,
        data: Vec<u8>,
        timestamp_us: i64,
        rtp_timestamp: u32,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            width,
            height,
            timestamp_us,
            rtp_timestamp,
            rotation: VideoRotation::None,
            buffer: VideoFrameBuffer::i420(width, height, data)?,
        })
    }

    /// Construct an owned NV12 frame, converting to the packed I420 format
    /// used by the injected-video source and decoder callback.
    pub fn nv12(
        width: u32,
        height: u32,
        planes: Nv12Planes,
        timestamp_us: i64,
        rtp_timestamp: u32,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            width,
            height,
            timestamp_us,
            rtp_timestamp,
            rotation: VideoRotation::None,
            buffer: VideoFrameBuffer::nv12_strided(width, height, planes.y, planes.uv)?,
        })
    }

    pub fn to_nv12(&self) -> Result<Nv12Planes, CodecError> {
        self.buffer.to_nv12(self.width, self.height)
    }

    pub fn with_rotation(mut self, rotation: VideoRotation) -> Self {
        self.rotation = rotation;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedVideoFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub rtp_timestamp: u32,
    pub frame_type: VideoFrameType,
    pub qp: Option<u8>,
}

/// Packetizer metadata for an externally encoded frame. The codec must match
/// the negotiated encoder format. Layer identities are never inferred from size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncodedVideoMetadata {
    pub codec: EncodedVideoCodec,
    pub simulcast_index: Option<u8>,
    pub spatial_index: Option<u8>,
    pub temporal_index: Option<u8>,
    pub end_of_picture: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodedVideoCodec {
    Vp8 {
        non_reference: bool,
        layer_sync: bool,
        key_index: Option<u8>,
    },
    Vp9 {
        first_frame_in_picture: bool,
        inter_picture_predicted: bool,
        flexible_mode: bool,
        num_spatial_layers: u8,
        inter_layer_predicted: bool,
        temporal_up_switch: bool,
    },
    H264 {
        /// Native CodecSpecificInfoH264::base_layer_sync: whether this temporal
        /// enhancement frame can synchronize with the base layer. Forwarded to
        /// libwebrtc; this does not calculate dependencies or allocation.
        base_layer_sync: bool,
    },
    Av1,
    H265,
}

impl EncodedVideoMetadata {
    fn ffi(self) -> ffi::FfiEncodedVideoMetadata {
        let (
            codec,
            non_reference,
            layer_sync,
            key_index,
            first_frame_in_picture,
            inter_picture_predicted,
            flexible_mode,
            num_spatial_layers,
            inter_layer_predicted,
            temporal_up_switch,
        ) = match self.codec {
            EncodedVideoCodec::Vp8 {
                non_reference,
                layer_sync,
                key_index,
            } => (
                1,
                non_reference,
                layer_sync,
                key_index.map_or(-1, i32::from),
                false,
                false,
                false,
                1,
                false,
                false,
            ),
            EncodedVideoCodec::Vp9 {
                first_frame_in_picture,
                inter_picture_predicted,
                flexible_mode,
                num_spatial_layers,
                inter_layer_predicted,
                temporal_up_switch,
            } => (
                2,
                false,
                false,
                -1,
                first_frame_in_picture,
                inter_picture_predicted,
                flexible_mode,
                num_spatial_layers,
                inter_layer_predicted,
                temporal_up_switch,
            ),
            EncodedVideoCodec::H264 { base_layer_sync } => (
                3,
                false,
                base_layer_sync,
                -1,
                false,
                false,
                false,
                1,
                false,
                false,
            ),
            EncodedVideoCodec::Av1 => (4, false, false, -1, false, false, false, 1, false, false),
            EncodedVideoCodec::H265 => (5, false, false, -1, false, false, false, 1, false, false),
        };
        ffi::FfiEncodedVideoMetadata {
            codec,
            simulcast_index: self.simulcast_index.map_or(-1, i32::from),
            spatial_index: self.spatial_index.map_or(-1, i32::from),
            temporal_index: self.temporal_index.map_or(-1, i32::from),
            end_of_picture: self.end_of_picture,
            non_reference,
            layer_sync,
            key_index,
            first_frame_in_picture,
            inter_picture_predicted,
            flexible_mode,
            num_spatial_layers,
            inter_layer_predicted,
            temporal_up_switch,
        }
    }
}

/// One native simulcast stream at encoder initialization, in native order.
/// These are engine configuration facts, not a producer allocation or a claim
/// that a particular encoding is currently transmitting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoSimulcastStream {
    pub width: u32,
    pub height: u32,
    pub active: bool,
    pub temporal_layers: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoEncoderSettings {
    pub width: u32,
    pub height: u32,
    pub start_bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub min_bitrate_bps: u32,
    pub max_framerate: u32,
    pub cores: u32,
    pub max_payload_size: u32,
    /// Selected mode observed through native VideoCodec::GetScalabilityMode.
    /// Missing means native configuration has not supplied a mode.
    pub scalability_mode: Option<VideoScalabilityMode>,
    /// Raw native H264-specific configuration, not inferred from the mode.
    /// Absent for other codecs. This may differ from per-stream counts.
    pub h264_temporal_layers: Option<u8>,
    /// Raw native SimulcastStream counts, in configured stream order.
    /// Empty when native numberOfSimulcastStreams is zero. Do not infer an
    /// effective H264 count from only its codec-specific field.
    pub simulcast_temporal_layers: Vec<u8>,
    /// Exact native stream geometry and activation at initialization. Empty
    /// when native numberOfSimulcastStreams is zero; do not guess a stream from
    /// the outer dimensions. Current bitrate guidance is reported separately.
    pub simulcast_streams: Vec<VideoSimulcastStream>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoDecoderSettings {
    pub cores: u32,
    pub max_resolution: Option<VideoResolution>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoRateControl {
    /// Effective encoder allocation, after any native bitrate adjustment.
    pub bitrate_bps: u32,
    /// Native target allocation before encoder bitrate adjustment. This is
    /// observed from SetRates, not copied from the caller allocator's request.
    pub target_bitrate_bps: u32,
    pub framerate_fps: f64,
    pub bandwidth_bps: u64,
    pub bandwidth_limited: bool,
    pub target_bandwidth_limited: bool,
    /// Native bitrate allocation by spatial/simulcast slot then temporal layer.
    /// Cells are per-layer, not cumulative. `None` preserves an unset cell;
    /// `Some(0)` preserves an explicitly zero allocation. No VLA is inferred.
    pub layer_bitrates_bps: [[Option<u32>; 4]; 5],
    /// Exact native target cells, independently preserving absence and zero.
    /// Native simulcast adapters may slice these before product delivery.
    pub target_layer_bitrates_bps: [[Option<u32>; 4]; 5],
}

impl VideoRateControl {
    fn from_ffi(r: ffi::FfiRateControl) -> Self {
        Self {
            bitrate_bps: r.bitrate_bps,
            target_bitrate_bps: r.target_bitrate_bps,
            framerate_fps: r.framerate_fps,
            bandwidth_bps: r.bandwidth_bps,
            bandwidth_limited: r.bandwidth_limited,
            target_bandwidth_limited: r.target_bandwidth_limited,
            layer_bitrates_bps: std::array::from_fn(|spatial| {
                std::array::from_fn(|temporal| {
                    r.has_layer_bitrate[spatial][temporal]
                        .then_some(r.layer_bitrates_bps[spatial][temporal])
                })
            }),
            target_layer_bitrates_bps: std::array::from_fn(|spatial| {
                std::array::from_fn(|temporal| {
                    r.has_target_layer_bitrate[spatial][temporal]
                        .then_some(r.target_layer_bitrates_bps[spatial][temporal])
                })
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoEncoderInfo {
    pub implementation_name: String,
    pub hardware_accelerated: bool,
    pub supports_native_handle: bool,
    pub supports_simulcast: bool,
    /// Native encoder frame-rate fractions for five spatial/simulcast slots.
    /// Each vector describes cumulative temporal layers, with 0 meaning 0%
    /// and 255 meaning 100%. An empty vector means unspecified frame rates.
    /// `None` retains the engine's default single full-rate temporal layer.
    /// These are encoder capability facts, not a producer VLA declaration;
    /// libwebrtc computes allocation and generates negotiated VLA itself.
    pub fps_allocation: Option<[Vec<u8>; 5]>,
}

impl VideoEncoderInfo {
    pub(crate) fn into_native(self) -> crate::OwnedVideoEncoderInfo {
        let mut result = crate::OwnedVideoEncoderInfo::default();
        let mut fields = result.fields();
        fields.implementation_name = self.implementation_name.into_bytes();
        fields.is_hardware_accelerated = self.hardware_accelerated;
        fields.supports_native_handle = self.supports_native_handle;
        fields.supports_simulcast = self.supports_simulcast;
        if let Some(fps) = self.fps_allocation {
            fields.fps_allocation = fps;
        }
        result.set_fields(fields);
        result
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoDecoderInfo {
    pub implementation_name: String,
    pub hardware_accelerated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecError {
    UnsupportedFormat,
    ConstructionFailed,
    InvalidConfiguration,
    InvalidFrame,
    CallbackUnavailable,
    Released,
    ProviderPanicked,
    FormatProjection(crate::VideoFactoryQueryError),
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}",
            match self {
                Self::UnsupportedFormat => "unsupported codec format",
                Self::ConstructionFailed => "codec construction failed",
                Self::InvalidConfiguration => "invalid codec configuration",
                Self::InvalidFrame => "invalid video frame",
                Self::CallbackUnavailable => "codec output callback is unavailable",
                Self::Released => "codec has been released",
                Self::ProviderPanicked => "codec provider panicked",
                Self::FormatProjection(_) =>
                    "complete native format is not representable by the legacy codec provider",
            }
        )
    }
}

impl std::error::Error for CodecError {}

pub trait VideoEncoderFactory: Send + Sync + 'static {
    fn encoder_selector(&self) -> Option<crate::OwnedVideoEncoderSelector> {
        None
    }
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        Vec::new()
    }
    /// Complete owning SDK declarations; override for the full byte/field domain.
    fn supported_format_values(&self) -> Vec<crate::SdpVideoFormat> {
        self.supported_formats()
            .iter()
            .map(crate::SdpVideoFormat::from)
            .collect()
    }
    fn implementation_values(&self) -> Vec<crate::SdpVideoFormat> {
        self.implementations()
            .iter()
            .map(crate::SdpVideoFormat::from)
            .collect()
    }
    fn query_support_legacy_value(
        &self,
        format: &crate::SdpVideoFormat,
        mode: Option<&[u8]>,
    ) -> Result<CodecSupport, crate::VideoFactoryQueryError> {
        self.query_support_value(format, mode, None)
    }
    fn query_support_value(
        &self,
        format: &crate::SdpVideoFormat,
        mode: Option<&[u8]>,
        resolution: Option<&crate::Resolution>,
    ) -> Result<CodecSupport, crate::VideoFactoryQueryError> {
        let format = VideoCodecFormat::try_from(format)?;
        let mode = mode
            .map(|mode| {
                std::str::from_utf8(mode)
                    .map_err(|_| crate::VideoFactoryQueryError::LegacyModeBytes)
            })
            .transpose()?;
        Ok(self.query_support(
            &format,
            mode,
            crate::video_factory_values::legacy_resolution(resolution)?,
        ))
    }

    /// SDK GetImplementations is separate from signaling discovery. Its native
    /// default uses GetSupportedFormats; implementation tags may differ.
    fn implementations(&self) -> Vec<VideoCodecFormat> {
        self.supported_formats()
    }
    fn query_support(
        &self,
        format: &VideoCodecFormat,
        scalability_mode: Option<&str>,
        resolution: Option<VideoResolution>,
    ) -> CodecSupport;
    fn create(&self, format: &VideoCodecFormat) -> Result<Box<dyn VideoEncoder>, CodecError>;
    fn create_value(
        &self,
        format: &crate::SdpVideoFormat,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        self.create(&VideoCodecFormat::try_from(format).map_err(CodecError::FormatProjection)?)
    }
    fn create_with_context(
        &self,
        context: crate::VideoCodecCreationContext,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        self.create_value(&context.format)
    }
}

pub trait VideoDecoderFactory: Send + Sync + 'static {
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        Vec::new()
    }
    /// Complete owning SDK declarations; override for the full byte/field domain.
    fn supported_format_values(&self) -> Vec<crate::SdpVideoFormat> {
        self.supported_formats()
            .iter()
            .map(crate::SdpVideoFormat::from)
            .collect()
    }
    fn query_support_legacy_value(
        &self,
        format: &crate::SdpVideoFormat,
        reference_scaling: bool,
    ) -> Result<CodecSupport, crate::VideoFactoryQueryError> {
        self.query_support_value(format, reference_scaling, None)
    }
    fn query_support_value(
        &self,
        format: &crate::SdpVideoFormat,
        reference_scaling: bool,
        resolution: Option<&crate::Resolution>,
    ) -> Result<CodecSupport, crate::VideoFactoryQueryError> {
        Ok(self.query_support(
            &VideoCodecFormat::try_from(format)?,
            reference_scaling,
            crate::video_factory_values::legacy_resolution(resolution)?,
        ))
    }

    fn query_support(
        &self,
        format: &VideoCodecFormat,
        reference_scaling: bool,
        resolution: Option<VideoResolution>,
    ) -> CodecSupport;
    fn create(&self, format: &VideoCodecFormat) -> Result<Box<dyn VideoDecoder>, CodecError>;
    fn create_value(
        &self,
        format: &crate::SdpVideoFormat,
    ) -> Result<Box<dyn VideoDecoder>, CodecError> {
        self.create(&VideoCodecFormat::try_from(format).map_err(CodecError::FormatProjection)?)
    }
    fn create_with_context(
        &self,
        context: crate::VideoCodecCreationContext,
    ) -> Result<Box<dyn VideoDecoder>, CodecError> {
        self.create_value(&context.format)
    }
}

/// Complete native loss notification. Absence and a present false flag are
/// different SDK observations; RTP timestamps retain their wrapping bit values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoEncoderLossNotification {
    pub timestamp_of_last_decodable: u32,
    pub timestamp_of_last_received: u32,
    pub dependencies_of_last_received_decodable: Option<bool>,
    pub last_received_decodable: Option<bool>,
}
impl VideoEncoderLossNotification {
    pub(crate) fn native(self) -> ffi::FfiVideoEncoderLossNotification {
        ffi::FfiVideoEncoderLossNotification {
            last_decodable: self.timestamp_of_last_decodable,
            last_received: self.timestamp_of_last_received,
            has_dependencies_decodable: self.dependencies_of_last_received_decodable.is_some(),
            dependencies_decodable: self
                .dependencies_of_last_received_decodable
                .unwrap_or(false),
            has_received_decodable: self.last_received_decodable.is_some(),
            received_decodable: self.last_received_decodable.unwrap_or(false),
        }
    }
    fn from_native(value: ffi::FfiVideoEncoderLossNotification) -> Self {
        Self {
            timestamp_of_last_decodable: value.last_decodable,
            timestamp_of_last_received: value.last_received,
            dependencies_of_last_received_decodable: value
                .has_dependencies_decodable
                .then_some(value.dependencies_decodable),
            last_received_decodable: value
                .has_received_decodable
                .then_some(value.received_decodable),
        }
    }
}
pub trait VideoEncoder: Send + 'static {
    /// Native default is a no-op. Borrowed aliases retire on replacement/release.
    fn set_fec_controller_override(
        &mut self,
        _value: Option<crate::FecControllerOverride>,
    ) -> Result<(), CodecError> {
        Ok(())
    }
    /// Full native initialization with exact integer result. The default is an
    /// explicit checked compatibility projection to the legacy provider API.
    fn initialize_value(
        &mut self,
        codec: crate::OwnedVideoCodec,
        settings: crate::VideoEncoderSettingsValue,
    ) -> Result<i32, CodecError> {
        let v = codec.fields();
        let bps = |kbps: u32| {
            kbps.checked_mul(1000)
                .ok_or(CodecError::InvalidConfiguration)
        };
        let streams = v
            .simulcast_streams
            .get(..usize::from(v.number_of_simulcast_streams))
            .ok_or(CodecError::InvalidConfiguration)?;
        self.initialize(VideoEncoderSettings {
            width: u32::from(v.width),
            height: u32::from(v.height),
            start_bitrate_bps: bps(v.start_bitrate_kbps)?,
            max_bitrate_bps: bps(v.max_bitrate_kbps)?,
            min_bitrate_bps: bps(v.min_bitrate_kbps)?,
            max_framerate: v.max_framerate,
            cores: u32::try_from(settings.number_of_cores)
                .map_err(|_| CodecError::InvalidConfiguration)?,
            max_payload_size: u32::try_from(settings.max_payload_size)
                .map_err(|_| CodecError::InvalidConfiguration)?,
            scalability_mode: v.scalability_mode,
            h264_temporal_layers: match v.codec_specific {
                Some(crate::VideoCodecSpecificConfiguration::H264 {
                    temporal_layers, ..
                }) => Some(temporal_layers),
                _ => None,
            },
            simulcast_temporal_layers: streams.iter().map(|s| s.temporal_layers).collect(),
            simulcast_streams: streams
                .iter()
                .map(|s| {
                    Ok(VideoSimulcastStream {
                        width: u32::try_from(s.width)
                            .map_err(|_| CodecError::InvalidConfiguration)?,
                        height: u32::try_from(s.height)
                            .map_err(|_| CodecError::InvalidConfiguration)?,
                        active: s.active,
                        temporal_layers: s.temporal_layers,
                    })
                })
                .collect::<Result<_, CodecError>>()?,
        })?;
        Ok(CODEC_OK)
    }
    /// Complete raw input, including frame-type-vector absence versus empty.
    /// Override for full metadata; the default deliberately projects to I420.
    fn encode_value(
        &mut self,
        frame: crate::RawVideoFrame,
        types: Option<Vec<crate::EncodedImageFrameType>>,
        callback: EncodedImageCallback,
    ) -> Result<i32, CodecError> {
        let legacy = frame.to_legacy().map_err(|_| CodecError::InvalidFrame)?;
        let types = types
            .unwrap_or_default()
            .into_iter()
            .map(|v| match v {
                crate::EncodedImageFrameType::Key => Ok(VideoFrameType::Key),
                crate::EncodedImageFrameType::Delta => Ok(VideoFrameType::Delta),
                _ => Err(CodecError::InvalidFrame),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let token = frame
            .metadata()
            .presentation_timestamp
            .and_then(|v| v.microseconds().ok());
        self.encode_with_presentation_token(legacy, &types, token, callback)
            .map(|()| CODEC_OK)
    }
    /// Native SetRates is void; this Result is callback-failure evidence only.
    fn set_rates_value(&mut self, rates: crate::OwnedVideoRateControl) -> Result<(), CodecError> {
        let v = rates.fields();
        let bandwidth_bps = match v.bandwidth_allocation {
            crate::DataRate::BitsPerSecond(v) => {
                u64::try_from(v).map_err(|_| CodecError::InvalidConfiguration)?
            }
            _ => return Err(CodecError::InvalidConfiguration),
        };
        self.set_rates(VideoRateControl {
            bitrate_bps: v
                .effective
                .sum_bps()
                .ok_or(CodecError::InvalidConfiguration)?,
            target_bitrate_bps: v.target.sum_bps().ok_or(CodecError::InvalidConfiguration)?,
            framerate_fps: v.framerate_fps,
            bandwidth_bps,
            bandwidth_limited: v.effective.bandwidth_limited,
            target_bandwidth_limited: v.target.bandwidth_limited,
            layer_bitrates_bps: v.effective.layer_bitrates_bps,
            target_layer_bitrates_bps: v.target.layer_bitrates_bps,
        })
    }
    /// Native defaults are no-ops. Errors here are bridge-observable callback
    /// failures, not invented statuses for the SDK's void feedback methods.
    fn on_packet_loss_rate_update(&mut self, _packet_loss_rate: f32) -> Result<(), CodecError> {
        Ok(())
    }
    fn on_rtt_update(&mut self, _rtt_ms: i64) -> Result<(), CodecError> {
        Ok(())
    }
    fn on_loss_notification(
        &mut self,
        _notification: VideoEncoderLossNotification,
    ) -> Result<(), CodecError> {
        Ok(())
    }
    /// Actual nullable native registration, including a handle before input.
    fn register_callback_value(
        &mut self,
        _callback: Option<EncodedImageCallback>,
    ) -> Result<i32, CodecError> {
        self.register_callback().map(|()| CODEC_OK)
    }
    fn release_value(&mut self) -> Result<i32, CodecError> {
        self.release().map(|()| CODEC_OK)
    }
    fn initialize(&mut self, settings: VideoEncoderSettings) -> Result<(), CodecError>;
    fn register_callback(&mut self) -> Result<(), CodecError> {
        Ok(())
    }
    fn encode(
        &mut self,
        frame: VideoFrame,
        frame_types: &[VideoFrameType],
        callback: EncodedImageCallback,
    ) -> Result<(), CodecError>;
    /// The presentation token is reserved for internal encoded-source
    /// correlation. External encoders can use the default raw-frame path.
    fn encode_with_presentation_token(
        &mut self,
        frame: VideoFrame,
        frame_types: &[VideoFrameType],
        presentation_token: Option<i64>,
        callback: EncodedImageCallback,
    ) -> Result<(), CodecError> {
        let _ = presentation_token;
        self.encode(frame, frame_types, callback)
    }
    fn set_rates(&mut self, rates: VideoRateControl) -> Result<(), CodecError>;
    fn release(&mut self) -> Result<(), CodecError>;
    fn info(&self) -> VideoEncoderInfo;
    /// Complete byte-preserving SDK info. This default explicitly projects the
    /// legacy capability DTO onto actual SDK defaults; override for all fields.
    fn info_value(&self) -> Result<crate::OwnedVideoEncoderInfo, CodecError> {
        Ok(self.info().into_native())
    }
}

pub trait VideoDecoder: Send + 'static {
    fn configure(&mut self, _settings: VideoDecoderSettings) -> Result<(), CodecError> {
        Err(CodecError::UnsupportedFormat)
    }
    /// Complete SDK configuration. Override for full-value products. The default
    /// is an explicit compatibility projection to the older provider contract;
    /// negative dimensions/cores cannot be represented and are refused, not clamped.
    fn configure_value(
        &mut self,
        settings: crate::OwnedVideoDecoderSettings,
    ) -> Result<bool, CodecError> {
        let fields = settings.fields();
        let cores =
            u32::try_from(fields.number_of_cores).map_err(|_| CodecError::InvalidConfiguration)?;
        let width =
            u32::try_from(fields.max_render_width).map_err(|_| CodecError::InvalidConfiguration)?;
        let height = u32::try_from(fields.max_render_height)
            .map_err(|_| CodecError::InvalidConfiguration)?;
        self.configure(VideoDecoderSettings {
            cores,
            max_resolution: resolution(width != 0 && height != 0, width, height),
        })?;
        Ok(true)
    }
    /// Separate deprecated SDK name operation. Native C strings cannot contain
    /// interior NULs; their bytes need not be UTF-8. The SDK default is unknown.
    fn implementation_name(&self) -> Result<std::ffi::CString, CodecError> {
        Ok(c"unknown".to_owned())
    }
    /// Fresh complete info, including byte-preserving native names.
    fn info_value(&self) -> Result<crate::OwnedVideoDecoderInfo, CodecError> {
        let legacy = self.info();
        let mut result = crate::OwnedVideoDecoderInfo::default();
        result.set_fields(crate::VideoDecoderInfoFields {
            implementation_name: legacy.implementation_name.into_bytes(),
            is_hardware_accelerated: legacy.hardware_accelerated,
        });
        Ok(result)
    }
    fn register_callback(&mut self) -> Result<(), CodecError> {
        Ok(())
    }
    /// Native registration, including explicit null. Retained callback aliases
    /// become retired before replacement/release; they never target a new slot.
    fn register_callback_value(
        &mut self,
        _callback: Option<DecodedImageCallback>,
    ) -> Result<i32, CodecError> {
        self.register_callback().map(|()| CODEC_OK)
    }
    /// Complete owning input and exact SDK integer result. This default is an
    /// explicit projection to the older provider contract, which cannot carry
    /// render time or additional encoded metadata. Override for full dispatch.
    fn decode_value(
        &mut self,
        frame: crate::EncodedImage,
        render_time_ms: i64,
        callback: DecodedImageCallback,
    ) -> Result<i32, CodecError> {
        self.decode_with_missing_frames_value(frame, false, render_time_ms, callback)
    }
    /// Independent deprecated overload; the SDK-provided missing flag is not
    /// inferred from payload contents. Override to observe it and render time.
    fn decode_with_missing_frames_value(
        &mut self,
        frame: crate::EncodedImage,
        _missing_frames: bool,
        _render_time_ms: i64,
        callback: DecodedImageCallback,
    ) -> Result<i32, CodecError> {
        let metadata = frame.metadata();
        let frame_type = match metadata.frame_type {
            crate::EncodedImageFrameType::Key => VideoFrameType::Key,
            crate::EncodedImageFrameType::Delta => VideoFrameType::Delta,
            _ => return Err(CodecError::InvalidFrame),
        };
        let qp = if metadata.qp == -1 {
            None
        } else {
            Some(u8::try_from(metadata.qp).map_err(|_| CodecError::InvalidFrame)?)
        };
        self.decode(
            EncodedVideoFrame {
                data: frame.to_vec(),
                width: metadata.width,
                height: metadata.height,
                rtp_timestamp: metadata.rtp_timestamp,
                frame_type,
                qp,
            },
            callback,
        )
        .map(|()| CODEC_OK)
    }
    fn decode(
        &mut self,
        _frame: EncodedVideoFrame,
        _callback: DecodedImageCallback,
    ) -> Result<(), CodecError> {
        Err(CodecError::UnsupportedFormat)
    }
    /// Exact native Release result; default is the legacy unit-status projection.
    fn release_value(&mut self) -> Result<i32, CodecError> {
        self.release().map(|()| CODEC_OK)
    }
    fn release(&mut self) -> Result<(), CodecError>;
    fn info(&self) -> VideoDecoderInfo {
        VideoDecoderInfo {
            implementation_name: "unknown".into(),
            hardware_accelerated: false,
        }
    }
}

#[derive(Clone)]
pub struct EncodedImageCallback {
    native: cxx::SharedPtr<ffi::NativeEncodedImageCallback>,
}

// SAFETY: registration-specific admission counts calls and retires before the
// SDK target is released. No bridge mutex spans SDK/user callback invocation.
unsafe impl Send for EncodedImageCallback {}
unsafe impl Sync for EncodedImageCallback {}

impl EncodedImageCallback {
    /// Complete owning output and exact callback result. Retirement is a bridge
    /// error, not a fabricated native ERROR_SEND_FAILED result.
    pub fn encoded(
        &self,
        image: &crate::EncodedImage,
        info: Option<&crate::OwnedCodecSpecificInfo>,
    ) -> Result<crate::EncodedImageCallbackResult, CodecError> {
        let native = self
            .native
            .as_ref()
            .ok_or(CodecError::CallbackUnavailable)?;
        let pointer = info.map_or(std::ptr::null(), |v| v.native() as *const _);
        // SAFETY: the optional metadata is borrowed only for synchronous dispatch.
        let result = unsafe { ffi::encoded_callback_frame(native, image.native(), pointer) };
        if result.invoked {
            Ok(crate::EncodedImageCallbackResult::from_native(result))
        } else {
            Err(CodecError::CallbackUnavailable)
        }
    }
    pub fn frame_dropped(
        &self,
        rtp_timestamp: u32,
        spatial_id: i32,
        end_of_temporal_unit: bool,
    ) -> Result<(), CodecError> {
        let native = self
            .native
            .as_ref()
            .ok_or(CodecError::CallbackUnavailable)?;
        if ffi::encoded_callback_dropped(native, rtp_timestamp, spatial_id, end_of_temporal_unit) {
            Ok(())
        } else {
            Err(CodecError::CallbackUnavailable)
        }
    }
    pub fn same_registration(&self, other: &Self) -> bool {
        match (self.native.as_ref(), other.native.as_ref()) {
            (Some(a), Some(b)) => ffi::encoded_callback_same(a, b),
            _ => false,
        }
    }
    pub fn is_retired(&self) -> bool {
        self.native
            .as_ref()
            .is_none_or(ffi::encoded_callback_retired)
    }
    pub fn emit(&self, frame: &EncodedVideoFrame) -> Result<(), CodecError> {
        self.emit_inner(frame, None)
    }

    pub fn emit_with_metadata(
        &self,
        frame: &EncodedVideoFrame,
        metadata: EncodedVideoMetadata,
    ) -> Result<(), CodecError> {
        self.emit_inner(frame, Some(metadata))
    }

    fn emit_inner(
        &self,
        frame: &EncodedVideoFrame,
        metadata: Option<EncodedVideoMetadata>,
    ) -> Result<(), CodecError> {
        let native = self
            .native
            .as_ref()
            .ok_or(CodecError::CallbackUnavailable)?;
        ffi::encoded_callback_emit(
            native,
            &frame.data,
            frame.width,
            frame.height,
            frame.rtp_timestamp,
            frame.frame_type == VideoFrameType::Key,
            frame.qp.map_or(-1, i32::from),
            &metadata.map_or_else(
                || ffi::FfiEncodedVideoMetadata {
                    codec: 0,
                    simulcast_index: -1,
                    spatial_index: -1,
                    temporal_index: -1,
                    end_of_picture: true,
                    non_reference: false,
                    layer_sync: false,
                    key_index: -1,
                    first_frame_in_picture: false,
                    inter_picture_predicted: false,
                    flexible_mode: false,
                    num_spatial_layers: 1,
                    inter_layer_predicted: false,
                    temporal_up_switch: false,
                },
                EncodedVideoMetadata::ffi,
            ),
        )
        .then_some(())
        .ok_or(CodecError::Released)
    }
}

#[derive(Clone)]
pub struct DecodedImageCallback {
    native: cxx::SharedPtr<ffi::NativeDecodedImageCallback>,
}

// SAFETY: a registration-specific slot counts admitted calls and retires before
// releasing the borrowed SDK target. No bridge lock spans native invocation.
unsafe impl Send for DecodedImageCallback {}
unsafe impl Sync for DecodedImageCallback {}

impl DecodedImageCallback {
    pub fn same_registration(&self, other: &Self) -> bool {
        match (self.native.as_ref(), other.native.as_ref()) {
            (Some(a), Some(b)) => ffi::decoded_callback_same(a, b),
            _ => false,
        }
    }
    pub fn is_retired(&self) -> bool {
        self.native
            .as_ref()
            .is_none_or(ffi::decoded_callback_retired)
    }
    fn full_frame(
        &self,
        frame: &mut crate::RawVideoFrame,
        overload: u8,
        time_ms: i64,
        optional_time_ms: Option<i32>,
        qp: Option<u8>,
    ) -> Result<i32, CodecError> {
        let native = self
            .native
            .as_ref()
            .ok_or(CodecError::CallbackUnavailable)?;
        let result = ffi::decoded_callback_frame(
            native,
            frame.native.pin_mut(),
            overload,
            time_ms,
            optional_time_ms.is_some(),
            optional_time_ms.unwrap_or(0),
            qp.is_some(),
            qp.unwrap_or(0),
        );
        if result.invoked {
            Ok(result.status)
        } else {
            Err(CodecError::CallbackUnavailable)
        }
    }
    /// Actual basic callback and exact native integer result. Native mutations
    /// of the frame remain visible to this caller.
    pub fn decoded(&self, frame: &mut crate::RawVideoFrame) -> Result<i32, CodecError> {
        self.full_frame(frame, 0, 0, None, None)
    }
    /// Independent native int64 overload. Engine consumers apply their own
    /// documented negative/legacy narrowing behavior; no time is synthesized.
    pub fn decoded_with_time(
        &self,
        frame: &mut crate::RawVideoFrame,
        decode_time_ms: i64,
    ) -> Result<i32, CodecError> {
        self.full_frame(frame, 1, decode_time_ms, None, None)
    }
    /// Native optional-time/QP overload is void. Success here means invocation,
    /// not an invented native output status or wire acknowledgement.
    pub fn decoded_with_metadata(
        &self,
        frame: &mut crate::RawVideoFrame,
        decode_time_ms: Option<i32>,
        qp: Option<u8>,
    ) -> Result<(), CodecError> {
        self.full_frame(frame, 2, 0, decode_time_ms, qp).map(|_| ())
    }
    /// Explicit compatibility path for the old packed-I420 frame value.
    pub fn emit(&self, frame: &VideoFrame) -> Result<(), CodecError> {
        let native = self
            .native
            .as_ref()
            .ok_or(CodecError::CallbackUnavailable)?;
        ffi::decoded_callback_emit(
            native,
            frame.buffer.as_bytes(),
            frame.width,
            frame.height,
            frame.timestamp_us,
            frame.rtp_timestamp,
            frame.rotation as u16,
        )
        .then_some(())
        .ok_or(CodecError::Released)
    }
}

pub struct RustVideoEncoderFactory {
    state: Arc<RustVideoEncoderFactoryState>,
}
pub struct RustVideoEncoderFactoryState {
    provider: Option<Box<dyn VideoEncoderFactory>>,
    terminal: AtomicBool,
    failure: crate::VideoFactoryFailureObserver,
}
impl std::ops::Deref for RustVideoEncoderFactory {
    type Target = RustVideoEncoderFactoryState;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}
pub(crate) fn encoder_factory_clone(
    source: &RustVideoEncoderFactory,
) -> Box<RustVideoEncoderFactory> {
    Box::new(RustVideoEncoderFactory {
        state: source.state.clone(),
    })
}
pub struct RustVideoDecoderFactory {
    state: Arc<RustVideoDecoderFactoryState>,
}
pub struct RustVideoDecoderFactoryState {
    provider: Option<Box<dyn VideoDecoderFactory>>,
    terminal: AtomicBool,
    failure: crate::VideoFactoryFailureObserver,
}
impl std::ops::Deref for RustVideoDecoderFactory {
    type Target = RustVideoDecoderFactoryState;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}
pub(crate) fn decoder_factory_clone(
    source: &RustVideoDecoderFactory,
) -> Box<RustVideoDecoderFactory> {
    Box::new(RustVideoDecoderFactory {
        state: source.state.clone(),
    })
}
pub struct RustVideoEncoder {
    codec: Option<Box<dyn VideoEncoder>>,
    terminal: AtomicBool,
}
pub struct RustVideoDecoder {
    codec: Option<Box<dyn VideoDecoder>>,
    terminal: AtomicBool,
}

pub(crate) use crate::panic_payload::discard as discard_panic_payload;
// No application destructor or panic-payload destructor may unwind through CXX.
fn contained_video_drop<T>(value: T) -> bool {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    match catch_unwind(AssertUnwindSafe(|| drop(value))) {
        Ok(()) => true,
        Err(payload) => {
            discard_panic_payload(payload);
            false
        }
    }
}
impl Drop for RustVideoEncoderFactoryState {
    fn drop(&mut self) {
        if !contained_video_drop(self.provider.take()) {
            self.failure
                .report(crate::VideoFactoryQueryError::ProviderPanicked);
        }
    }
}
impl Drop for RustVideoDecoderFactoryState {
    fn drop(&mut self) {
        if !contained_video_drop(self.provider.take()) {
            self.failure
                .report(crate::VideoFactoryQueryError::ProviderPanicked);
        }
    }
}
impl Drop for RustVideoEncoder {
    fn drop(&mut self) {
        contained_video_drop(self.codec.take());
    }
}
impl Drop for RustVideoDecoder {
    fn drop(&mut self) {
        contained_video_drop(self.codec.take());
    }
}

struct EncoderFactoryInner {
    native: cxx::UniquePtr<ffi::NativeVideoEncoderFactory>,
    direct_encoded: Option<crate::video::DirectEncodedVideo>,
    failure: Option<crate::VideoFactoryFailureObserver>,
}
struct DecoderFactoryInner {
    native: cxx::UniquePtr<ffi::NativeVideoDecoderFactory>,
    builtin_vp8: bool,
    encoded_receive_only: Option<VideoCodecFormat>,
    failure: Option<crate::VideoFactoryFailureObserver>,
}

// SAFETY: the provider is Send + Sync and native factory methods only perform shared calls.
unsafe impl Send for EncoderFactoryInner {}
// SAFETY: see the Send rationale; provider discovery and creation accept shared access.
unsafe impl Sync for EncoderFactoryInner {}
// SAFETY: the provider is Send + Sync and native factory methods only perform shared calls.
unsafe impl Send for DecoderFactoryInner {}
// SAFETY: see the Send rationale; provider discovery and creation accept shared access.
unsafe impl Sync for DecoderFactoryInner {}

#[derive(Clone)]
pub struct VideoEncoderFactoryHandle(Arc<EncoderFactoryInner>);
#[derive(Clone)]
pub struct VideoDecoderFactoryHandle(Arc<DecoderFactoryInner>);

impl VideoEncoderFactoryHandle {
    /// Select the pinned artifact's built-in video encoders. Discovery and
    /// support queries report native capabilities; no codec is substituted.
    /// Absence of a factory selection on a peer builder remains distinct and
    /// does not implicitly enable video codecs.
    pub fn builtin() -> Result<Self, CodecError> {
        let native = ffi::new_builtin_video_encoder_factory();
        if native.is_null() {
            Err(CodecError::ConstructionFailed)
        } else {
            Ok(Self(Arc::new(EncoderFactoryInner {
                native,
                direct_encoded: None,
                failure: None,
            })))
        }
    }

    pub fn new(provider: impl VideoEncoderFactory) -> Result<Self, CodecError> {
        let failure = crate::VideoFactoryFailureObserver::default();
        let native = ffi::new_video_encoder_factory(Box::new(RustVideoEncoderFactory {
            state: Arc::new(RustVideoEncoderFactoryState {
                provider: Some(Box::new(provider)),
                terminal: AtomicBool::new(false),
                failure: failure.clone(),
            }),
        }));
        if native.is_null() {
            Err(CodecError::ConstructionFailed)
        } else {
            Ok(Self(Arc::new(EncoderFactoryInner {
                native,
                direct_encoded: None,
                failure: Some(failure),
            })))
        }
    }
    pub(crate) fn new_direct_encoded(
        provider: impl VideoEncoderFactory,
        codec: crate::video::DirectEncodedVideo,
    ) -> Result<Self, CodecError> {
        let mut handle = Self::new(provider)?;
        Arc::get_mut(&mut handle.0)
            .expect("new factory has one owner")
            .direct_encoded = Some(codec);
        Ok(handle)
    }

    /// Complete owning SDK values, including optional packetization/tx_mode.
    pub fn supported_format_values(&self) -> Vec<crate::SdpVideoFormat> {
        crate::video_codec_values::sdp_video_formats_from_list(ffi::video_encoder_formats_value(
            self.native(),
        ))
    }
    /// Actual selected factory GetImplementations, not a substituted discovery
    /// list. The native default may delegate to GetSupportedFormats.
    pub fn implementation_values(&self) -> Vec<crate::SdpVideoFormat> {
        crate::video_codec_values::sdp_video_formats_from_list(
            ffi::video_encoder_implementations_value(self.native()),
        )
    }
    pub fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        self.try_supported_formats().expect(
            "legacy format projection; use supported_format_values for complete native fields",
        )
    }
    pub fn query_support(
        &self,
        format: &VideoCodecFormat,
        scalability_mode: Option<&str>,
        resolution: Option<VideoResolution>,
    ) -> CodecSupport {
        let resolution = resolution.unwrap_or(VideoResolution {
            width: 0,
            height: 0,
        });
        let result = ffi::video_encoder_query(
            self.native(),
            &to_ffi_format(format),
            scalability_mode.is_some(),
            scalability_mode.unwrap_or(""),
            resolution.width != 0 && resolution.height != 0,
            resolution.width,
            resolution.height,
        );
        CodecSupport {
            supported: result.supported,
            power_efficient: result.power_efficient,
        }
    }
    pub fn failure_observer(&self) -> Option<crate::VideoFactoryFailureObserver> {
        self.0.failure.clone()
    }
    pub(crate) fn native(&self) -> &ffi::NativeVideoEncoderFactory {
        self.0.native.as_ref().expect("validated encoder factory")
    }
    pub(crate) fn same_provider(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

// Wire-format registration remains separate from decoded-pixel capability.
// This sink decoder lets libwebrtc's native reference finder and frame buffer
// select ordered encoded frames for the track's recordable-frame callback. It
// reports successful consumption but never fabricates a decoded image.
struct EncodedReceiveFactory(VideoCodecFormat);

struct EncodedReceiveSinkDecoder;

fn same_video_codec(left: &VideoCodecFormat, right: &VideoCodecFormat) -> bool {
    if left.name == "H264" && right.name == "H264" {
        crate::SdpVideoFormat::from(left).is_same_codec(&crate::SdpVideoFormat::from(right))
    } else {
        left.name == right.name && left.parameters == right.parameters
    }
}

impl VideoDecoder for EncodedReceiveSinkDecoder {
    fn configure(&mut self, _: VideoDecoderSettings) -> Result<(), CodecError> {
        Ok(())
    }

    fn decode(&mut self, _: EncodedVideoFrame, _: DecodedImageCallback) -> Result<(), CodecError> {
        Ok(())
    }

    fn release(&mut self) -> Result<(), CodecError> {
        Ok(())
    }

    fn info(&self) -> VideoDecoderInfo {
        VideoDecoderInfo {
            implementation_name: "native-ordered-encoded-sink".into(),
            hardware_accelerated: false,
        }
    }
}
impl VideoDecoderFactory for EncodedReceiveFactory {
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        vec![self.0.clone()]
    }
    fn query_support(
        &self,
        format: &VideoCodecFormat,
        reference_scaling: bool,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        CodecSupport {
            supported: !reference_scaling && same_video_codec(format, &self.0),
            power_efficient: false,
        }
    }
    fn create(&self, format: &VideoCodecFormat) -> Result<Box<dyn VideoDecoder>, CodecError> {
        if !same_video_codec(format, &self.0) {
            return Err(CodecError::UnsupportedFormat);
        }
        Ok(Box::new(EncodedReceiveSinkDecoder))
    }
}

impl VideoDecoderFactoryHandle {
    /// Select all built-in video decoders available in the pinned artifact.
    /// This is not the single-thread VP8-only selection used by controlled
    /// media. Codec discovery and support stay native and artifact-dependent.
    pub fn builtin() -> Result<Self, CodecError> {
        let native = ffi::new_builtin_video_decoder_factory();
        if native.is_null() {
            Err(CodecError::ConstructionFailed)
        } else {
            Ok(Self(Arc::new(DecoderFactoryInner {
                native,
                builtin_vp8: false,
                encoded_receive_only: None,
                failure: None,
            })))
        }
    }

    pub(crate) fn encoded_receive(format: VideoCodecFormat) -> Result<Self, CodecError> {
        let mut handle = Self::new(EncodedReceiveFactory(format.clone()))?;
        Arc::get_mut(&mut handle.0)
            .expect("new factory has one owner")
            .encoded_receive_only = Some(format);
        Ok(handle)
    }

    pub(crate) fn is_encoded_receive_only(&self) -> bool {
        self.0.encoded_receive_only.is_some()
    }

    /// Select the pinned real libvpx VP8 software decoder, with no other codecs
    /// or hardware fallback. The pinned decoder uses one libvpx thread.
    pub fn builtin_vp8() -> Result<Self, CodecError> {
        let native = ffi::new_builtin_vp8_decoder_factory();
        if native.is_null() {
            Err(CodecError::ConstructionFailed)
        } else {
            Ok(Self(Arc::new(DecoderFactoryInner {
                native,
                builtin_vp8: true,
                encoded_receive_only: None,
                failure: None,
            })))
        }
    }

    /// Observations of actual native configure/decode/output calls. Custom
    /// Rust decoder providers do not produce these builtin-codec observations.
    pub fn decoder_statistics(&self) -> Option<DecoderStatistics> {
        self.0
            .builtin_vp8
            .then(|| DecoderStatistics::from(ffi::video_decoder_statistics(self.native())))
    }

    pub fn new(provider: impl VideoDecoderFactory) -> Result<Self, CodecError> {
        let failure = crate::VideoFactoryFailureObserver::default();
        let native = ffi::new_video_decoder_factory(Box::new(RustVideoDecoderFactory {
            state: Arc::new(RustVideoDecoderFactoryState {
                provider: Some(Box::new(provider)),
                terminal: AtomicBool::new(false),
                failure: failure.clone(),
            }),
        }));
        if native.is_null() {
            Err(CodecError::ConstructionFailed)
        } else {
            Ok(Self(Arc::new(DecoderFactoryInner {
                native,
                builtin_vp8: false,
                encoded_receive_only: None,
                failure: Some(failure),
            })))
        }
    }
    pub fn supported_format_values(&self) -> Vec<crate::SdpVideoFormat> {
        crate::video_codec_values::sdp_video_formats_from_list(ffi::video_decoder_formats_value(
            self.native(),
        ))
    }
    pub fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        self.try_supported_formats().expect(
            "legacy format projection; use supported_format_values for complete native fields",
        )
    }
    pub fn query_support(
        &self,
        format: &VideoCodecFormat,
        reference_scaling: bool,
        resolution: Option<VideoResolution>,
    ) -> CodecSupport {
        let resolution = resolution.unwrap_or(VideoResolution {
            width: 0,
            height: 0,
        });
        let result = ffi::video_decoder_query(
            self.native(),
            &to_ffi_format(format),
            reference_scaling,
            resolution.width != 0 && resolution.height != 0,
            resolution.width,
            resolution.height,
        );
        CodecSupport {
            supported: result.supported,
            power_efficient: result.power_efficient,
        }
    }
    pub fn failure_observer(&self) -> Option<crate::VideoFactoryFailureObserver> {
        self.0.failure.clone()
    }
    pub(crate) fn native(&self) -> &ffi::NativeVideoDecoderFactory {
        self.0.native.as_ref().expect("validated decoder factory")
    }
}

struct AudioEncoderFactoryInner {
    native: cxx::UniquePtr<ffi::NativeAudioEncoderFactory>,
    opus_frames: bool,
}
struct AudioDecoderFactoryInner {
    native: cxx::UniquePtr<ffi::NativeAudioDecoderFactory>,
    builtin_opus: bool,
}
// SAFETY: upstream audio codec factories are ref-counted, thread-safe factory interfaces.
unsafe impl Send for AudioEncoderFactoryInner {}
// SAFETY: only the immutable factory handle is exposed.
unsafe impl Sync for AudioEncoderFactoryInner {}
// SAFETY: upstream audio codec factories are ref-counted, thread-safe factory interfaces.
unsafe impl Send for AudioDecoderFactoryInner {}
// SAFETY: only the immutable factory handle is exposed.
unsafe impl Sync for AudioDecoderFactoryInner {}

#[derive(Clone)]
pub struct AudioEncoderFactory(Arc<AudioEncoderFactoryInner>);
#[derive(Clone)]
pub struct AudioDecoderFactory(Arc<AudioDecoderFactoryInner>);

impl AudioEncoderFactory {
    /// Construct an Opus-only peer audio encoder factory for prerecorded
    /// mono or stereo Opus packets. Raw PCM tracks require a separate peer factory.
    /// The sender passes bytes through without invoking an Opus encoder.
    /// Mono/stereo share one advertised `opus/48000/2` RTP codec. The remote
    /// receiver's `stereo` fmtp preference selects the carrier channel count.
    pub fn with_opus_frames() -> Result<Self, CodecError> {
        let native = ffi::new_opus_carrier_audio_encoder_factory();
        if native.is_null() {
            Err(CodecError::ConstructionFailed)
        } else {
            Ok(Self(Arc::new(AudioEncoderFactoryInner {
                native,
                opus_frames: true,
            })))
        }
    }

    pub fn builtin() -> Result<Self, CodecError> {
        let native = ffi::new_builtin_audio_encoder_factory();
        if native.is_null() {
            Err(CodecError::ConstructionFailed)
        } else {
            Ok(Self(Arc::new(AudioEncoderFactoryInner {
                native,
                opus_frames: false,
            })))
        }
    }
    /// Prioritized native discovery list. Query/Create may accept additional
    /// formats not advertised here. Carrier discovery preserves its single
    /// canonical Opus RTP identity and actual mono codec metadata.
    pub fn supported_encoders(&self) -> Vec<crate::AudioCodecSpec> {
        ffi::audio_encoder_formats(self.native())
            .into_iter()
            .map(crate::AudioCodecSpec::from_native)
            .collect()
    }
    /// Native capability query, including non-advertised formats. Safety errors
    /// are distinct from the selected factory's ordinary unsupported result.
    pub fn query_encoder(
        &self,
        format: &crate::SdpAudioFormat,
    ) -> Result<Option<crate::AudioCodecInfo>, CodecError> {
        let result = ffi::audio_encoder_query(self.native(), &format.to_native());
        if !result.valid {
            return Err(CodecError::InvalidConfiguration);
        }
        Ok(result
            .present
            .then(|| crate::AudioCodecInfo::from_native(result.info)))
    }
    /// Create an owning encoder in the exact supplied environment. Neither an
    /// unsupported format nor a native null result is replaced with a fallback.
    pub fn create(
        &self,
        environment: &crate::Environment,
        format: &crate::SdpAudioFormat,
        options: &crate::AudioEncoderOptions,
    ) -> Result<Option<crate::AudioEncoder>, CodecError> {
        let absent = cxx::UniquePtr::null();
        let pair_id = options
            .codec_pair_id
            .as_ref()
            .map_or(&absent, crate::AudioCodecPairId::native);
        let result = ffi::audio_encoder_create(
            self.native(),
            environment.native(),
            &format.to_native(),
            options.payload_type,
            pair_id,
        );
        if !result.valid {
            return Err(CodecError::InvalidConfiguration);
        }
        if result.encoder.is_null() {
            return Ok(None);
        }
        Ok(Some(crate::AudioEncoder::from_native(
            result.encoder,
            environment.clone(),
        )))
    }
    /// Create using an independently owned SDK Options value, without projecting
    /// it through Rust fields. Backend-specific admission and native unsupported
    /// results are the same as `create`; the supplied options remain unmodified.
    pub fn create_with_options(
        &self,
        environment: &crate::Environment,
        format: &crate::SdpAudioFormat,
        options: &crate::OwnedAudioEncoderOptions,
    ) -> Result<Option<crate::AudioEncoder>, CodecError> {
        let result = ffi::audio_encoder_create_with_options(
            self.native(),
            environment.native(),
            &format.to_native(),
            options.native(),
        );
        if !result.valid {
            return Err(CodecError::InvalidConfiguration);
        }
        if result.encoder.is_null() {
            return Ok(None);
        }
        Ok(Some(crate::AudioEncoder::from_native(
            result.encoder,
            environment.clone(),
        )))
    }
    pub(crate) fn supports_opus_frames(&self) -> bool {
        self.0.opus_frames
    }
    pub fn is_available(&self) -> bool {
        !self.0.native.is_null()
    }
    /// Frames dropped at the Opus carrier's native sender handoff because
    /// callback/queue identity or a supported metadata setter was unavailable.
    /// This is observed adapter failure, not a submitted-frame echo or delivery
    /// receipt. Builtin (non-carrier) factories return zero.
    pub fn opus_frame_handoff_failures(&self) -> u64 {
        ffi::audio_encoder_opus_handoff_failures(self.native())
    }

    pub(crate) fn native(&self) -> &ffi::NativeAudioEncoderFactory {
        self.0
            .native
            .as_ref()
            .expect("validated audio encoder factory")
    }
}
impl AudioDecoderFactory {
    /// Internal exact SDK factory view for creation-provider contexts. This
    /// owns the supplied refcounted factory, not a separately selected builtin.
    /// SDK-null remains absent. Selected builtin capacity and telemetry evidence
    /// transfer only after native comparison with the configured factory identity.
    /// An unproven reference is retained, but stock parser/decoder operations
    /// reject it before invoking a backend with unjustified capacity assumptions.
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeAudioDecoderFactory>,
    ) -> Option<Self> {
        let factory = native.as_ref()?;
        let builtin_opus = ffi::audio_decoder_factory_builtin_kind(factory) == 2;
        Some(Self(Arc::new(AudioDecoderFactoryInner {
            native,
            builtin_opus,
        })))
    }
    /// Select only the pinned real software Opus decoder. Parsed primary and
    /// FEC frames retain their decoder and preserve upstream decode behavior.
    pub fn builtin_opus() -> Result<Self, CodecError> {
        let native = ffi::new_builtin_opus_decoder_factory();
        if native.is_null() {
            Err(CodecError::ConstructionFailed)
        } else {
            Ok(Self(Arc::new(AudioDecoderFactoryInner {
                native,
                builtin_opus: true,
            })))
        }
    }
    /// Complete native advertised formats and implementation metadata. This is
    /// an owned snapshot, not inferred from selected Opus configuration.
    pub fn supported_decoders(&self) -> Vec<crate::AudioCodecSpec> {
        ffi::audio_decoder_formats(self.native())
            .into_iter()
            .map(crate::AudioCodecSpec::from_native)
            .collect()
    }

    /// Returns a native capability result, or an error before the stock
    /// parser reaches an aborting conversion or overflowing configuration.
    /// Full SDP values remain representable independently of parser safety.
    pub fn is_supported_decoder(&self, format: &crate::SdpAudioFormat) -> Result<bool, CodecError> {
        match ffi::audio_decoder_supported(self.native(), &format.to_native()) {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CodecError::InvalidConfiguration),
        }
    }

    /// Create a current owning decoder product. Native unsupported formats and
    /// null constructor results remain `Ok(None)`; unsafe parser inputs error.
    /// Products and parsed frames retain their environment independently.
    pub fn create(
        &self,
        environment: &crate::Environment,
        format: &crate::SdpAudioFormat,
    ) -> Result<Option<crate::AudioDecoder>, CodecError> {
        let value =
            ffi::audio_decoder_create(self.native(), environment.native(), &format.to_native());
        Self::created_decoder(value, environment)
    }

    /// Call the distinct deprecated factory overload with exact optional pair
    /// identity. This does not expose the unsafe obsolete decoder decode APIs.
    pub fn create_with_codec_pair_id(
        &self,
        environment: &crate::Environment,
        format: &crate::SdpAudioFormat,
        codec_pair_id: Option<&crate::AudioCodecPairId>,
    ) -> Result<Option<crate::AudioDecoder>, CodecError> {
        let absent = cxx::UniquePtr::null();
        let pair_id = codec_pair_id.map_or(&absent, crate::AudioCodecPairId::native);
        let value = ffi::audio_decoder_create_with_pair_id(
            self.native(),
            environment.native(),
            &format.to_native(),
            pair_id,
        );
        Self::created_decoder(value, environment)
    }

    fn created_decoder(
        value: ffi::FfiAudioDecoderCreation,
        environment: &crate::Environment,
    ) -> Result<Option<crate::AudioDecoder>, CodecError> {
        if !value.valid {
            return Err(CodecError::InvalidConfiguration);
        }
        Ok(if value.decoder.is_null() {
            None
        } else {
            Some(crate::AudioDecoder::from_native(
                value.decoder,
                environment.clone(),
            ))
        })
    }

    pub fn decoder_statistics(&self) -> Option<DecoderStatistics> {
        self.0
            .builtin_opus
            .then(|| DecoderStatistics::from(ffi::audio_decoder_statistics(self.native())))
    }

    pub fn builtin() -> Result<Self, CodecError> {
        let native = ffi::new_builtin_audio_decoder_factory();
        if native.is_null() {
            Err(CodecError::ConstructionFailed)
        } else {
            Ok(Self(Arc::new(AudioDecoderFactoryInner {
                native,
                builtin_opus: false,
            })))
        }
    }
    pub fn is_available(&self) -> bool {
        !self.0.native.is_null()
    }
    pub(crate) fn native(&self) -> &ffi::NativeAudioDecoderFactory {
        self.0
            .native
            .as_ref()
            .expect("validated audio decoder factory")
    }
}

/// Owned observations from the selected real software decoder factories.
/// Counts include parsed audio frame decoding, not only legacy decode entry
/// points. Thread tokens are process-local diagnostic identities, not replay
/// values; mismatches compare actual native thread identities without hashing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DecoderStatistics {
    pub decoder_creations: u64,
    pub configure_calls: u64,
    pub decode_calls: u64,
    /// Decoder-input packet/access-unit count (audio parsing includes FEC).
    pub input_packets: u64,
    /// FNV-1a of the most recent compressed decoder input, for observing
    /// byte-preserving encoded input. This is not a cryptographic checksum.
    pub last_input_hash: u64,
    pub decoded_outputs: u64,
    pub decode_errors: u64,
    pub thread_token: u64,
    pub thread_mismatches: u64,
}

impl DecoderStatistics {
    pub fn current_thread_token() -> u64 {
        ffi::native_codec_thread_token()
    }
}

impl From<ffi::FfiDecoderStatistics> for DecoderStatistics {
    fn from(value: ffi::FfiDecoderStatistics) -> Self {
        Self {
            decoder_creations: value.decoder_creations,
            configure_calls: value.configure_calls,
            decode_calls: value.decode_calls,
            input_packets: value.input_packets,
            last_input_hash: value.last_input_hash,
            decoded_outputs: value.decoded_outputs,
            decode_errors: value.decode_errors,
            thread_token: value.thread_token,
            thread_mismatches: value.thread_mismatches,
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CodecTestResult {
    pub status: i32,
    pub encoded_frames: u32,
    pub decoded_frames: u32,
    pub checksum: u64,
}

impl VideoEncoderFactoryHandle {
    #[cfg(test)]
    fn test_roundtrip(&self, decoder: &VideoDecoderFactoryHandle, frames: u32) -> CodecTestResult {
        let result = ffi::test_codec_roundtrip(self.native(), decoder.native(), frames);
        CodecTestResult {
            status: result.status,
            encoded_frames: result.encoded_frames,
            decoded_frames: result.decoded_frames,
            checksum: result.checksum,
        }
    }
    #[cfg(test)]
    fn test_cross_thread_lifecycle(&self) -> bool {
        ffi::test_encoder_factory_cross_thread(self.native())
    }
}

fn dimensions(width: u32, height: u32) -> Result<(usize, usize), CodecError> {
    if width == 0 || height == 0 {
        return Err(CodecError::InvalidFrame);
    }
    Ok((
        usize::try_from(width).map_err(|_| CodecError::InvalidFrame)?,
        usize::try_from(height).map_err(|_| CodecError::InvalidFrame)?,
    ))
}

fn check_plane(plane: &VideoPlane, row_bytes: usize, rows: usize) -> Result<(), CodecError> {
    if plane.stride < row_bytes || plane.stride.checked_mul(rows) != Some(plane.data.len()) {
        return Err(CodecError::InvalidFrame);
    }
    Ok(())
}

fn append_plane(
    result: &mut Vec<u8>,
    plane: &VideoPlane,
    row_bytes: usize,
    rows: usize,
) -> Result<(), CodecError> {
    check_plane(plane, row_bytes, rows)?;
    for row in plane.data.chunks_exact(plane.stride) {
        result.extend_from_slice(&row[..row_bytes]);
    }
    Ok(())
}

fn i420_len(width: u32, height: u32) -> Result<usize, CodecError> {
    let y = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .ok_or(CodecError::InvalidFrame)?;
    let cw = usize::try_from(width.div_ceil(2)).map_err(|_| CodecError::InvalidFrame)?;
    let ch = usize::try_from(height.div_ceil(2)).map_err(|_| CodecError::InvalidFrame)?;
    y.checked_add(
        cw.checked_mul(ch)
            .and_then(|c| c.checked_mul(2))
            .ok_or(CodecError::InvalidFrame)?,
    )
    .ok_or(CodecError::InvalidFrame)
}

fn to_ffi_format(format: &VideoCodecFormat) -> ffi::FfiCodecFormat {
    ffi::FfiCodecFormat {
        name: format.name.clone(),
        scalability_modes: format
            .scalability_modes
            .iter()
            .map(|mode| mode.as_str().into())
            .collect(),
        parameters: format
            .parameters
            .iter()
            .map(|p| ffi::FfiCodecParameter {
                key: p.key.clone(),
                value: p.value.clone(),
            })
            .collect(),
    }
}
#[cfg(test)]
pub(crate) fn from_ffi_format(format: ffi::FfiCodecFormat) -> VideoCodecFormat {
    VideoCodecFormat {
        name: format.name,
        scalability_modes: format
            .scalability_modes
            .into_iter()
            .map(VideoScalabilityMode::from_native)
            .collect(),
        parameters: format
            .parameters
            .into_iter()
            .map(|p| CodecParameter {
                key: p.key,
                value: p.value,
            })
            .collect(),
    }
}
#[cfg(test)]
fn from_ffi_format_ref(format: &ffi::FfiCodecFormat) -> VideoCodecFormat {
    VideoCodecFormat {
        name: format.name.clone(),
        scalability_modes: format
            .scalability_modes
            .iter()
            .cloned()
            .map(VideoScalabilityMode::from_native)
            .collect(),
        parameters: format
            .parameters
            .iter()
            .map(|parameter| CodecParameter {
                key: parameter.key.clone(),
                value: parameter.value.clone(),
            })
            .collect(),
    }
}
fn resolution(has: bool, width: u32, height: u32) -> Option<VideoResolution> {
    has.then_some(VideoResolution { width, height })
}
fn status(result: Result<(), CodecError>) -> i32 {
    match result {
        Ok(()) => CODEC_OK,
        Err(CodecError::InvalidConfiguration | CodecError::InvalidFrame) => CODEC_PARAMETER,
        Err(CodecError::Released | CodecError::CallbackUnavailable) => CODEC_UNINITIALIZED,
        Err(_) => CODEC_ERROR,
    }
}

pub(crate) fn encoder_factory_selector(
    factory: &RustVideoEncoderFactory,
    valid: &mut bool,
) -> cxx::UniquePtr<ffi::NativeOwnedVideoEncoderSelector> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    *valid = false;
    if factory.terminal.load(Ordering::Acquire) {
        return cxx::UniquePtr::null();
    }
    match catch_unwind(AssertUnwindSafe(|| {
        factory
            .provider
            .as_ref()
            .expect("live video factory")
            .encoder_selector()
    })) {
        Ok(value) => {
            *valid = true;
            value.map_or_else(
                cxx::UniquePtr::null,
                crate::OwnedVideoEncoderSelector::into_native,
            )
        }
        Err(payload) => {
            factory.terminal.store(true, Ordering::Release);
            factory
                .failure
                .report(crate::VideoFactoryQueryError::ProviderPanicked);
            discard_panic_payload(payload);
            cxx::UniquePtr::null()
        }
    }
}
pub(crate) fn encoder_factory_formats(
    factory: &RustVideoEncoderFactory,
) -> cxx::UniquePtr<ffi::NativeSdpVideoFormatList> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    if factory.terminal.load(Ordering::Acquire) {
        return crate::video_factory_values::native_failed_formats();
    }
    match catch_unwind(AssertUnwindSafe(|| {
        factory
            .provider
            .as_ref()
            .expect("live video factory provider")
            .supported_format_values()
    })) {
        Ok(formats) => crate::video_factory_values::native_formats(formats),
        Err(payload) => {
            factory.terminal.store(true, Ordering::Release);
            factory
                .failure
                .report(crate::VideoFactoryQueryError::ProviderPanicked);
            discard_panic_payload(payload);
            crate::video_factory_values::native_failed_formats()
        }
    }
}
pub(crate) fn encoder_factory_implementations(
    factory: &RustVideoEncoderFactory,
) -> cxx::UniquePtr<ffi::NativeSdpVideoFormatList> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    if factory.terminal.load(Ordering::Acquire) {
        return crate::video_factory_values::native_failed_formats();
    }
    match catch_unwind(AssertUnwindSafe(|| {
        factory
            .provider
            .as_ref()
            .expect("live video factory provider")
            .implementation_values()
    })) {
        Ok(formats) => crate::video_factory_values::native_formats(formats),
        Err(payload) => {
            factory.terminal.store(true, Ordering::Release);
            factory
                .failure
                .report(crate::VideoFactoryQueryError::ProviderPanicked);
            discard_panic_payload(payload);
            crate::video_factory_values::native_failed_formats()
        }
    }
}
pub(crate) fn encoder_factory_query(
    factory: &RustVideoEncoderFactory,
    format: cxx::UniquePtr<ffi::NativeSdpVideoFormat>,
    has_scalability_mode: bool,
    scalability_mode: &[u8],
    resolution: cxx::UniquePtr<ffi::NativeRtpResolution>,
    legacy: bool,
) -> ffi::FfiVideoFactorySupport {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    use crate::VideoFactoryQueryError;
    if factory.terminal.load(Ordering::Acquire) {
        return crate::video_factory_values::ffi_support(Err(
            VideoFactoryQueryError::ProviderPanicked,
        ));
    }
    let format = crate::SdpVideoFormat::from_native(format);
    let resolution = (!resolution.is_null()).then(|| crate::Resolution::from_native(resolution));
    let result = match catch_unwind(AssertUnwindSafe(|| {
        let provider = factory
            .provider
            .as_ref()
            .expect("live video factory provider");
        if legacy {
            provider.query_support_legacy_value(
                &format,
                has_scalability_mode.then_some(scalability_mode),
            )
        } else {
            provider.query_support_value(
                &format,
                has_scalability_mode.then_some(scalability_mode),
                resolution.as_ref(),
            )
        }
    })) {
        Ok(result) => result,
        Err(payload) => {
            factory.terminal.store(true, Ordering::Release);
            discard_panic_payload(payload);
            Err(VideoFactoryQueryError::ProviderPanicked)
        }
    };
    if let Err(error) = result {
        factory.failure.report(error);
    }
    crate::video_factory_values::ffi_support(result)
}
pub(crate) fn encoder_factory_create(
    factory: &RustVideoEncoderFactory,
    environment: cxx::UniquePtr<ffi::NativeEnvironment>,
    retention: &crate::video_factory_values::RustVideoFactoryEnvironment,
    format: cxx::UniquePtr<ffi::NativeSdpVideoFormat>,
) -> Box<RustVideoEncoder> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    if factory.terminal.load(Ordering::Acquire) {
        return Box::new(RustVideoEncoder {
            codec: None,
            terminal: AtomicBool::new(true),
        });
    }
    let context = crate::VideoCodecCreationContext {
        environment: crate::Environment::from_sdk(environment, Some(&retention.0)),
        format: crate::SdpVideoFormat::from_native(format),
    };
    match catch_unwind(AssertUnwindSafe(|| {
        factory
            .provider
            .as_ref()
            .expect("live video factory provider")
            .create_with_context(context)
    })) {
        Ok(Ok(codec)) => Box::new(RustVideoEncoder {
            codec: Some(codec),
            terminal: AtomicBool::new(false),
        }),
        Ok(Err(error)) => {
            match error {
                CodecError::UnsupportedFormat => {}
                CodecError::FormatProjection(error) => factory.failure.report(error),
                _ => factory
                    .failure
                    .report(crate::VideoFactoryQueryError::CodecCreationFailed),
            }
            Box::new(RustVideoEncoder {
                codec: None,
                terminal: AtomicBool::new(false),
            })
        }
        Err(payload) => {
            discard_panic_payload(payload);
            factory.terminal.store(true, Ordering::Release);
            factory
                .failure
                .report(crate::VideoFactoryQueryError::ProviderPanicked);
            Box::new(RustVideoEncoder {
                codec: None,
                terminal: AtomicBool::new(true),
            })
        }
    }
}
pub(crate) fn encoder_is_valid(encoder: &RustVideoEncoder) -> bool {
    encoder.codec.is_some() && !encoder.terminal.load(Ordering::Acquire)
}
fn with_encoder(
    encoder: &mut RustVideoEncoder,
    operation: impl FnOnce(&mut dyn VideoEncoder) -> Result<(), CodecError>,
) -> i32 {
    with_encoder_status(encoder, |codec| operation(codec).map(|()| CODEC_OK))
}
fn with_encoder_status(
    encoder: &mut RustVideoEncoder,
    operation: impl FnOnce(&mut dyn VideoEncoder) -> Result<i32, CodecError>,
) -> i32 {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    if encoder.terminal.load(Ordering::Acquire) {
        return CODEC_ERROR;
    }
    let Some(codec) = encoder.codec.as_deref_mut() else {
        return CODEC_UNINITIALIZED;
    };
    match catch_unwind(AssertUnwindSafe(|| operation(codec))) {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => status(Err(error)),
        Err(payload) => {
            discard_panic_payload(payload);
            encoder.terminal.store(true, Ordering::Release);
            CODEC_ERROR
        }
    }
}
fn encoder_settings_from_ffi(s: ffi::FfiEncoderSettings) -> VideoEncoderSettings {
    VideoEncoderSettings {
        width: s.width,
        height: s.height,
        start_bitrate_bps: s.start_bitrate_bps,
        max_bitrate_bps: s.max_bitrate_bps,
        min_bitrate_bps: s.min_bitrate_bps,
        max_framerate: s.max_framerate,
        cores: s.cores,
        max_payload_size: s.max_payload_size,
        scalability_mode: s
            .has_scalability_mode
            .then(|| VideoScalabilityMode::from_native(s.scalability_mode)),
        h264_temporal_layers: s.has_h264_temporal_layers.then_some(s.h264_temporal_layers),
        simulcast_temporal_layers: s.simulcast_temporal_layers,
        simulcast_streams: s
            .simulcast_streams
            .into_iter()
            .map(|stream| VideoSimulcastStream {
                width: stream.width,
                height: stream.height,
                active: stream.active,
                temporal_layers: stream.temporal_layers,
            })
            .collect(),
    }
}
pub(crate) fn encoder_set_fec_override(
    encoder: &mut RustVideoEncoder,
    value: cxx::SharedPtr<ffi::NativeFecOverride>,
) -> i32 {
    with_encoder(encoder, |c| {
        c.set_fec_controller_override(
            (!value.is_null()).then(|| crate::FecControllerOverride::borrowed(value)),
        )
    })
}
pub(crate) fn encoder_init_value(
    encoder: &mut RustVideoEncoder,
    codec: cxx::UniquePtr<ffi::NativeVideoCodec>,
    settings: ffi::FfiOwnedEncoderSettings,
) -> i32 {
    if codec.is_null() {
        return CODEC_PARAMETER;
    }
    with_encoder_status(encoder, |c| {
        c.initialize_value(
            crate::OwnedVideoCodec::from_native(codec),
            crate::VideoEncoderSettingsValue::from_native(settings),
        )
    })
}
pub(crate) fn encoder_encode_value(
    encoder: &mut RustVideoEncoder,
    frame: cxx::UniquePtr<ffi::NativeRawVideoFrame>,
    has_types: bool,
    types: Vec<i32>,
    callback: cxx::SharedPtr<ffi::NativeEncodedImageCallback>,
) -> i32 {
    if frame.is_null() {
        return CODEC_PARAMETER;
    }
    with_encoder_status(encoder, |c| {
        c.encode_value(
            crate::RawVideoFrame::from_decoded_native(frame),
            has_types.then(|| {
                types
                    .into_iter()
                    .map(crate::EncodedImageFrameType::from_native)
                    .collect()
            }),
            EncodedImageCallback { native: callback },
        )
    })
}
pub(crate) fn encoder_set_rates_value(
    encoder: &mut RustVideoEncoder,
    rates: cxx::UniquePtr<ffi::NativeVideoRateControl>,
) -> i32 {
    if rates.is_null() {
        return CODEC_PARAMETER;
    }
    with_encoder(encoder, |c| {
        c.set_rates_value(crate::OwnedVideoRateControl::from_native(rates))
    })
}
pub(crate) fn encoder_init(encoder: &mut RustVideoEncoder, s: ffi::FfiEncoderSettings) -> i32 {
    with_encoder(encoder, |c| c.initialize(encoder_settings_from_ffi(s)))
}
pub(crate) fn encoder_register_callback_value(
    encoder: &mut RustVideoEncoder,
    callback: cxx::SharedPtr<ffi::NativeEncodedImageCallback>,
) -> i32 {
    let callback = (!callback.is_null()).then_some(EncodedImageCallback { native: callback });
    with_encoder_status(encoder, |codec| codec.register_callback_value(callback))
}
pub(crate) fn encoder_register_callback(encoder: &mut RustVideoEncoder) -> i32 {
    with_encoder(encoder, |c| c.register_callback())
}
pub(crate) fn encoder_encode(
    encoder: &mut RustVideoEncoder,
    frame: cxx::UniquePtr<ffi::NativeVideoFrame>,
    frame_types: &[u8],
    presentation_token: i64,
    callback: cxx::SharedPtr<ffi::NativeEncodedImageCallback>,
) -> i32 {
    let Some(native) = frame.as_ref() else {
        return CODEC_PARAMETER;
    };
    let frame = match VideoFrame::i420(
        ffi::native_video_frame_width(native),
        ffi::native_video_frame_height(native),
        ffi::native_video_frame_i420(native),
        ffi::native_video_frame_timestamp_us(native),
        ffi::native_video_frame_rtp_timestamp(native),
    ) {
        Ok(frame) => frame.with_rotation(VideoRotation::from_degrees(
            ffi::native_video_frame_rotation(native),
        )),
        Err(error) => return status(Err(error)),
    };
    let types: Vec<_> = frame_types
        .iter()
        .filter_map(|value| match value {
            3 => Some(VideoFrameType::Key),
            4 => Some(VideoFrameType::Delta),
            _ => None,
        })
        .collect();
    with_encoder(encoder, |c| {
        c.encode_with_presentation_token(
            frame,
            &types,
            (presentation_token != 0).then_some(presentation_token),
            EncodedImageCallback { native: callback },
        )
    })
}
pub(crate) fn encoder_set_rates(encoder: &mut RustVideoEncoder, r: ffi::FfiRateControl) -> i32 {
    with_encoder(encoder, |c| c.set_rates(VideoRateControl::from_ffi(r)))
}
pub(crate) fn encoder_packet_loss(encoder: &mut RustVideoEncoder, rate: f32) -> i32 {
    with_encoder(encoder, |codec| codec.on_packet_loss_rate_update(rate))
}
pub(crate) fn encoder_rtt(encoder: &mut RustVideoEncoder, rtt_ms: i64) -> i32 {
    with_encoder(encoder, |codec| codec.on_rtt_update(rtt_ms))
}
pub(crate) fn encoder_loss_notification(
    encoder: &mut RustVideoEncoder,
    notification: ffi::FfiVideoEncoderLossNotification,
) -> i32 {
    with_encoder(encoder, |codec| {
        codec.on_loss_notification(VideoEncoderLossNotification::from_native(notification))
    })
}
pub(crate) fn encoder_factory_report_feedback_failure(factory: &RustVideoEncoderFactory) {
    factory
        .failure
        .report(crate::VideoFactoryQueryError::CodecFeedbackFailed);
}
pub(crate) fn encoder_release(encoder: &mut RustVideoEncoder) -> i32 {
    let result = with_encoder_status(encoder, |c| c.release_value());
    // SDK Release is not destruction: simulcast contexts can be cached and
    // reinitialized. Only a poisoned provider is destroyed at this boundary.
    if encoder.terminal.load(Ordering::Acquire) && !contained_video_drop(encoder.codec.take()) {
        return CODEC_ERROR;
    }
    result
}
pub(crate) fn encoder_get_info_value(
    encoder: &RustVideoEncoder,
) -> cxx::UniquePtr<ffi::NativeVideoEncoderInfo> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    let Some(codec) = encoder
        .codec
        .as_deref()
        .filter(|_| !encoder.terminal.load(Ordering::Acquire))
    else {
        return cxx::UniquePtr::null();
    };
    match catch_unwind(AssertUnwindSafe(|| codec.info_value())) {
        Ok(Ok(value)) => value.into_native(),
        Ok(Err(_)) => cxx::UniquePtr::null(),
        Err(payload) => {
            discard_panic_payload(payload);
            encoder.terminal.store(true, Ordering::Release);
            cxx::UniquePtr::null()
        }
    }
}
pub(crate) fn encoder_factory_report_codec_info_failure(factory: &RustVideoEncoderFactory) {
    factory
        .failure
        .report(crate::VideoFactoryQueryError::CodecInfoFailed);
}
#[cfg(test)]
pub(crate) fn encoder_get_info(encoder: &RustVideoEncoder) -> ffi::FfiEncoderInfo {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    encoder
        .codec
        .as_deref()
        .filter(|_| !encoder.terminal.load(Ordering::Acquire))
        .and_then(|c| match catch_unwind(AssertUnwindSafe(|| c.info())) {
            Ok(info) => Some(info),
            Err(payload) => {
                discard_panic_payload(payload);
                encoder.terminal.store(true, Ordering::Release);
                None
            }
        })
        .map_or_else(
            || ffi::FfiEncoderInfo {
                implementation_name: "failed Rust encoder".into(),
                hardware_accelerated: false,
                supports_native_handle: false,
                supports_simulcast: false,
                fps_allocation: Vec::new(),
            },
            encoder_info_to_ffi,
        )
}

#[cfg(test)]
fn encoder_info_to_ffi(info: VideoEncoderInfo) -> ffi::FfiEncoderInfo {
    ffi::FfiEncoderInfo {
        implementation_name: info.implementation_name,
        hardware_accelerated: info.hardware_accelerated,
        supports_native_handle: info.supports_native_handle,
        supports_simulcast: info.supports_simulcast,
        fps_allocation: info.fps_allocation.map_or_else(Vec::new, |spatial| {
            spatial
                .into_iter()
                .map(|temporal| ffi::FfiTemporalFrameRates {
                    fractions: temporal,
                })
                .collect()
        }),
    }
}

pub(crate) fn decoder_factory_formats(
    factory: &RustVideoDecoderFactory,
) -> cxx::UniquePtr<ffi::NativeSdpVideoFormatList> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    if factory.terminal.load(Ordering::Acquire) {
        return crate::video_factory_values::native_failed_formats();
    }
    match catch_unwind(AssertUnwindSafe(|| {
        factory
            .provider
            .as_ref()
            .expect("live video factory provider")
            .supported_format_values()
    })) {
        Ok(formats) => crate::video_factory_values::native_formats(formats),
        Err(payload) => {
            factory.terminal.store(true, Ordering::Release);
            factory
                .failure
                .report(crate::VideoFactoryQueryError::ProviderPanicked);
            discard_panic_payload(payload);
            crate::video_factory_values::native_failed_formats()
        }
    }
}
pub(crate) fn decoder_factory_query(
    factory: &RustVideoDecoderFactory,
    format: cxx::UniquePtr<ffi::NativeSdpVideoFormat>,
    reference_scaling: bool,
    resolution: cxx::UniquePtr<ffi::NativeRtpResolution>,
    legacy: bool,
) -> ffi::FfiVideoFactorySupport {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    use crate::VideoFactoryQueryError;
    if factory.terminal.load(Ordering::Acquire) {
        return crate::video_factory_values::ffi_support(Err(
            VideoFactoryQueryError::ProviderPanicked,
        ));
    }
    let format = crate::SdpVideoFormat::from_native(format);
    let resolution = (!resolution.is_null()).then(|| crate::Resolution::from_native(resolution));
    let result = match catch_unwind(AssertUnwindSafe(|| {
        let provider = factory
            .provider
            .as_ref()
            .expect("live video factory provider");
        if legacy {
            provider.query_support_legacy_value(&format, reference_scaling)
        } else {
            provider.query_support_value(&format, reference_scaling, resolution.as_ref())
        }
    })) {
        Ok(result) => result,
        Err(payload) => {
            factory.terminal.store(true, Ordering::Release);
            discard_panic_payload(payload);
            Err(VideoFactoryQueryError::ProviderPanicked)
        }
    };
    if let Err(error) = result {
        factory.failure.report(error);
    }
    crate::video_factory_values::ffi_support(result)
}
pub(crate) fn decoder_factory_create(
    factory: &RustVideoDecoderFactory,
    environment: cxx::UniquePtr<ffi::NativeEnvironment>,
    retention: &crate::video_factory_values::RustVideoFactoryEnvironment,
    format: cxx::UniquePtr<ffi::NativeSdpVideoFormat>,
) -> Box<RustVideoDecoder> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    if factory.terminal.load(Ordering::Acquire) {
        return Box::new(RustVideoDecoder {
            codec: None,
            terminal: AtomicBool::new(true),
        });
    }
    let context = crate::VideoCodecCreationContext {
        environment: crate::Environment::from_sdk(environment, Some(&retention.0)),
        format: crate::SdpVideoFormat::from_native(format),
    };
    match catch_unwind(AssertUnwindSafe(|| {
        factory
            .provider
            .as_ref()
            .expect("live video factory provider")
            .create_with_context(context)
    })) {
        Ok(Ok(codec)) => Box::new(RustVideoDecoder {
            codec: Some(codec),
            terminal: AtomicBool::new(false),
        }),
        Ok(Err(error)) => {
            if let CodecError::FormatProjection(error) = error {
                factory.failure.report(error);
            }
            Box::new(RustVideoDecoder {
                codec: None,
                terminal: AtomicBool::new(false),
            })
        }
        Err(payload) => {
            discard_panic_payload(payload);
            factory.terminal.store(true, Ordering::Release);
            Box::new(RustVideoDecoder {
                codec: None,
                terminal: AtomicBool::new(true),
            })
        }
    }
}
pub(crate) fn decoder_is_valid(decoder: &RustVideoDecoder) -> bool {
    decoder.codec.is_some() && !decoder.terminal.load(Ordering::Acquire)
}
fn with_decoder(
    decoder: &mut RustVideoDecoder,
    operation: impl FnOnce(&mut dyn VideoDecoder) -> Result<(), CodecError>,
) -> i32 {
    with_decoder_status(decoder, |codec| operation(codec).map(|()| CODEC_OK))
}
fn with_decoder_status(
    decoder: &mut RustVideoDecoder,
    operation: impl FnOnce(&mut dyn VideoDecoder) -> Result<i32, CodecError>,
) -> i32 {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    if decoder.terminal.load(Ordering::Acquire) {
        return CODEC_ERROR;
    }
    let Some(codec) = decoder.codec.as_deref_mut() else {
        return CODEC_UNINITIALIZED;
    };
    match catch_unwind(AssertUnwindSafe(|| operation(codec))) {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => status(Err(error)),
        Err(payload) => {
            discard_panic_payload(payload);
            decoder.terminal.store(true, Ordering::Release);
            CODEC_ERROR
        }
    }
}
pub(crate) fn decoder_configure_value(
    decoder: &mut RustVideoDecoder,
    settings: cxx::UniquePtr<ffi::NativeVideoDecoderSettings>,
) -> bool {
    let Some(settings) = crate::OwnedVideoDecoderSettings::from_native(settings) else {
        return false;
    };
    let mut configured = false;
    let status = with_decoder(decoder, |codec| {
        configured = codec.configure_value(settings)?;
        Ok(())
    });
    status == CODEC_OK && configured
}
pub(crate) fn decoder_implementation_name(decoder: &RustVideoDecoder, valid: &mut bool) -> Vec<u8> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    *valid = false;
    let Some(codec) = decoder
        .codec
        .as_deref()
        .filter(|_| !decoder.terminal.load(Ordering::Acquire))
    else {
        return Vec::new();
    };
    match catch_unwind(AssertUnwindSafe(|| codec.implementation_name())) {
        Ok(Ok(name)) => {
            *valid = true;
            name.into_bytes()
        }
        Ok(Err(_)) => Vec::new(),
        Err(payload) => {
            discard_panic_payload(payload);
            decoder.terminal.store(true, Ordering::Release);
            Vec::new()
        }
    }
}
pub(crate) fn decoder_get_info_value(
    decoder: &RustVideoDecoder,
) -> cxx::UniquePtr<ffi::NativeVideoDecoderInfo> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    let Some(codec) = decoder
        .codec
        .as_deref()
        .filter(|_| !decoder.terminal.load(Ordering::Acquire))
    else {
        return cxx::UniquePtr::null();
    };
    match catch_unwind(AssertUnwindSafe(|| codec.info_value())) {
        Ok(Ok(info)) => info.into_native(),
        Ok(Err(_)) => cxx::UniquePtr::null(),
        Err(payload) => {
            discard_panic_payload(payload);
            decoder.terminal.store(true, Ordering::Release);
            cxx::UniquePtr::null()
        }
    }
}
pub(crate) fn decoder_register_callback_value(
    decoder: &mut RustVideoDecoder,
    callback: cxx::SharedPtr<ffi::NativeDecodedImageCallback>,
) -> i32 {
    let callback = (!callback.is_null()).then_some(DecodedImageCallback { native: callback });
    with_decoder_status(decoder, |codec| codec.register_callback_value(callback))
}
pub(crate) fn decoder_decode_value(
    decoder: &mut RustVideoDecoder,
    frame: cxx::UniquePtr<ffi::NativeEncodedImage>,
    missing_overload: bool,
    missing: bool,
    render_time_ms: i64,
    callback: cxx::SharedPtr<ffi::NativeDecodedImageCallback>,
) -> i32 {
    if frame.is_null() {
        return CODEC_PARAMETER;
    }
    let frame = crate::EncodedImage::from_native(frame);
    with_decoder_status(decoder, |codec| {
        let callback = DecodedImageCallback { native: callback };
        if missing_overload {
            codec.decode_with_missing_frames_value(frame, missing, render_time_ms, callback)
        } else {
            codec.decode_value(frame, render_time_ms, callback)
        }
    })
}
pub(crate) fn decoder_register_callback(decoder: &mut RustVideoDecoder) -> i32 {
    with_decoder(decoder, |c| c.register_callback())
}
pub(crate) fn decoder_decode(
    decoder: &mut RustVideoDecoder,
    frame: cxx::UniquePtr<ffi::NativeEncodedVideoFrame>,
    callback: cxx::SharedPtr<ffi::NativeDecodedImageCallback>,
) -> i32 {
    let Some(native) = frame.as_ref() else {
        return CODEC_PARAMETER;
    };
    let frame = EncodedVideoFrame {
        data: ffi::native_encoded_frame_data(native),
        width: ffi::native_encoded_frame_width(native),
        height: ffi::native_encoded_frame_height(native),
        rtp_timestamp: ffi::native_encoded_frame_rtp_timestamp(native),
        frame_type: if ffi::native_encoded_frame_key(native) {
            VideoFrameType::Key
        } else {
            VideoFrameType::Delta
        },
        qp: u8::try_from(ffi::native_encoded_frame_qp(native)).ok(),
    };
    with_decoder(decoder, |c| {
        c.decode(frame, DecodedImageCallback { native: callback })
    })
}
pub(crate) fn decoder_release(decoder: &mut RustVideoDecoder) -> i32 {
    let result = with_decoder_status(decoder, |c| c.release_value());
    // Healthy SDK products remain available for Configure after Release.
    if decoder.terminal.load(Ordering::Acquire) && !contained_video_drop(decoder.codec.take()) {
        return CODEC_ERROR;
    }
    result
}
#[cfg(test)]
fn decoder_get_info(decoder: &RustVideoDecoder) -> ffi::FfiDecoderInfo {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    decoder
        .codec
        .as_deref()
        .filter(|_| !decoder.terminal.load(Ordering::Acquire))
        .and_then(|c| match catch_unwind(AssertUnwindSafe(|| c.info())) {
            Ok(info) => Some(info),
            Err(payload) => {
                discard_panic_payload(payload);
                decoder.terminal.store(true, Ordering::Release);
                None
            }
        })
        .map_or_else(
            || ffi::FfiDecoderInfo {
                implementation_name: "failed Rust decoder".into(),
                hardware_accelerated: false,
            },
            |i| ffi::FfiDecoderInfo {
                implementation_name: i.implementation_name,
                hardware_accelerated: i.hardware_accelerated,
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn native_rate_allocation_preserves_unset_and_explicit_zero_cells() {
        let mut r = ffi::FfiRateControl {
            bitrate_bps: 100_007,
            target_bitrate_bps: 123_456,
            framerate_fps: 30.0,
            bandwidth_bps: 200_000,
            bandwidth_limited: false,
            target_bandwidth_limited: true,
            layer_bitrates_bps: [[0; 4]; 5],
            has_layer_bitrate: [[false; 4]; 5],
            target_layer_bitrates_bps: [[0; 4]; 5],
            has_target_layer_bitrate: [[false; 4]; 5],
        };
        r.layer_bitrates_bps[0][0] = 100_000;
        r.has_layer_bitrate[0][0] = true;
        r.has_layer_bitrate[0][2] = true;
        r.layer_bitrates_bps[4][3] = 7;
        r.has_layer_bitrate[4][3] = true;
        r.layer_bitrates_bps[1][0] = 999;
        r.target_layer_bitrates_bps[0][0] = 123_456;
        r.has_target_layer_bitrate[0][0] = true;
        r.has_target_layer_bitrate[0][1] = true;
        let rates = VideoRateControl::from_ffi(r);
        assert_eq!(rates.bitrate_bps, 100_007);
        assert_eq!(rates.framerate_fps, 30.0);
        assert_eq!(rates.bandwidth_bps, 200_000);
        assert_eq!(rates.layer_bitrates_bps[0][0], Some(100_000));
        assert_eq!(rates.layer_bitrates_bps[0][1], None);
        assert_eq!(rates.layer_bitrates_bps[0][2], Some(0));
        assert_eq!(rates.layer_bitrates_bps[4][3], Some(7));
        assert_eq!(rates.layer_bitrates_bps[1][0], None);
        assert_eq!(rates.target_bitrate_bps, 123_456);
        assert_eq!(rates.target_layer_bitrates_bps[0][0], Some(123_456));
        assert_eq!(rates.target_layer_bitrates_bps[0][1], Some(0));
        assert_eq!(rates.target_layer_bitrates_bps[0][2], None);
        assert_eq!(rates.target_layer_bitrates_bps[4][3], None);
        assert!(!rates.bandwidth_limited);
        assert!(rates.target_bandwidth_limited);
    }

    struct PanicDropEncoder {
        drops: Arc<std::sync::atomic::AtomicUsize>,
        info_calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl VideoEncoder for PanicDropEncoder {
        fn initialize(&mut self, _: VideoEncoderSettings) -> Result<(), CodecError> {
            Ok(())
        }
        fn encode(
            &mut self,
            _: VideoFrame,
            _: &[VideoFrameType],
            _: EncodedImageCallback,
        ) -> Result<(), CodecError> {
            Err(CodecError::Released)
        }
        fn set_rates(&mut self, _: VideoRateControl) -> Result<(), CodecError> {
            Ok(())
        }
        fn release(&mut self) -> Result<(), CodecError> {
            Ok(())
        }
        fn info(&self) -> VideoEncoderInfo {
            self.info_calls.fetch_add(1, Ordering::SeqCst);
            panic!("encoder info");
        }
    }
    impl Drop for PanicDropEncoder {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            panic!("encoder destructor");
        }
    }
    #[test]
    fn native_release_retains_reusable_product_and_final_drop_is_contained() {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut encoder = RustVideoEncoder {
            codec: Some(Box::new(PanicDropEncoder {
                drops: drops.clone(),
                info_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })),
            terminal: AtomicBool::new(false),
        };
        assert_eq!(encoder_release(&mut encoder), CODEC_OK);
        assert_eq!(encoder_release(&mut encoder), CODEC_OK);
        assert!(encoder.codec.is_some());
        assert!(!encoder.terminal.load(Ordering::Acquire));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(encoder);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn poisoned_encoder_info_skips_provider_and_final_drop_is_contained() {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let encoder = RustVideoEncoder {
            codec: Some(Box::new(PanicDropEncoder {
                drops: drops.clone(),
                info_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })),
            terminal: AtomicBool::new(true),
        };
        assert_eq!(
            encoder_get_info(&encoder).implementation_name,
            "failed Rust encoder"
        );
        drop(encoder);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    struct PanicDropDecoder {
        drops: Arc<std::sync::atomic::AtomicUsize>,
        info_calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl VideoDecoder for PanicDropDecoder {
        fn configure(&mut self, _: VideoDecoderSettings) -> Result<(), CodecError> {
            Ok(())
        }
        fn decode(
            &mut self,
            _: EncodedVideoFrame,
            _: DecodedImageCallback,
        ) -> Result<(), CodecError> {
            Err(CodecError::Released)
        }
        fn release(&mut self) -> Result<(), CodecError> {
            Ok(())
        }
        fn info(&self) -> VideoDecoderInfo {
            self.info_calls.fetch_add(1, Ordering::SeqCst);
            panic!("decoder info");
        }
    }
    impl Drop for PanicDropDecoder {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            panic!("decoder destructor");
        }
    }
    #[test]
    fn decoder_release_and_poisoned_info_contain_destructor_failure() {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut decoder = RustVideoDecoder {
            codec: Some(Box::new(PanicDropDecoder {
                drops: drops.clone(),
                info_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            })),
            terminal: AtomicBool::new(true),
        };
        assert_eq!(
            decoder_get_info(&decoder).implementation_name,
            "failed Rust decoder"
        );
        // Poisoned release still retires the product, without invoking release
        // on its partially failed provider or letting its Drop escape CXX.
        assert_eq!(decoder_release(&mut decoder), CODEC_ERROR);
        assert!(decoder.codec.is_none());
        drop(decoder);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn info_panics_poison_each_product_before_any_later_provider_call() {
        let encoder_drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let encoder_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let encoder = RustVideoEncoder {
            codec: Some(Box::new(PanicDropEncoder {
                drops: encoder_drops.clone(),
                info_calls: encoder_calls.clone(),
            })),
            terminal: AtomicBool::new(false),
        };
        assert_eq!(
            encoder_get_info(&encoder).implementation_name,
            "failed Rust encoder"
        );
        assert!(encoder.terminal.load(Ordering::Acquire));
        assert_eq!(
            encoder_get_info(&encoder).implementation_name,
            "failed Rust encoder"
        );
        assert_eq!(encoder_calls.load(Ordering::SeqCst), 1);
        drop(encoder);
        assert_eq!(encoder_drops.load(Ordering::SeqCst), 1);
        let decoder_drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let decoder_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let decoder = RustVideoDecoder {
            codec: Some(Box::new(PanicDropDecoder {
                drops: decoder_drops.clone(),
                info_calls: decoder_calls.clone(),
            })),
            terminal: AtomicBool::new(false),
        };
        assert_eq!(
            decoder_get_info(&decoder).implementation_name,
            "failed Rust decoder"
        );
        assert!(decoder.terminal.load(Ordering::Acquire));
        assert_eq!(
            decoder_get_info(&decoder).implementation_name,
            "failed Rust decoder"
        );
        assert_eq!(decoder_calls.load(Ordering::SeqCst), 1);
        drop(decoder);
        assert_eq!(decoder_drops.load(Ordering::SeqCst), 1);
    }

    const MASK: u8 = 0xa5;
    fn h264() -> VideoCodecFormat {
        VideoCodecFormat::new("H264").with_parameter("profile-level-id", "42e01f")
    }

    struct Factory {
        fail_create: bool,
        panic_create: bool,
        panic_encode: bool,
        saved: Arc<Mutex<Option<EncodedImageCallback>>>,
    }
    impl VideoEncoderFactory for Factory {
        fn supported_formats(&self) -> Vec<VideoCodecFormat> {
            vec![h264()]
        }
        fn query_support(
            &self,
            f: &VideoCodecFormat,
            _: Option<&str>,
            _: Option<VideoResolution>,
        ) -> CodecSupport {
            CodecSupport {
                supported: f.name.eq_ignore_ascii_case("H264"),
                power_efficient: false,
            }
        }
        fn create(&self, f: &VideoCodecFormat) -> Result<Box<dyn VideoEncoder>, CodecError> {
            if self.panic_create {
                panic!("test provider panic")
            }
            if self.fail_create || !f.name.eq_ignore_ascii_case("H264") {
                Err(CodecError::UnsupportedFormat)
            } else {
                Ok(Box::new(TestEncoder {
                    panic_encode: self.panic_encode,
                    saved: self.saved.clone(),
                }))
            }
        }
    }
    struct TestEncoder {
        panic_encode: bool,
        saved: Arc<Mutex<Option<EncodedImageCallback>>>,
    }
    impl VideoEncoder for TestEncoder {
        fn initialize(&mut self, _: VideoEncoderSettings) -> Result<(), CodecError> {
            Ok(())
        }
        fn encode(
            &mut self,
            frame: VideoFrame,
            types: &[VideoFrameType],
            callback: EncodedImageCallback,
        ) -> Result<(), CodecError> {
            if self.panic_encode {
                panic!("test encode panic")
            }
            *self.saved.lock().unwrap() = Some(callback.clone());
            let mut data = frame.buffer.into_bytes();
            for b in &mut data {
                *b ^= MASK;
            }
            callback.emit(&EncodedVideoFrame {
                data,
                width: frame.width,
                height: frame.height,
                rtp_timestamp: frame.rtp_timestamp,
                frame_type: types.first().copied().unwrap_or(VideoFrameType::Delta),
                qp: Some(23),
            })
        }
        fn set_rates(&mut self, _: VideoRateControl) -> Result<(), CodecError> {
            Ok(())
        }
        fn release(&mut self) -> Result<(), CodecError> {
            Ok(())
        }
        fn info(&self) -> VideoEncoderInfo {
            VideoEncoderInfo {
                implementation_name: "test-only reversible H264-shaped encoder".into(),
                hardware_accelerated: false,
                supports_native_handle: false,
                supports_simulcast: false,
                fps_allocation: Some([vec![255], vec![], vec![], vec![], vec![]]),
            }
        }
    }
    struct DecoderFactory;
    impl VideoDecoderFactory for DecoderFactory {
        fn supported_formats(&self) -> Vec<VideoCodecFormat> {
            vec![h264()]
        }
        fn query_support(
            &self,
            f: &VideoCodecFormat,
            _: bool,
            _: Option<VideoResolution>,
        ) -> CodecSupport {
            CodecSupport {
                supported: f.name.eq_ignore_ascii_case("H264"),
                power_efficient: false,
            }
        }
        fn create(&self, f: &VideoCodecFormat) -> Result<Box<dyn VideoDecoder>, CodecError> {
            if f.name.eq_ignore_ascii_case("H264") {
                Ok(Box::new(TestDecoder))
            } else {
                Err(CodecError::UnsupportedFormat)
            }
        }
    }
    struct TestDecoder;
    impl VideoDecoder for TestDecoder {
        fn configure(&mut self, _: VideoDecoderSettings) -> Result<(), CodecError> {
            Ok(())
        }
        fn decode(
            &mut self,
            frame: EncodedVideoFrame,
            callback: DecodedImageCallback,
        ) -> Result<(), CodecError> {
            let mut data = frame.data;
            for b in &mut data {
                *b ^= MASK;
            }
            let decoded = VideoFrame::i420(
                frame.width,
                frame.height,
                data,
                i64::from(frame.rtp_timestamp) * 1000,
                frame.rtp_timestamp,
            )?;
            callback.emit(&decoded)
        }
        fn release(&mut self) -> Result<(), CodecError> {
            Ok(())
        }
        fn info(&self) -> VideoDecoderInfo {
            VideoDecoderInfo {
                implementation_name: "test-only reversible H264-shaped decoder".into(),
                hardware_accelerated: false,
            }
        }
    }

    #[test]
    fn native_encoder_configuration_preserves_mode_and_h264_count_independently() {
        let settings = |mode_present, count_present| ffi::FfiEncoderSettings {
            width: 16,
            height: 16,
            start_bitrate_bps: 64_000,
            max_bitrate_bps: 256_000,
            min_bitrate_bps: 16_000,
            max_framerate: 30,
            cores: 1,
            max_payload_size: 1200,
            has_scalability_mode: mode_present,
            scalability_mode: "L1T3".into(),
            has_h264_temporal_layers: count_present,
            h264_temporal_layers: 1,
            simulcast_temporal_layers: vec![3],
            simulcast_streams: vec![ffi::FfiSimulcastStream {
                width: 16,
                height: 16,
                active: false,
                temporal_layers: 3,
            }],
        };
        let observed = encoder_settings_from_ffi(settings(true, true));
        assert_eq!(
            observed
                .scalability_mode
                .as_ref()
                .map(VideoScalabilityMode::as_str),
            Some("L1T3")
        );
        assert_eq!(observed.h264_temporal_layers, Some(1));
        assert_eq!(observed.simulcast_temporal_layers, [3]);
        let missing = encoder_settings_from_ffi(settings(false, false));
        assert_eq!(missing.scalability_mode, None);
        assert_eq!(missing.h264_temporal_layers, None);
        assert_eq!(missing.simulcast_temporal_layers, [3]);
        assert_eq!(missing.width, observed.width);
        assert_eq!(missing.max_payload_size, 1200);
        assert_eq!(
            observed.simulcast_streams,
            [VideoSimulcastStream {
                width: 16,
                height: 16,
                active: false,
                temporal_layers: 3,
            }]
        );
        assert_eq!(missing.simulcast_streams, observed.simulcast_streams);
        let mut empty = settings(false, false);
        empty.simulcast_temporal_layers.clear();
        empty.simulcast_streams.clear();
        assert!(
            encoder_settings_from_ffi(empty)
                .simulcast_streams
                .is_empty()
        );
    }

    #[test]
    fn native_simulcast_configuration_preserves_order_and_inactive_geometry() {
        let settings = ffi::FfiEncoderSettings {
            simulcast_temporal_layers: vec![1, 2, 3],
            simulcast_streams: vec![
                ffi::FfiSimulcastStream {
                    width: 320,
                    height: 180,
                    active: true,
                    temporal_layers: 1,
                },
                ffi::FfiSimulcastStream {
                    width: 640,
                    height: 360,
                    active: false,
                    temporal_layers: 2,
                },
                ffi::FfiSimulcastStream {
                    width: 1280,
                    height: 720,
                    active: true,
                    temporal_layers: 3,
                },
            ],
            ..Default::default()
        };
        let observed = encoder_settings_from_ffi(settings);
        assert_eq!(
            observed.simulcast_streams,
            [
                VideoSimulcastStream {
                    width: 320,
                    height: 180,
                    active: true,
                    temporal_layers: 1
                },
                VideoSimulcastStream {
                    width: 640,
                    height: 360,
                    active: false,
                    temporal_layers: 2
                },
                VideoSimulcastStream {
                    width: 1280,
                    height: 720,
                    active: true,
                    temporal_layers: 3
                },
            ]
        );
    }

    #[test]
    fn native_scalability_modes_survive_factory_format_roundtrip() {
        struct DeclaredModes(VideoCodecFormat);
        impl VideoEncoderFactory for DeclaredModes {
            fn supported_formats(&self) -> Vec<VideoCodecFormat> {
                vec![self.0.clone()]
            }
            fn query_support(
                &self,
                format: &VideoCodecFormat,
                mode: Option<&str>,
                _: Option<VideoResolution>,
            ) -> CodecSupport {
                CodecSupport {
                    supported: format == &self.0
                        && mode.is_none_or(|name| {
                            self.0
                                .scalability_modes
                                .iter()
                                .any(|mode| mode.as_str() == name)
                        }),
                    power_efficient: false,
                }
            }
            fn create(&self, _: &VideoCodecFormat) -> Result<Box<dyn VideoEncoder>, CodecError> {
                Err(CodecError::ConstructionFailed)
            }
        }
        assert_eq!(
            VideoScalabilityMode::parse("invented-mode"),
            Err(CodecError::UnsupportedFormat)
        );
        let mode = VideoScalabilityMode::parse("L1T3").unwrap();
        assert_eq!(mode.as_str(), "L1T3");
        let format = h264().with_scalability_mode(mode);
        let dto = to_ffi_format(&format);
        assert_eq!(from_ffi_format_ref(&dto), format);
        assert_eq!(from_ffi_format(dto), format);
        let factory = VideoEncoderFactoryHandle::new(DeclaredModes(format.clone())).unwrap();
        assert_eq!(factory.supported_formats(), vec![format.clone()]);
        assert!(factory.query_support(&format, Some("L1T3"), None).supported);
        assert!(!factory.query_support(&format, Some("L1T2"), None).supported);
        // These are advertised capabilities only; no encoder was created and
        // this test does not qualify temporal output or native allocation.
    }

    #[test]
    fn h264_metadata_forwards_native_base_layer_sync() {
        for base_layer_sync in [false, true] {
            let converted = EncodedVideoMetadata {
                codec: EncodedVideoCodec::H264 { base_layer_sync },
                simulcast_index: Some(1),
                spatial_index: None,
                temporal_index: Some(2),
                end_of_picture: true,
            }
            .ffi();
            assert_eq!(converted.codec, 3);
            assert_eq!(converted.layer_sync, base_layer_sync);
            assert_eq!(converted.simulcast_index, 1);
            assert_eq!(converted.spatial_index, -1);
            assert_eq!(converted.temporal_index, 2);
            assert!(converted.end_of_picture);
        }
    }

    #[test]
    fn encoder_frame_rate_capabilities_preserve_native_shape_and_defaults() {
        let info = |fps_allocation| VideoEncoderInfo {
            implementation_name: "capability DTO".into(),
            hardware_accelerated: false,
            supports_native_handle: false,
            supports_simulcast: false,
            fps_allocation,
        };
        assert!(encoder_info_to_ffi(info(None)).fps_allocation.is_empty());
        let fractions = [
            vec![64, 128, 255],
            vec![128, 255],
            vec![],
            vec![255],
            vec![],
        ];
        let converted = encoder_info_to_ffi(info(Some(fractions.clone())));
        assert_eq!(converted.fps_allocation.len(), 5);
        for (native, original) in converted.fps_allocation.iter().zip(fractions) {
            assert_eq!(native.fractions, original);
        }
        // Explicitly unspecified fractions differ from retaining native defaults.
        let unspecified = encoder_info_to_ffi(info(Some(Default::default())));
        assert_eq!(unspecified.fps_allocation.len(), 5);
        assert!(
            unspecified
                .fps_allocation
                .iter()
                .all(|layer| layer.fractions.is_empty())
        );
    }

    #[test]
    fn real_virtual_factories_roundtrip_multiple_frames_and_release_callbacks() {
        let saved = Arc::new(Mutex::new(None));
        let encoder = VideoEncoderFactoryHandle::new(Factory {
            fail_create: false,
            panic_create: false,
            panic_encode: false,
            saved: saved.clone(),
        })
        .unwrap();
        let decoder = VideoDecoderFactoryHandle::new(DecoderFactory).unwrap();
        assert_eq!(encoder.supported_formats(), vec![h264()]);
        assert!(
            encoder
                .query_support(
                    &h264(),
                    None,
                    Some(VideoResolution {
                        width: 16,
                        height: 16
                    })
                )
                .supported
        );
        assert!(
            !encoder
                .query_support(&VideoCodecFormat::new("AV1"), None, None)
                .supported
        );
        assert!(AudioEncoderFactory::builtin().unwrap().is_available());
        assert!(AudioDecoderFactory::builtin().unwrap().is_available());
        let result = encoder.test_roundtrip(&decoder, 4);
        assert_eq!(
            (result.status, result.encoded_frames, result.decoded_frames),
            (0, 4, 4)
        );
        assert_ne!(result.checksum, 0);
        assert_eq!(
            saved
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .emit(&EncodedVideoFrame {
                    data: vec![1],
                    width: 1,
                    height: 1,
                    rtp_timestamp: 9,
                    frame_type: VideoFrameType::Delta,
                    qp: None
                }),
            Err(CodecError::Released)
        );
        assert!(encoder.test_cross_thread_lifecycle());
    }

    #[test]
    fn construction_failure_and_provider_panic_are_terminal_errors() {
        let saved = Arc::new(Mutex::new(None));
        let decoder = VideoDecoderFactoryHandle::new(DecoderFactory).unwrap();
        let failed = VideoEncoderFactoryHandle::new(Factory {
            fail_create: true,
            panic_create: false,
            panic_encode: false,
            saved: saved.clone(),
        })
        .unwrap();
        assert!(failed.test_roundtrip(&decoder, 1).status < 0);
        let panicked = VideoEncoderFactoryHandle::new(Factory {
            fail_create: false,
            panic_create: true,
            panic_encode: false,
            saved,
        })
        .unwrap();
        assert!(panicked.test_roundtrip(&decoder, 1).status < 0);
        assert!(panicked.try_supported_format_values().is_err());
        assert!(panicked.failure_observer().unwrap().failure().is_some());

        let panicked_during_encode = VideoEncoderFactoryHandle::new(Factory {
            fail_create: false,
            panic_create: false,
            panic_encode: true,
            saved: Arc::new(Mutex::new(None)),
        })
        .unwrap();
        assert!(panicked_during_encode.test_roundtrip(&decoder, 2).status < 0);
    }

    #[test]
    fn invalid_i420_buffers_are_rejected() {
        assert_eq!(
            VideoFrameBuffer::i420(4, 4, vec![0; 23]),
            Err(CodecError::InvalidFrame)
        );
    }

    #[test]
    fn sdk_factory_context_retains_identity_and_only_exact_selected_provenance() {
        let environment = crate::Environment::builder().build().unwrap();
        let other = AudioDecoderFactory::builtin().unwrap();
        for factory in [
            AudioDecoderFactory::builtin().unwrap(),
            AudioDecoderFactory::builtin_opus().unwrap(),
        ] {
            assert!(ffi::audio_decoder_factory_context_contract(
                factory.native(),
                other.native(),
                environment.native()
            ));
        }
    }

    #[test]
    fn factory_and_callback_handles_have_justified_thread_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VideoEncoderFactoryHandle>();
        assert_send_sync::<VideoDecoderFactoryHandle>();
        assert_send_sync::<AudioEncoderFactory>();
        assert_send_sync::<AudioDecoderFactory>();
        assert_send_sync::<EncodedImageCallback>();
        assert_send_sync::<DecodedImageCallback>();
    }
}
