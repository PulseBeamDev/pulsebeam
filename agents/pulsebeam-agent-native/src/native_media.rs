//! Owned application media values. Transport and buffering belong to libwebrtc.
use pulsebeam_webrtc_sys as rtc;

// Controlled hooks are process-exclusive and must not overlap any threaded
// libwebrtc graph, including a graph in a different Rust test worker.
#[cfg(test)]
pub(crate) static NATIVE_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, uniffi::Enum)]
pub enum AudioInput {
    #[default]
    Pcm,
    EncodedOpus {
        channels: u8,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, uniffi::Enum)]
pub enum VideoInput {
    #[default]
    I420,
    EncodedH264,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, uniffi::Enum)]
pub enum ReceiveMode {
    Encoded,
    #[default]
    Decoded,
    /// Video only: passive ordered encoded observations and native I420
    /// from the same receiver, without changing its decoder or interception.
    EncodedAndDecoded,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct VideoPublicationConfig {
    pub slot: String,
    pub source: VideoSourceConfig,
}

#[derive(Clone, Debug, Default, PartialEq, uniffi::Record)]
pub struct MediaConfig {
    pub audio_input: AudioInput,
    pub video_input: VideoInput,
    pub video_publications: Vec<VideoPublicationConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SourceIdentity {
    pub generation: u64,
    pub source: u64,
}

/// The native SDK report serialized on the actor, with no live engine handle.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct NativeStatistics {
    pub generation: u64,
    pub timestamp_us: i64,
    pub report_json: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct Encoding {
    pub rid: Option<String>,
    pub scale_down_by: f64,
    pub max_bitrate_bps: Option<u32>,
    pub max_framerate: Option<f64>,
    pub temporal_layers: u8,
}

impl Default for Encoding {
    fn default() -> Self {
        Self {
            rid: None,
            scale_down_by: 1.0,
            max_bitrate_bps: None,
            max_framerate: None,
            temporal_layers: 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct VideoSourceConfig {
    pub encodings: Vec<Encoding>,
    pub dependency_descriptor: bool,
}

impl Default for VideoSourceConfig {
    fn default() -> Self {
        Self {
            encodings: vec![Encoding::default()],
            dependency_descriptor: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct EncodingLimits {
    pub max_bitrate_bps: Option<u32>,
    pub max_framerate: Option<f64>,
}

/// Readback of the actual SDK encoding, preserving absent native fields.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct EncodingState {
    pub rid: Vec<u8>,
    pub active: bool,
    pub max_bitrate_bps: Option<i32>,
    pub max_framerate: Option<f64>,
    pub scale_down_by: Option<f64>,
    pub temporal_layers: Option<i32>,
    pub scalability_mode: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct VideoRateControl {
    pub bitrate_bps: u32,
    pub target_bitrate_bps: u32,
    pub framerate_fps: f64,
    pub bandwidth_bps: u64,
    pub bandwidth_limited: bool,
    pub target_bandwidth_limited: bool,
    pub layer_bitrates_bps: Vec<Vec<Option<u32>>>,
    pub target_layer_bitrates_bps: Vec<Vec<Option<u32>>>,
}

/// Owned SDK encoder failure classification. Keep actionable kinds separate
/// from display text so callers never recover by matching an error string.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum VideoEncoderError {
    UnsupportedFormat,
    ConstructionFailed,
    InvalidConfiguration,
    InvalidFrame,
    CallbackUnavailable,
    Released,
    ProviderPanicked,
    FormatProjection { detail: String },
}

impl From<rtc::CodecError> for VideoEncoderError {
    fn from(error: rtc::CodecError) -> Self {
        match error {
            rtc::CodecError::UnsupportedFormat => Self::UnsupportedFormat,
            rtc::CodecError::ConstructionFailed => Self::ConstructionFailed,
            rtc::CodecError::InvalidConfiguration => Self::InvalidConfiguration,
            rtc::CodecError::InvalidFrame => Self::InvalidFrame,
            rtc::CodecError::CallbackUnavailable => Self::CallbackUnavailable,
            rtc::CodecError::Released => Self::Released,
            rtc::CodecError::ProviderPanicked => Self::ProviderPanicked,
            rtc::CodecError::FormatProjection(detail) => Self::FormatProjection {
                detail: format!("{detail:?}"),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ProducerFeedback {
    pub keyframe_requested: bool,
    pub encoding_keyframes_requested: Vec<bool>,
    pub rates: Option<VideoRateControl>,
    pub encoder_error: Option<VideoEncoderError>,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct PcmFrame {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
    pub timestamp_us: i64,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct I420Frame {
    pub width: u32,
    pub height: u32,
    /// Native monotonic capture microseconds. Anchor relative producer offsets
    /// with `LocalSource::capture_time_us`, not an unrelated epoch or PTS zero.
    pub timestamp_us: i64,
    /// Tightly packed Y, U and V planes, with rounded-up chroma dimensions.
    pub data: Vec<u8>,
    pub rotation_degrees: u16,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct OpusFrame {
    pub timestamp: u32,
    pub samples_per_channel: u32,
    pub payload: Vec<u8>,
    /// Producer RFC 6464 level, 0..=127 dBov below full scale.
    pub audio_level: Option<u8>,
    pub voice_activity: bool,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct H264Frame {
    /// Native monotonic capture microseconds. Anchor relative producer offsets
    /// with `LocalSource::capture_time_us`; strictly increase within this source.
    pub timestamp_us: i64,
    pub width: u16,
    pub height: u16,
    pub keyframe: bool,
    pub temporal_index: Option<u8>,
    pub base_layer_sync: bool,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum InputFrame {
    Pcm {
        frame: PcmFrame,
    },
    I420 {
        frame: I420Frame,
    },
    Opus {
        frame: OpusFrame,
    },
    H264 {
        frame: H264Frame,
    },
    /// Genuine encoding-specific access units, ordered as the declared RIDs.
    H264Simulcast {
        frames: Vec<H264Frame>,
    },
}

#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum OutputFrame {
    Pcm {
        sample_rate_hz: u32,
        channels: u8,
        samples: Vec<i16>,
    },
    I420 {
        frame: I420Frame,
    },
    Opus {
        timestamp: u32,
        payload: Vec<u8>,
    },
    H264 {
        timestamp: u32,
        keyframe: bool,
        payload: Vec<u8>,
    },
}

/// Native receiver identity, separate from the control-plane catalog mapping.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RemoteTrack {
    pub generation: u64,
    pub receiver: u64,
    pub track_id: String,
    pub mid: String,
    pub media_index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum DecodeTargetIndication {
    NotPresent,
    Discardable,
    Switch,
    Required,
}

/// Only metadata actually supplied by the native frame callback is present.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct FrameMetadata {
    pub rtp_timestamp: Option<u32>,
    /// Present only when supplied by the native encoded-audio callback.
    pub sequence_number: Option<u16>,
    pub ssrc: Option<u32>,
    pub codec_mime_type: Option<String>,
    pub audio_level_dbov: Option<u8>,
    pub voice_activity: Option<bool>,
    pub samples_per_channel: Option<u32>,
    pub payload_type: Option<u8>,
    pub rid: Option<String>,
    pub capture_time_ms: Option<i64>,
    pub capture_time_us: Option<i64>,
    pub receive_time_us: Option<i64>,
    pub frame_id: Option<i64>,
    pub spatial_index: Option<i32>,
    pub temporal_index: Option<i32>,
    pub dependencies: Vec<i64>,
    pub decode_target_indications: Vec<DecodeTargetIndication>,
}

/// Actual decoded callback facts, not polling delivery or UI presentation.
/// Track identity is captured from the native receiver before sink attachment.
#[derive(Debug)]
pub struct DecodedVideoObservation {
    pub track: RemoteTrack,
    pub observed_at: std::time::Instant,
    /// SDK global monotonic callback clock, controlled by an exclusive world.
    pub native_observed_at_us: i64,
    pub frame: Result<RemoteFrame, String>,
}

/// Optional Rust-only synchronous observation. Do not block or synchronously
/// control the SDK. Callback panics fail the native generation.
#[derive(Clone)]
pub struct DecodedVideoObserver(
    std::sync::Arc<dyn Fn(DecodedVideoObservation) + Send + Sync + 'static>,
);
impl std::fmt::Debug for DecodedVideoObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DecodedVideoObserver(..)")
    }
}
impl DecodedVideoObserver {
    pub fn new(observer: impl Fn(DecodedVideoObservation) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(observer))
    }
    pub(crate) fn observe(&self, observation: DecodedVideoObservation) {
        (self.0)(observation);
    }
}

/// Actual native ordered encoded output, independent of polling retention.
/// Dropped means unavailable native identity/metadata, not polling queue loss.
#[derive(Debug)]
pub enum EncodedVideoObservation {
    Frame { frame: Box<RemoteFrame> },
    Dropped { track: RemoteTrack, skipped: u64 },
}

/// Optional Rust-only synchronous observation of combined video reception.
/// Do not block or synchronously control the SDK. Panics fail the generation.
#[derive(Clone)]
pub struct EncodedVideoObserver(
    std::sync::Arc<dyn Fn(EncodedVideoObservation) + Send + Sync + 'static>,
);
impl std::fmt::Debug for EncodedVideoObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("EncodedVideoObserver(..)")
    }
}
impl EncodedVideoObserver {
    pub fn new(observer: impl Fn(EncodedVideoObservation) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(observer))
    }
    pub(crate) fn observe(&self, observation: EncodedVideoObservation) {
        (self.0)(observation);
    }
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RemoteFrame {
    pub track: RemoteTrack,
    pub metadata: FrameMetadata,
    pub frame: OutputFrame,
}

#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
#[allow(
    clippy::large_enum_variant,
    reason = "UniFFI frame outcomes carry owned media records by value"
)]
pub enum MediaStreamUpdate {
    Track {
        track: RemoteTrack,
    },
    Frame {
        frame: RemoteFrame,
    },
    /// Native sink queue drops, distinct from lag in this observer's queue.
    NativeDropped {
        skipped: u64,
    },
    Lagged {
        skipped: u64,
    },
    Closed,
    /// Decoded-video observer retention loss in combined mode. This is not an
    /// encoded queue drop, decoder loss, or lag in the application observer.
    NativeDecodedDropped {
        skipped: u64,
    },
}

pub(crate) fn h264_format() -> rtc::VideoCodecFormat {
    rtc::VideoCodecFormat::new("H264")
        .with_parameter("level-asymmetry-allowed", "1")
        .with_parameter("packetization-mode", "1")
        .with_parameter("profile-level-id", "42e01f")
}

pub(crate) fn session<R: rtc::SessionRuntime>(
    runtime: R,
    audio: AudioInput,
    video: VideoInput,
) -> Result<rtc::Session<R>, rtc::PeerError> {
    let mut builder = rtc::Session::with_runtime(
        runtime,
        rtc::SessionConfig {
            video_format: (video == VideoInput::EncodedH264).then(h264_format),
        },
    );
    if matches!(audio, AudioInput::EncodedOpus { .. }) {
        builder = builder.encoded_audio();
    }
    if video == VideoInput::EncodedH264 {
        builder = builder.h264_profiles([64, 128, 255]);
    }
    // Encoded input's default interception factory is not a decoded receiver.
    // Both runtimes deliberately use the stock H.264 decoder here.
    let decoder = rtc::VideoDecoderFactoryHandle::builtin().map_err(|error| rtc::PeerError {
        kind: rtc::PeerErrorKind::NativeConstruction,
        message: error.to_string(),
    })?;
    #[cfg(feature = "sim")]
    if tracing::enabled!(target: crate::native_packet_diagnostics::TARGET, tracing::Level::TRACE)
        || tracing::enabled!(target: crate::native_packet_diagnostics::CC_TARGET, tracing::Level::TRACE)
        || tracing::enabled!(target: crate::native_packet_diagnostics::TRANSPORT_TARGET, tracing::Level::TRACE)
    {
        builder = builder.event_log_factory(crate::native_packet_diagnostics::Factory::new());
    }
    #[cfg(feature = "transport-conformance")]
    {
        builder = builder.network_manager_factory(&rtc::NetworkManagerFactory::from_provider(
            crate::qualification_network::LoopbackInterfaces,
        ));
    }
    builder.video_decoder_factory(&decoder).build()
}
