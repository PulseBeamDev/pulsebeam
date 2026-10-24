//! Runtime-injected ownership. Local handles never leave this module's
//! resource graph; IDs and owned values are the only application boundary.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

#[cfg(feature = "native")]
mod capture;
mod retirement;
mod runtime;
use retirement::SessionRetirement;
pub use runtime::{ManagedRuntime, SessionRuntime, ThreadedRuntime};

/// Compatibility names for the native threaded instantiation of the shared API.
pub type ProductionSession = Session<ThreadedRuntime>;
pub type ProductionSessionBuilder = SessionBuilder<ThreadedRuntime>;
pub type SessionConfig = ProductionSessionConfig;

use crate::{
    AudioDecoderFactory, AudioEncoderFactory, CodecError, DataChannel, DataChannelConfiguration,
    DataChannelError, DataChannelEvent, DataChannelMessage, DataChannelSendResult,
    EncodedAudioFrame, EncodedAudioSink, EncodedAudioSource, EncodedReceivedVideoFrame,
    EncodedVideoAccessUnit, EncodedVideoInput, EncodedVideoSink, EncodedVideoSource, IceCandidate,
    IceCandidateDisposition, OfferAnswerOptions, OperationCompletion, OperationId, PeerConnection,
    PeerConnectionEvent, PeerConnectionFactory, PeerDescriptions, PeerError, PeerErrorKind,
    PeerStatsSnapshot, RtcConfiguration, RtpHeaderExtensionCapability, RtpReceiver, RtpSender,
    RtpSenderParameters, RtpTransceiver, RtpTransceiverDirection, SessionDescription,
    VideoCodecFormat, VideoRateControl, readiness::Readiness,
};

#[path = "production_cleanup.rs"]
mod cleanup;
#[path = "production_connectivity.rs"]
mod connectivity;
#[path = "production_media.rs"]
mod media;
pub use connectivity::{SessionDtmfError, SessionSctpTransportInformation, SessionTransportError};

#[path = "production_codec.rs"]
mod codec;

macro_rules! resource_id {
    ($($name:ident),+ $(,)?) => {$ (
        /// An opaque session-owned identity. Copying it does not share access
        /// to an engine resource. IDs are never recycled across sessions.
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub struct $name(u64);
        impl $name { pub fn get(self) -> u64 { self.0 } }
    )+ };
}
resource_id!(
    SessionVideoEncoderId,
    SessionVideoDecoderId,
    SessionEncodedImageId,
    SessionEncodedVideoBufferId,
    SessionWritableEncodedBufferId,
    SessionEncodedImageCallbackId,
    SessionDecodedImageCallbackId,
    SessionFecOverrideId,
    SessionVideoEncoderSelectorId,
    SessionSenderParametersOperationId,
    SessionPeerId,
    SessionChannelId,
    SessionSourceId,
    SessionLocalMediaStreamId,
    SessionTrackId,
    SessionMediaStreamObserverId,
    SessionVideoSourceViewId,
    SessionVideoSinkId,
    SessionRawVideoFrameId,
    SessionRawVideoBufferId,
    SessionWritableI420BufferId,
    SessionVideoBufferPreparationId,
    SessionVideoSourceEncodedSinkId,
    SessionVideoObserverId,
    SessionAudioSourceViewId,
    SessionAudioTrackProcessorId,
    SessionAudioObserverId,
    SessionDesktopOptionsId,
    SessionDesktopMetadataId,
    SessionCameraOptionsId,
    SessionCameraDeviceInfoId,
    SessionTransceiverId,
    SessionSenderId,
    SessionReceiverId,
    SessionFrameTransformerId,
    SessionFrameEncryptorId,
    SessionFrameDecryptorId,
    SessionSframeEncrypterId,
    SessionSframeDecrypterId,
    SessionDtmfSenderId,
    SessionDtlsTransportId,
    SessionSctpTransportId,
    SessionIceTransportId,
    SessionMediaTrackId,
    SessionMediaStreamId,
    SessionTrackSourceId,
    SessionAudioProcessorId
);

static NEXT_RESOURCE: AtomicU64 = AtomicU64::new(1);
const MAX_OUTSTANDING_OPERATIONS: usize = 64;

fn next_id() -> Result<u64, PeerError> {
    NEXT_RESOURCE
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            next.checked_add(1)
        })
        .map_err(|_| {
            error(
                PeerErrorKind::ResourceExhausted,
                "session resource identities exhausted",
            )
        })
}
fn error(kind: PeerErrorKind, message: &str) -> PeerError {
    PeerError {
        kind,
        message: message.into(),
    }
}
fn missing() -> PeerError {
    error(
        PeerErrorKind::InvalidParameter,
        "resource does not belong to this session",
    )
}
fn codec_error(value: CodecError) -> PeerError {
    PeerError {
        kind: PeerErrorKind::InvalidParameter,
        message: value.to_string(),
    }
}

/// Media configuration shared by all session runtimes. Headless defaults use
/// stock software codecs with raw PCM/video; encoded input and physical devices
/// are explicit selections, independent of the chosen execution services.
#[derive(Clone, Debug, Default)]
pub struct ProductionSessionConfig {
    /// Optional direct encoded-video format. H.264 and VP8 are accepted.
    /// Reception is encoded-only; this does not advertise decoded H.264 output.
    pub video_format: Option<VideoCodecFormat>,
}

#[derive(Clone)]
enum StreamTrack {
    Audio(crate::AudioTrack),
    Video(crate::VideoTrack),
}

struct Peer {
    value: PeerConnection,
    outstanding: HashSet<OperationId>,
}
struct Resource<T> {
    peer: SessionPeerId,
    value: T,
}
struct Transceiver {
    value: RtpTransceiver,
    sender: SessionSenderId,
    receiver: SessionReceiverId,
}
enum Source {
    Pcm(crate::AudioSource),
    RawVideo(crate::VideoSource),
    Opus(EncodedAudioSource),
    Video(EncodedVideoSource),
    #[cfg(feature = "native")]
    Camera(crate::CameraCapture),
    #[cfg(feature = "native")]
    Desktop(crate::ScreenCapture),
}
impl Source {
    fn audio_source(&self) -> Result<&crate::AudioSource, PeerError> {
        match self {
            Self::Pcm(source) => Ok(source),
            Self::Opus(source) => Ok(source.source()),
            _ => Err(missing()),
        }
    }
    fn close(&mut self) -> Result<(), PeerError> {
        match self {
            Self::Pcm(source) => source
                .close()
                .map_err(|e| error(PeerErrorKind::Internal, &e.to_string())),
            Self::RawVideo(source) => source.close().map_err(codec_error),
            Self::Opus(source) => source
                .close()
                .map_err(|e| error(PeerErrorKind::Internal, &e.to_string())),
            Self::Video(source) => source.close().map_err(codec_error),
            #[cfg(feature = "native")]
            Self::Camera(capture) => capture.stop(),
            #[cfg(feature = "native")]
            Self::Desktop(capture) => capture.stop(),
        }
    }
    fn video_track(
        &self,
        factory: &PeerConnectionFactory,
        id: &str,
    ) -> Result<crate::VideoTrack, PeerError> {
        match self {
            Self::RawVideo(source) => factory.create_video_track(id, source),
            Self::Video(source) => source.create_track(factory, id),
            #[cfg(feature = "native")]
            Self::Camera(capture) => factory.create_video_track(id, capture.source()),
            #[cfg(feature = "native")]
            Self::Desktop(capture) => factory.create_video_track(id, capture.source()),
            _ => Err(missing()),
        }
    }
}

// One explicit observation mode per receiver. Audio remains exclusive; video
// may deliberately combine a passive ordered tap with normal native decoding.
enum MediaSink {
    Pcm(crate::AudioSink),
    RawVideo(crate::VideoSink),
    EncodedAudio(EncodedAudioSink),
    EncodedVideo(EncodedVideoSink),
    RawAndEncodedVideo {
        raw: crate::VideoSink,
        encoded: EncodedVideoSink,
    },
}
impl MediaSink {
    fn close(&mut self) -> Result<(), PeerError> {
        match self {
            Self::Pcm(sink) => sink.close(),
            Self::RawVideo(sink) => sink.close(),
            Self::EncodedAudio(sink) => sink.close(),
            Self::EncodedVideo(sink) => {
                sink.close();
                Ok(())
            }
            Self::RawAndEncodedVideo { raw, encoded } => {
                // Preserve raw sink close admission before changing either
                // observation. The encoded handle's close is infallible.
                raw.close()?;
                encoded.close();
                Ok(())
            }
        }
    }
    fn dropped_frames(&self) -> u64 {
        match self {
            Self::Pcm(sink) => sink.dropped_frames(),
            Self::RawVideo(sink) => sink.dropped_frames(),
            Self::EncodedAudio(sink) => sink.dropped_frames(),
            Self::EncodedVideo(sink) => sink.dropped_frames(),
            Self::RawAndEncodedVideo { raw, encoded } => raw
                .dropped_frames()
                .saturating_add(encoded.dropped_frames()),
        }
    }
}

#[derive(Clone)]
enum OwnedVideoSink {
    Track(crate::VideoSink),
    Source(crate::VideoSourceSink),
}
impl OwnedVideoSink {
    fn wants(&self) -> Option<crate::VideoSinkWants> {
        match self {
            Self::Track(s) => s.wants(),
            Self::Source(s) => s.wants(),
        }
    }
    fn update_wants(&self, wants: &crate::VideoSinkWants) -> Result<(), PeerError> {
        match self {
            Self::Track(s) => s.update_wants(wants),
            Self::Source(s) => s.update_wants(wants),
        }
    }
    fn frame(&self) -> Option<crate::VideoFrame> {
        match self {
            Self::Track(s) => s.try_next_frame(),
            Self::Source(s) => s.try_next_frame(),
        }
    }
    fn native_frame(&self) -> Option<crate::RawVideoFrame> {
        match self {
            Self::Track(s) => s.try_next_native_frame(),
            Self::Source(s) => s.try_next_native_frame(),
        }
    }
    fn event(&self) -> Option<crate::VideoSinkEvent> {
        match self {
            Self::Track(s) => s.try_next_event(),
            Self::Source(s) => s.try_next_event(),
        }
    }
    fn dropped_frames(&self) -> u64 {
        match self {
            Self::Track(s) => s.dropped_frames(),
            Self::Source(s) => s.dropped_frames(),
        }
    }
    fn dropped_events(&self) -> u64 {
        match self {
            Self::Track(s) => s.dropped_events(),
            Self::Source(s) => s.dropped_events(),
        }
    }
    fn close(&mut self) -> Result<(), PeerError> {
        match self {
            Self::Track(s) => s.close(),
            Self::Source(s) => s.close(),
        }
    }
}

/// Owned peer notifications. Resource arrivals contain IDs, not local handles.
#[derive(Debug)]
pub enum SessionEvent {
    RenegotiationNeeded,
    IceConnectionReceivingChanged(bool),
    InterestingUsage {
        usage_pattern: i32,
    },
    DataChannelMessage(crate::PeerDataChannelMessage),
    IceSelectedCandidatePairChanged(crate::IceCandidatePairChange),
    Stats(PeerStatsSnapshot),
    LegacyStats(crate::PeerLegacyStatsSnapshot),
    OperationComplete(OperationCompletion),
    OperationCompleteBytes {
        operation_id: OperationId,
        description: crate::SessionDescriptionBytes,
    },
    OperationFailedBytes {
        operation_id: OperationId,
        error: crate::PeerErrorBytes,
    },
    IceCandidate(IceCandidate),
    IceCandidateBytes(crate::IceCandidateBytes),
    IceCandidateRemoved(IceCandidate),
    IceCandidateRemovedBytes(crate::IceCandidateBytes),
    ConnectionStateChanged(crate::ConnectionState),
    SignalingStateChanged(crate::SignalingState),
    IceGatheringStateChanged(crate::IceGatheringState),
    IceConnectionStateChanged(crate::PeerIceConnectionState),
    StandardizedIceConnectionStateChanged(crate::PeerIceConnectionState),
    NegotiationNeeded {
        event_id: u32,
    },
    IceCandidateError {
        address: String,
        port: i32,
        url: String,
        error_code: i32,
        message: String,
    },
    IceCandidateErrorBytes {
        address: String,
        port: i32,
        url: String,
        error_code: i32,
        message: Vec<u8>,
    },
    DataChannel {
        channel: SessionChannelId,
        label: Vec<u8>,
    },
    Track {
        transceiver: SessionTransceiverId,
        receiver: SessionReceiverId,
    },
    TrackRemoved {
        receiver: SessionReceiverId,
    },
    StreamAdded {
        stream: SessionMediaStreamId,
    },
    StreamRemoved {
        stream: SessionMediaStreamId,
    },
    TrackAdded {
        receiver: SessionReceiverId,
        streams: Vec<SessionMediaStreamId>,
    },
    Closed,
}

/// A move-only, `Send`, non-`Sync` owner for a production engine resource graph.
/// Every engine operation requires exclusive actor access. Native sequencing
/// uses the engine's existing signaling/worker/network roles, not a second
/// application owner executor. A single-use production retirement worker is
/// reserved at construction only for final Drop from native execution contexts;
/// it never proxies application commands. Such Drop returns after handoff,
/// while native teardown waits for active SDK work to finish. Explicit shutdown
/// and Drop on ordinary caller threads remain synchronous. Controlled worlds
/// and legacy local handles stay non-Send and cannot enter this graph.
///
/// ```
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::ProductionSession>();
/// ```
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::ProductionSession>();
/// ```
pub struct Session<R: SessionRuntime = ThreadedRuntime> {
    // Observers/transports retire before endpoints, peers and role threads.
    connectivity: connectivity::ConnectivityResources,
    local_stream_observers: HashMap<SessionMediaStreamObserverId, crate::MediaStreamObserver>,
    video_encoders: HashMap<SessionVideoEncoderId, crate::OwnedVideoEncoder>,
    video_decoders: HashMap<SessionVideoDecoderId, crate::OwnedVideoDecoder>,
    encoded_images: HashMap<SessionEncodedImageId, crate::EncodedImage>,
    encoded_buffers: HashMap<SessionEncodedVideoBufferId, crate::EncodedVideoBuffer>,
    encoded_writers: HashMap<SessionWritableEncodedBufferId, crate::WritableEncodedVideoBuffer>,
    encoded_callbacks: HashMap<SessionEncodedImageCallbackId, crate::EncodedImageCallbackSink>,
    decoded_callbacks: HashMap<SessionDecodedImageCallbackId, crate::DecodedImageCallbackSink>,
    fec_overrides: HashMap<SessionFecOverrideId, crate::FecControllerOverride>,
    video_selectors: HashMap<SessionVideoEncoderSelectorId, crate::VideoEncoderSelector>,
    sender_parameter_operations:
        HashMap<SessionSenderParametersOperationId, crate::SenderParametersOperation>,
    video_buffer_preparations:
        HashMap<SessionVideoBufferPreparationId, crate::VideoBufferPreparation>,
    raw_video_frames: HashMap<SessionRawVideoFrameId, crate::RawVideoFrame>,
    raw_video_buffers: HashMap<SessionRawVideoBufferId, crate::RawVideoBuffer>,
    writable_i420_buffers: HashMap<SessionWritableI420BufferId, crate::WritableI420Buffer>,
    video_sinks: HashMap<SessionVideoSinkId, OwnedVideoSink>,
    video_source_encoded_sinks:
        HashMap<SessionVideoSourceEncodedSinkId, crate::VideoSourceEncodedSink>,
    video_observers: HashMap<SessionVideoObserverId, crate::VideoObserver>,
    video_source_views: HashMap<SessionVideoSourceViewId, crate::VideoSourceView>,
    streams: HashMap<SessionLocalMediaStreamId, crate::LocalMediaStream>,
    stream_tracks: HashMap<SessionTrackId, StreamTrack>,
    audio_track_processors: HashMap<SessionAudioTrackProcessorId, crate::AudioTrackProcessor>,
    audio_observers: HashMap<SessionAudioObserverId, crate::AudioObserver>,
    audio_source_views: HashMap<SessionAudioSourceViewId, crate::AudioSourceView>,
    sframe_encrypters: HashMap<SessionSframeEncrypterId, Resource<crate::SframeEncrypter>>,
    sframe_decrypters: HashMap<SessionSframeDecrypterId, Resource<crate::SframeDecrypter>>,
    frame_transformers: HashMap<SessionFrameTransformerId, crate::FrameTransformer>,
    frame_encryptors: HashMap<SessionFrameEncryptorId, crate::FrameEncryptor>,
    frame_decryptors: HashMap<SessionFrameDecryptorId, Resource<crate::FrameDecryptor>>,
    sender_observers: HashMap<SessionSenderId, Resource<crate::RtpSenderObserver>>,
    receiver_observers: HashMap<SessionReceiverId, Resource<crate::RtpReceiverObserver>>,
    channels: HashMap<SessionChannelId, Resource<DataChannel>>,
    media_sinks: HashMap<SessionReceiverId, MediaSink>,
    track_observers: HashMap<SessionMediaTrackId, Resource<crate::MediaObserver>>,
    stream_observers: HashMap<SessionMediaStreamId, Resource<crate::MediaObserver>>,
    media_streams: HashMap<SessionMediaStreamId, Resource<crate::MediaStream>>,
    track_sources: HashMap<SessionTrackSourceId, Resource<crate::MediaTrackSource>>,
    audio_processors: HashMap<SessionAudioProcessorId, Resource<crate::AudioProcessor>>,
    media_tracks: HashMap<SessionMediaTrackId, Resource<crate::MediaTrack>>,
    transceivers: HashMap<SessionTransceiverId, Resource<Transceiver>>,
    senders: HashMap<SessionSenderId, Resource<RtpSender>>,
    receivers: HashMap<SessionReceiverId, Resource<RtpReceiver>>,
    sources: HashMap<SessionSourceId, Source>,
    #[cfg(feature = "native")]
    desktop_options: HashMap<SessionDesktopOptionsId, crate::DesktopCaptureOptions>,
    #[cfg(feature = "native")]
    camera_device_info: HashMap<SessionCameraDeviceInfoId, crate::CameraDeviceInfo>,
    #[cfg(feature = "native")]
    camera_options: HashMap<SessionCameraOptionsId, crate::CameraCaptureOptions>,
    #[cfg(feature = "native")]
    desktop_metadata: HashMap<SessionDesktopMetadataId, crate::DesktopCaptureMetadata>,
    peers: HashMap<SessionPeerId, Peer>,
    input: Option<EncodedVideoInput>,
    // Release defaults after native peer dependents, before their role owners.
    video_bitrate_allocator_factory: Option<crate::VideoBitrateAllocatorFactory>,
    factory: cleanup::CaptiveFactory,
    readiness: Arc<Readiness>,
    closed: bool,
    _exclusive: PhantomData<Cell<()>>,
    // Threaded-only handoff. It has no application command queue and accepts
    // at most one final owner, with all SDK roles/dependencies still live.
    retirement: Option<SessionRetirement>,
    // Last: retain runtime dependencies through graph destruction and cleanup.
    runtime: Option<R>,
}

// SAFETY: the *entire* Rc graph is captive here. Construction accepts only owned
// Send configuration and creates production resources internally. No method
// returns or imports a handle, Rc, or reference to that graph. Field-trial
// providers require Send + Sync; returned views are independent immutable
// native copies, not handles into the captive graph. Rc/Cell/RefCell mutations and destruction occur only under exclusive
// session ownership, even after migration; native callbacks never touch them.
// Native objects marshal affine operations and final releases to retained live
// engine role threads (including the bypass-proxy SDP initiation overloads).
// Callback-triggered final Drop transfers this same exclusive graph, not any
// shared Rc reference, to its pre-admitted single-use retirement worker. The
// worker detaches only after ownership handoff and never borrows the SDK caller.
// Only the independently synchronized Readiness and codec broker states cross
// callback threads. Drop tears down all dependents before the role threads.
// If the final owner is released by an SDK-thread Waker, the entire graph moves
// once to the cleanup caller; no Rc alias remains in the emptied old owner.
// Owning CPU video frames keep independent metadata and atomically refcounted
// immutable SDK buffers. Mutable I420 storage has exclusive captive ownership
// until freeze; native preparation callbacks retain only synchronized C++ state,
// never the Rc graph. No opaque/custom texture producer is admitted here.
// This rationale does NOT apply to any individual legacy/local handle.
unsafe impl Send for Session<ThreadedRuntime> {}

/// Owned construction options for the common session. Provider registrations
/// use owned contracts; runtime dependencies retain their appropriate affinity.
/// A threaded instantiation never imports an affine driver or local handle.
pub struct SessionBuilder<R: SessionRuntime = ThreadedRuntime> {
    runtime: R,
    execution_resources: Option<crate::ExecutionResources>,
    config: ProductionSessionConfig,
    audio_encoder_factory: Option<AudioEncoderFactory>,
    audio_decoder_factory: Option<AudioDecoderFactory>,
    pcm_audio: bool,
    audio_processing: Option<crate::AudioProcessingConfig>,
    audio_processor_builder: Option<crate::BuiltinAudioProcessorBuilder>,
    audio_mixer_sources: Option<Vec<crate::AudioMixerSource>>,
    audio_mixer_provider: Option<Box<dyn crate::AudioMixerProvider>>,
    native_audio: bool,
    native_audio_creation: Option<crate::ffi::FfiNativeAudioDeviceCreation>,
    video_encoder_factory: Option<crate::VideoEncoderFactoryHandle>,
    video_decoder_factory: Option<crate::VideoDecoderFactoryHandle>,
    temporal_fps: Option<[u8; 3]>,
    simulcast: bool,
    field_trials: Option<Arc<dyn crate::FieldTrialsView>>,
    event_logging: bool,
    event_log_factory: Option<Arc<dyn crate::RtcEventLogFactory>>,
    encode_metronome: Option<crate::Metronome>,
    decode_metronome: Option<crate::Metronome>,
    network_monitor_factory: Option<Arc<dyn crate::NetworkMonitorFactory>>,
    network_manager_factory: Option<crate::NetworkManagerFactory>,
    socket_factory: Option<crate::PacketSocketFactory>,
    network_predictor_factory: Option<crate::NetworkStatePredictorFactory>,
    fec_factory: Option<crate::FecControllerFactory>,
    video_bitrate_allocator_factory: Option<crate::VideoBitrateAllocatorFactory>,
    network_controller_factory: Option<crate::NetworkControllerFactory>,
    environment_event_log: Option<Box<dyn crate::RtcEventLogger>>,
    clock: Option<crate::Clock>,
    task_queue_factory: Option<crate::TaskQueueFactory>,
    randomness: Option<crate::RandomnessLease>,
    environment_factory: Option<crate::EnvironmentFactory>,
}

impl<R: SessionRuntime> SessionBuilder<R> {
    /// Explicit native Call resources, independent of media selection.
    /// Without registration, the selected runtime supplies its factory default:
    /// one core for caller-driven execution, native host detection for threaded.
    /// An explicit unset budget still requests native host detection.
    pub fn execution_resources(mut self, resources: crate::ExecutionResources) -> Self {
        self.execution_resources = Some(resources);
        self
    }

    /// Select stock PCM encoding, which is also the headless default.
    /// An explicitly selected encoder factory is unchanged.
    pub fn pcm_audio(mut self) -> Self {
        self.pcm_audio = true;
        self
    }

    /// Explicitly select the encoded Opus adapter instead of raw PCM input.
    /// This selection is identical for threaded and caller-pumped execution.
    pub fn encoded_audio(mut self) -> Self {
        self.pcm_audio = false;
        self
    }

    /// Select the factory's software processing module. PCM input is required;
    /// already encoded packets cannot be processed as captured PCM.
    /// Construct and install a stock mixer retaining these exact source products
    /// inside the actor. No sequence-bound standalone mixer crosses the boundary.
    pub fn audio_mixer_sources(mut self, sources: Vec<crate::AudioMixerSource>) -> Self {
        self.audio_mixer_provider = None;
        self.audio_mixer_sources = Some(sources);
        self
    }
    /// Construct the exact caller mixer inside the actor, not a stock delegate.
    pub fn audio_mixer_provider(mut self, provider: impl crate::AudioMixerProvider) -> Self {
        self.audio_mixer_sources = None;
        self.audio_mixer_provider = Some(Box::new(provider));
        self
    }
    /// A Send-only owning recipe, built in the actor's exact environment. The
    /// resulting local APM never crosses the public actor boundary.
    pub fn audio_processor_builder(mut self, builder: crate::BuiltinAudioProcessorBuilder) -> Self {
        self.audio_processor_builder = Some(builder);
        self.audio_processing = None;
        self
    }
    pub fn audio_processing(mut self, config: crate::AudioProcessingConfig) -> Self {
        self.audio_processor_builder = None;
        self.audio_processing = Some(config);
        self
    }

    /// Explicitly select the native artifact's recording/playout device module.
    /// This can fail on hosts without a usable sound service. It does not opt
    /// into microphone capture; publish_microphone is a separate operation.
    #[cfg(feature = "native")]
    pub fn native_audio(mut self, enabled: bool) -> Self {
        self.native_audio = enabled;
        self
    }

    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn native_audio_device(mut self, config: crate::AudioDeviceCreationConfig) -> Self {
        self.native_audio = true;
        self.native_audio_creation = Some(config.ffi());
        self
    }
    /// Explicit selection, distinct from this builder's absent carrier default.
    pub fn audio_encoder_factory(mut self, factory: &AudioEncoderFactory) -> Self {
        self.audio_encoder_factory = Some(factory.clone());
        self
    }
    pub fn audio_decoder_factory(mut self, factory: &AudioDecoderFactory) -> Self {
        self.audio_decoder_factory = Some(factory.clone());
        self
    }
    pub fn video_encoder_factory(mut self, factory: &crate::VideoEncoderFactoryHandle) -> Self {
        self.video_encoder_factory = Some(factory.clone());
        self
    }
    pub fn video_decoder_factory(mut self, factory: &crate::VideoDecoderFactoryHandle) -> Self {
        self.video_decoder_factory = Some(factory.clone());
        self
    }
    pub fn video_encoder_provider(
        self,
        provider: impl crate::VideoEncoderFactoryProvider,
    ) -> Result<Self, CodecError> {
        Ok(self.video_encoder_factory(&crate::VideoEncoderFactoryHandle::from_provider(provider)?))
    }
    pub fn video_decoder_provider(
        self,
        provider: impl crate::VideoDecoderFactoryProvider,
    ) -> Result<Self, CodecError> {
        Ok(self.video_decoder_factory(&crate::VideoDecoderFactoryHandle::from_provider(provider)?))
    }
    /// Default for subsequently created peers. An explicit per-peer allocator
    /// overrides it; omission of both preserves native stock selection.
    pub fn video_bitrate_allocator_factory(
        mut self,
        factory: &crate::VideoBitrateAllocatorFactory,
    ) -> Self {
        self.video_bitrate_allocator_factory = Some(factory.clone());
        self
    }
    pub fn video_bitrate_allocator_provider(
        self,
        provider: impl crate::VideoBitrateAllocatorFactoryProvider,
    ) -> Self {
        self.video_bitrate_allocator_factory(&crate::VideoBitrateAllocatorFactory::from_provider(
            provider,
        ))
    }
    pub fn network_controller_factory(mut self, factory: &crate::NetworkControllerFactory) -> Self {
        self.network_controller_factory = Some(factory.clone());
        self
    }
    pub fn network_controller_provider(
        self,
        provider: impl crate::NetworkControllerFactoryProvider,
    ) -> Self {
        self.network_controller_factory(&crate::NetworkControllerFactory::from_provider(provider))
    }
    pub fn fec_controller_factory(mut self, factory: &crate::FecControllerFactory) -> Self {
        self.fec_factory = Some(factory.clone());
        self
    }
    pub fn fec_controller_provider(
        self,
        provider: impl crate::FecControllerFactoryProvider,
    ) -> Self {
        self.fec_controller_factory(&crate::FecControllerFactory::from_provider(provider))
    }
    /// The pinned SDK ignores the standalone predictor dependency slot, so
    /// build rejects it. Use network_controller_factory with an explicit
    /// NetworkControllerFactory::goog_cc configuration instead.
    pub fn network_state_predictor_factory(
        mut self,
        factory: &crate::NetworkStatePredictorFactory,
    ) -> Self {
        self.network_predictor_factory = Some(factory.clone());
        self
    }
    pub fn network_state_predictor_provider(
        self,
        provider: impl crate::NetworkStatePredictorFactoryProvider,
    ) -> Self {
        self.network_state_predictor_factory(&crate::NetworkStatePredictorFactory::from_provider(
            provider,
        ))
    }
    /// Register a worker-sequence encode metronome without importing a local
    /// native handle into the actor's captive resource graph.
    pub fn encode_metronome(mut self, metronome: &crate::Metronome) -> Self {
        self.encode_metronome = Some(metronome.clone());
        self
    }
    pub fn encode_metronome_provider(self, provider: impl crate::MetronomeProvider) -> Self {
        self.encode_metronome(&crate::Metronome::from_provider(provider))
    }
    pub fn decode_metronome(mut self, metronome: &crate::Metronome) -> Self {
        self.decode_metronome = Some(metronome.clone());
        self
    }
    pub fn decode_metronome_provider(self, provider: impl crate::MetronomeProvider) -> Self {
        self.decode_metronome(&crate::Metronome::from_provider(provider))
    }

    /// Select a thread-safe production scheduler. Controlled factories remain
    /// excluded from the actor resource graph.
    pub fn task_queue_factory(mut self, factory: &crate::TaskQueueFactory) -> Self {
        self.task_queue_factory = Some(factory.clone());
        self
    }

    pub fn task_queue_provider(
        self,
        provider: impl crate::TaskQueueFactoryProvider,
    ) -> Result<Self, crate::BuildEnvironmentError> {
        Ok(self.task_queue_factory(&crate::TaskQueueFactory::from_provider(provider)?))
    }

    /// Select an owned clock without replacing WebRTC's process time hooks.
    pub fn clock_handle(mut self, clock: &crate::Clock) -> Self {
        self.clock = Some(clock.clone());
        self
    }

    pub fn clock_provider(self, provider: impl crate::ClockProvider) -> Self {
        self.clock_handle(&crate::Clock::from_provider(provider))
    }

    /// Retain the selected process-global installation through native creation,
    /// actor migration and final dependent-resource release. This does not
    /// reseed randomness or install a controlled clock/scheduler.
    pub fn randomness(mut self, lease: &crate::RandomnessLease) -> Self {
        self.randomness = Some(lease.clone());
        self
    }

    /// Snapshot a reusable owning environment factory. No local peer/thread
    /// handles are imported. Individual environment selections override it.
    pub fn environment_factory(mut self, factory: &crate::EnvironmentFactory) -> Self {
        self.environment_factory = Some(factory.clone());
        self
    }

    /// Create sequence-bound manager products inside the native network role;
    /// no local Network or manager handles are imported into the actor.
    pub fn network_manager_factory(mut self, factory: &crate::NetworkManagerFactory) -> Self {
        self.network_manager_factory = Some(factory.clone());
        self
    }
    pub fn network_manager_provider(
        self,
        provider: impl crate::NetworkManagerFactoryProvider,
    ) -> Self {
        self.network_manager_factory(&crate::NetworkManagerFactory::from_provider(provider))
    }

    /// Create local products inside the SDK network role; no !Send socket,
    /// accepted product or context is imported into the actor.
    pub fn socket_factory(mut self, factory: &crate::PacketSocketFactory) -> Self {
        self.socket_factory = Some(factory.clone());
        self
    }
    pub fn socket_factory_provider(
        self,
        provider: impl crate::PacketSocketFactoryImplementation,
    ) -> Self {
        self.socket_factory(&crate::PacketSocketFactory::from_provider(provider))
    }

    /// Register an owned monitor factory with native network-thread callbacks.
    pub fn network_monitor_factory(mut self, factory: impl crate::NetworkMonitorFactory) -> Self {
        self.network_monitor_factory = Some(Arc::new(factory));
        self
    }

    pub fn environment_event_log(mut self, logger: impl crate::RtcEventLogger) -> Self {
        self.environment_event_log = Some(Box::new(logger));
        self
    }

    pub fn event_log_factory(mut self, factory: impl crate::RtcEventLogFactory) -> Self {
        self.event_log_factory = Some(Arc::new(factory));
        self
    }

    pub fn event_log_factory_handle(mut self, factory: &crate::RtcEventLogFactoryHandle) -> Self {
        self.event_log_factory = factory.registration();
        self.event_logging = self.event_log_factory.is_none();
        self
    }

    pub fn field_trials(mut self, view: impl crate::FieldTrialsView) -> Self {
        self.field_trials = Some(Arc::new(view));
        self
    }

    /// Make the bundled native event logger available to subsequently created
    /// peers. Does not start logging or create an output file.
    pub fn event_logging(mut self, enabled: bool) -> Self {
        self.event_logging = enabled;
        self
    }

    /// Direct single-encoding L1T3 convenience configuration.
    pub fn l1t3_input(mut self, fractions: [u8; 3]) -> Self {
        self.temporal_fps = Some(fractions);
        self.simulcast = false;
        self
    }

    /// Mixed source profiles with one declared L1T3 FPS allocation.
    pub fn h264_profiles(mut self, fractions: [u8; 3]) -> Self {
        self.temporal_fps = Some(fractions);
        self.simulcast = true;
        self
    }

    /// Direct three-rung simulcast convenience configuration.
    pub fn simulcast_input(mut self) -> Self {
        self.temporal_fps = None;
        self.simulcast = true;
        self
    }

    pub fn build(self) -> Result<Session<R>, PeerError> {
        if (self.temporal_fps.is_some() || self.simulcast) && self.config.video_format.is_none() {
            return Err(error(
                PeerErrorKind::UnsupportedParameter,
                "layered input requires a video format",
            ));
        }
        Session::build(self)
    }
}

impl<R: SessionRuntime> Session<R> {
    pub fn builder(config: SessionConfig) -> SessionBuilder<R>
    where
        R: Default,
    {
        Self::with_runtime(R::default(), config)
    }

    /// One construction path for either selected runtime. The outer harness
    /// supplies dependencies; all session operations use this same graph.
    pub fn with_runtime(runtime: R, config: SessionConfig) -> SessionBuilder<R> {
        SessionBuilder {
            runtime,
            config,
            randomness: None,
            environment_factory: None,
            audio_encoder_factory: None,
            audio_decoder_factory: None,
            pcm_audio: true,
            execution_resources: None,
            audio_processing: None,
            audio_processor_builder: None,
            audio_mixer_sources: None,
            audio_mixer_provider: None,
            native_audio: false,
            native_audio_creation: None,
            video_encoder_factory: None,
            video_decoder_factory: None,
            temporal_fps: None,
            simulcast: false,
            field_trials: None,
            event_logging: false,
            event_log_factory: None,
            encode_metronome: None,
            decode_metronome: None,
            network_monitor_factory: None,
            network_manager_factory: None,
            socket_factory: None,
            video_bitrate_allocator_factory: None,
            network_predictor_factory: None,
            fec_factory: None,
            network_controller_factory: None,
            environment_event_log: None,
            clock: None,
            task_queue_factory: None,
        }
    }

    pub fn new(config: ProductionSessionConfig) -> Result<Self, PeerError>
    where
        R: Default,
    {
        Self::builder(config).build()
    }

    /// Create the actor with direct, single-encoding H264 L1T3 input.
    /// Configuration must select the supported constrained-baseline H264 format.
    /// Native DD must be negotiated and L1T3 selected on the sender before input.
    /// `fps` passes cumulative native temporal frame-rate capabilities unchanged.
    pub fn new_l1t3(config: ProductionSessionConfig, fps: [u8; 3]) -> Result<Self, PeerError>
    where
        R: Default,
    {
        if config.video_format.is_none() {
            return Err(error(
                PeerErrorKind::UnsupportedParameter,
                "L1T3 requires an H264 video format",
            ));
        }
        Self::builder(config).l1t3_input(fps).build()
    }

    /// Create direct three-rung H264 input through native sender callbacks.
    /// Publication requires native three-stream initialization and three RIDs
    /// with matching rung geometry. Ambiguous single-encoder fallback rejects
    /// asynchronously rather than guessing an encoding. This does
    /// not extend stock libwebrtc's simultaneous simulcast receiving support.
    pub fn new_simulcast(config: ProductionSessionConfig) -> Result<Self, PeerError>
    where
        R: Default,
    {
        if config.video_format.is_none() {
            return Err(error(
                PeerErrorKind::UnsupportedParameter,
                "simulcast requires an H264 video format",
            ));
        }
        Self::builder(config).simulcast_input().build()
    }

    /// Install owned values or a caller-defined immutable provider before
    /// native factory construction. Provider callbacks may run on native threads.
    pub fn new_with_field_trials(
        config: ProductionSessionConfig,
        view: impl crate::FieldTrialsView,
    ) -> Result<Self, PeerError>
    where
        R: Default,
    {
        Self::builder(config).field_trials(view).build()
    }

    /// L1T3 constructor with an immutable field-trial provider.
    pub fn new_l1t3_with_field_trials(
        config: ProductionSessionConfig,
        fps: [u8; 3],
        view: impl crate::FieldTrialsView,
    ) -> Result<Self, PeerError>
    where
        R: Default,
    {
        if config.video_format.is_none() {
            return Err(error(
                PeerErrorKind::UnsupportedParameter,
                "L1T3 requires an H264 video format",
            ));
        }
        Self::builder(config)
            .l1t3_input(fps)
            .field_trials(view)
            .build()
    }

    /// Simulcast constructor with an immutable field-trial provider.
    pub fn new_simulcast_with_field_trials(
        config: ProductionSessionConfig,
        view: impl crate::FieldTrialsView,
    ) -> Result<Self, PeerError>
    where
        R: Default,
    {
        if config.video_format.is_none() {
            return Err(error(
                PeerErrorKind::UnsupportedParameter,
                "simulcast requires an H264 video format",
            ));
        }
        Self::builder(config)
            .simulcast_input()
            .field_trials(view)
            .build()
    }

    /// Full owning SDK capability metadata from this actor's selected factory.
    /// Safe after caller migration; no local engine handle escapes.
    pub fn get_rtp_sender_capabilities(
        &self,
        kind: crate::MediaType,
    ) -> Result<crate::RtpCapabilities, PeerError> {
        self.check_open()?;
        Ok(self.factory.get_rtp_sender_capabilities(kind))
    }

    /// Full owning SDK receiver capability metadata through the retained factory.
    pub fn get_rtp_receiver_capabilities(
        &self,
        kind: crate::MediaType,
    ) -> Result<crate::RtpCapabilities, PeerError> {
        self.check_open()?;
        Ok(self.factory.get_rtp_receiver_capabilities(kind))
    }

    /// Copy the actual factory environment's view, including after migration.
    pub fn field_trials(&self) -> Result<crate::FieldTrialsSnapshot, PeerError> {
        self.check_open()?;
        Ok(self.factory.field_trials())
    }

    /// Owned reading from the factory's effective native clock, including
    /// signed values. No local engine handle is returned to the actor caller.
    pub fn clock_timestamp(&self) -> Result<crate::Timestamp, PeerError> {
        self.check_open()?;
        Ok(self.factory.clock_timestamp())
    }

    fn build(options: SessionBuilder<R>) -> Result<Self, PeerError> {
        // Keep the runtime until error-path provider cleanup has finished.
        let SessionBuilder {
            mut runtime,
            execution_resources,
            randomness,
            environment_factory,
            config,
            audio_encoder_factory,
            audio_decoder_factory,
            pcm_audio,
            audio_processing,
            audio_processor_builder,
            audio_mixer_sources,
            audio_mixer_provider,
            native_audio,
            native_audio_creation,
            video_encoder_factory,
            video_decoder_factory,
            temporal_fps,
            simulcast,
            field_trials,
            event_logging,
            event_log_factory,
            encode_metronome,
            decode_metronome,
            network_monitor_factory,
            network_manager_factory,
            socket_factory,
            video_bitrate_allocator_factory,
            network_predictor_factory,
            fec_factory,
            network_controller_factory,
            environment_event_log,
            clock,
            task_queue_factory,
        } = options;
        if config
            .video_format
            .as_ref()
            .is_some_and(|f| !matches!(f.name.as_str(), "H264" | "VP8"))
        {
            return Err(error(
                PeerErrorKind::UnsupportedParameter,
                "production session supports encoded H264 or VP8",
            ));
        }
        if config.video_format.is_some() && video_encoder_factory.is_some() {
            return Err(error(
                PeerErrorKind::InvalidParameter,
                "the configured encoded input requires its own selected encoder factory; choose encoded input or an explicit encoder factory",
            ));
        }
        let readiness = Arc::new(Readiness::default());
        let selected_audio_encoder = match audio_encoder_factory {
            Some(factory) => factory,
            None if pcm_audio => AudioEncoderFactory::builtin().map_err(codec_error)?,
            None => AudioEncoderFactory::with_opus_frames().map_err(codec_error)?,
        };
        let selected_audio_decoder = match audio_decoder_factory {
            Some(factory) => factory,
            None if pcm_audio => AudioDecoderFactory::builtin().map_err(codec_error)?,
            None => AudioDecoderFactory::builtin_opus().map_err(codec_error)?,
        };
        let mut builder = runtime::configure(&mut runtime, PeerConnectionFactory::builder())
            .audio_encoder_factory(selected_audio_encoder)
            .audio_decoder_factory(selected_audio_decoder)
            .readiness(readiness.clone())
            .event_logging(event_logging);
        if let Some(resources) = execution_resources {
            builder = builder.execution_resources(resources);
        }
        if let Some(config) = audio_processing {
            builder = builder.audio_processing(config);
        }
        if let Some(processor) = audio_processor_builder {
            builder = builder.audio_processor_builder(processor);
        }
        if let Some(sources) = audio_mixer_sources {
            let mut mixer = crate::AudioMixer::stock()
                .map_err(|e| error(PeerErrorKind::NativeConstruction, &e.to_string()))?;
            for source in &sources {
                mixer
                    .add_source(source)
                    .map_err(|e| error(PeerErrorKind::NativeConstruction, &e.to_string()))?;
            }
            builder = builder.audio_mixer(mixer);
        }
        if let Some(provider) = audio_mixer_provider {
            builder = builder.audio_mixer(
                crate::AudioMixer::from_boxed_provider(provider)
                    .map_err(|e| error(PeerErrorKind::NativeConstruction, &e.to_string()))?,
            );
        }
        #[cfg(feature = "native")]
        {
            builder = builder.native_audio(native_audio);
        }
        if let Some(creation) = native_audio_creation {
            builder = builder.native_audio_creation_wire(creation);
        }
        #[cfg(not(feature = "native"))]
        let _ = native_audio;
        if let Some(metronome) = encode_metronome {
            builder = builder.encode_metronome(&metronome);
        }
        if let Some(metronome) = decode_metronome {
            builder = builder.decode_metronome(&metronome);
        }
        if let Some(factory) = event_log_factory {
            builder = builder.shared_event_log_factory(factory);
        }
        if let Some(factory) = network_monitor_factory {
            builder = builder.shared_network_monitor_factory(factory);
        }
        if let Some(factory) = network_manager_factory {
            builder = builder.network_manager_factory(&factory);
        }
        if let Some(factory) = socket_factory {
            builder = builder.socket_factory(&factory);
        }
        if let Some(factory) = network_predictor_factory {
            builder = builder.network_state_predictor_factory(&factory);
        }
        if let Some(factory) = fec_factory {
            builder = builder.fec_controller_factory(&factory);
        }
        if let Some(factory) = network_controller_factory {
            builder = builder.network_controller_factory(&factory);
        }
        if let Some(factory) = &task_queue_factory {
            runtime::validate_queues(&runtime, Some(factory.is_cooperative()))?;
        }
        if environment_factory.is_some()
            || field_trials.is_some()
            || environment_event_log.is_some()
            || clock.is_some()
            || task_queue_factory.is_some()
            || randomness.is_some()
        {
            let mut environment = match environment_factory {
                Some(factory) => factory,
                None => runtime::environment_factory(&runtime)?,
            };
            let configure = || -> Result<crate::Environment, crate::BuildEnvironmentError> {
                if let Some(factory) = task_queue_factory {
                    environment.set_task_queue_factory(&factory)?;
                }
                if let Some(clock) = clock {
                    environment.set_clock_handle(&clock)?;
                }
                if let Some(view) = field_trials {
                    environment.set_field_trials(view)?;
                }
                if let Some(logger) = environment_event_log {
                    environment.set_event_log(logger)?;
                }
                if let Some(lease) = randomness {
                    environment.set_randomness(&lease)?;
                }
                environment.create()
            };
            let environment = configure()
                .map_err(|e| error(PeerErrorKind::NativeConstruction, &e.to_string()))?;
            runtime::validate_queues(&runtime, environment.cooperative_queues())?;
            builder = builder.environment(environment);
        }
        let input = config
            .video_format
            .map(|format| {
                let input = match temporal_fps {
                    None if simulcast => {
                        EncodedVideoInput::new_simulcast_for_format(format.clone())
                    }
                    Some(fps) if simulcast => {
                        EncodedVideoInput::new_h264_profiles_for_format(format.clone(), fps)
                    }
                    Some(fps) => EncodedVideoInput::new_l1t3_for_format(format.clone(), fps),
                    None => EncodedVideoInput::new_for_format(format.clone()),
                }
                .map_err(codec_error)?;
                let decoder = input.encoded_receive_factory().map_err(codec_error)?;
                Ok::<_, PeerError>((input, decoder))
            })
            .transpose()?;
        let input = if let Some((input, decoder)) = input {
            builder = builder
                .video_encoder_factory(input.encoder_factory())
                .video_decoder_factory(decoder);
            Some(input)
        } else {
            None
        };
        // Retain the actual selected registrations, including explicit native
        // defaults, for standalone actor discovery/Create dispatch as well.
        if let Some(factory) = video_encoder_factory {
            builder = builder.video_encoder_factory(factory);
        } else if input.is_none() {
            builder = builder.video_encoder_factory(
                crate::VideoEncoderFactoryHandle::builtin().map_err(codec_error)?,
            );
        }
        if let Some(factory) = video_decoder_factory {
            builder = builder.video_decoder_factory(factory);
        } else if input.is_none() {
            builder = builder.video_decoder_factory(
                crate::VideoDecoderFactoryHandle::builtin().map_err(codec_error)?,
            );
        }
        let factory = builder.build()?;
        // Only the threaded runtime reserves a cleanup worker. Managed
        // construction and final teardown never start that background role.
        let retirement = runtime::reserve(&runtime)?;
        Ok(Self {
            connectivity: connectivity::ConnectivityResources::default(),
            video_encoders: HashMap::new(),
            video_decoders: HashMap::new(),
            encoded_images: HashMap::new(),
            encoded_buffers: HashMap::new(),
            encoded_writers: HashMap::new(),
            encoded_callbacks: HashMap::new(),
            decoded_callbacks: HashMap::new(),
            fec_overrides: HashMap::new(),
            video_selectors: HashMap::new(),
            sender_parameter_operations: HashMap::new(),
            sframe_encrypters: HashMap::new(),
            sframe_decrypters: HashMap::new(),
            frame_transformers: HashMap::new(),
            frame_encryptors: HashMap::new(),
            frame_decryptors: HashMap::new(),
            sender_observers: HashMap::new(),
            receiver_observers: HashMap::new(),
            channels: HashMap::new(),
            media_sinks: HashMap::new(),
            transceivers: HashMap::new(),
            senders: HashMap::new(),
            receivers: HashMap::new(),
            track_observers: HashMap::new(),
            local_stream_observers: HashMap::new(),
            media_streams: HashMap::new(),
            track_sources: HashMap::new(),
            audio_track_processors: HashMap::new(),
            media_tracks: HashMap::new(),
            sources: HashMap::new(),
            #[cfg(feature = "native")]
            desktop_options: HashMap::new(),
            #[cfg(feature = "native")]
            camera_device_info: HashMap::new(),
            #[cfg(feature = "native")]
            camera_options: HashMap::new(),
            #[cfg(feature = "native")]
            desktop_metadata: HashMap::new(),
            video_bitrate_allocator_factory,
            video_observers: HashMap::new(),
            video_source_views: HashMap::new(),
            video_buffer_preparations: HashMap::new(),
            raw_video_frames: HashMap::new(),
            raw_video_buffers: HashMap::new(),
            writable_i420_buffers: HashMap::new(),
            video_sinks: HashMap::new(),
            video_source_encoded_sinks: HashMap::new(),
            stream_observers: HashMap::new(),
            streams: HashMap::new(),
            stream_tracks: HashMap::new(),
            audio_processors: HashMap::new(),
            audio_observers: HashMap::new(),
            audio_source_views: HashMap::new(),
            peers: HashMap::new(),
            input,
            factory: cleanup::CaptiveFactory::new(factory),
            readiness,
            closed: false,
            _exclusive: PhantomData,
            retirement,
            runtime: Some(runtime),
        })
    }

    fn check_open(&self) -> Result<(), PeerError> {
        if self.closed {
            Err(error(PeerErrorKind::Closed, "session is closed"))
        } else {
            Ok(())
        }
    }

    /// Replace factory options for subsequently created peers. Existing peers
    /// are unchanged. This can be called before the first peer is created and
    /// after migrating the actor. Native `SetOptions` has no validation result.
    pub fn set_factory_options(&mut self, options: crate::FactoryOptions) -> Result<(), PeerError> {
        self.check_open()?;
        self.factory.set_options(options);
        Ok(())
    }

    pub fn create_peer(
        &mut self,
        config: crate::PeerConfiguration,
    ) -> Result<SessionPeerId, PeerError> {
        self.create_peer_with_configuration(config.into())
    }

    /// Actor-safe creation with complete owned native configuration values.
    pub fn create_peer_with_configuration(
        &mut self,
        config: RtcConfiguration,
    ) -> Result<SessionPeerId, PeerError> {
        self.create_peer_with_dependencies(config, crate::PeerConnectionDependencies::default())
    }

    /// Actor-safe creation with owned per-peer executable providers.
    pub fn create_peer_with_dependencies(
        &mut self,
        config: RtcConfiguration,
        mut dependencies: crate::PeerConnectionDependencies,
    ) -> Result<SessionPeerId, PeerError> {
        self.check_open()?;
        if dependencies.video_bitrate_allocator_factory.is_none() {
            dependencies.video_bitrate_allocator_factory =
                self.video_bitrate_allocator_factory.clone();
        }
        let id = SessionPeerId(next_id()?);
        let value = self
            .factory
            .create_peer_connection_with_dependencies(config, dependencies)?;
        self.peers.insert(
            id,
            Peer {
                value,
                outstanding: HashSet::new(),
            },
        );
        Ok(id)
    }

    /// Owned native snapshot. This can leave the actor, including across
    /// migrations; it contains no peer-affine references.
    pub fn peer_configuration(&self, peer: SessionPeerId) -> Result<RtcConfiguration, PeerError> {
        self.check_open()?;
        let peer = self.peers.get(&peer).ok_or_else(missing)?;
        if peer.value.inner.closed.get() {
            return Err(error(PeerErrorKind::Closed, "peer is closed"));
        }
        Ok(peer.value.configuration())
    }

    /// Read binding RTP admission diagnostics without an SDK role hop. Remains
    /// available after peer/session close while this session owns the peer.
    pub fn peer_call_admission_stats(
        &self,
        peer: SessionPeerId,
    ) -> Result<Option<crate::CallAdmissionStats>, PeerError> {
        Ok(self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .call_admission_stats())
    }

    pub fn set_peer_configuration(
        &mut self,
        peer: SessionPeerId,
        config: RtcConfiguration,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .set_configuration(config)
    }

    fn operation(
        &mut self,
        peer: SessionPeerId,
        submit: impl FnOnce(&PeerConnection) -> Result<OperationId, PeerError>,
    ) -> Result<OperationId, PeerError> {
        self.check_open()?;
        let peer = self.peers.get_mut(&peer).ok_or_else(missing)?;
        Self::submit_operation(peer, submit)
    }
    fn submit_operation<E: From<PeerError>>(
        peer: &mut Peer,
        submit: impl FnOnce(&PeerConnection) -> Result<OperationId, E>,
    ) -> Result<OperationId, E> {
        if peer.value.inner.closed.get() {
            return Err(error(PeerErrorKind::Closed, "peer is closed").into());
        }
        if peer.outstanding.len() >= MAX_OUTSTANDING_OPERATIONS {
            return Err(error(
                PeerErrorKind::ResourceExhausted,
                "consume operation outcomes before submitting more work (limit 64)",
            )
            .into());
        }
        let id = submit(&peer.value)?;
        peer.outstanding.insert(id);
        Ok(id)
    }

    pub fn create_offer(
        &mut self,
        peer: SessionPeerId,
        ice_restart: bool,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, |p| {
            if ice_restart {
                p.create_ice_restart_offer()
            } else {
                p.create_offer()
            }
        })
    }
    pub fn create_offer_with_options(
        &mut self,
        peer: SessionPeerId,
        options: OfferAnswerOptions,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, |p| p.create_offer_with_options(options))
    }
    pub fn create_answer_with_options(
        &mut self,
        peer: SessionPeerId,
        options: OfferAnswerOptions,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, |p| p.create_answer_with_options(options))
    }
    pub fn restart_ice(&mut self, peer: SessionPeerId) -> Result<(), PeerError> {
        self.check_open()?;
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .restart_ice()
    }
    pub fn should_fire_negotiation_needed(
        &mut self,
        peer: SessionPeerId,
        event_id: u32,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .should_fire_negotiation_needed(event_id)
    }
    pub fn set_local_description_auto(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, PeerConnection::set_local_description_auto)
    }
    pub fn reconfigure_bandwidth_estimation(
        &mut self,
        peer: SessionPeerId,
        settings: crate::BandwidthEstimationSettings,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .reconfigure_bandwidth_estimation(settings)
    }
    /// Generic SDK SetTrack for existing audio/video senders. None detaches.
    /// The boolean is the native acceptance result, not a fabricated RTCError.
    pub fn set_sender_track(
        &mut self,
        sender: SessionSenderId,
        track: Option<SessionMediaTrackId>,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        let sender = &self.senders.get(&sender).ok_or_else(missing)?.value;
        let track = track
            .map(|id| {
                self.media_tracks
                    .get(&id)
                    .map(|r| &r.value)
                    .ok_or_else(missing)
            })
            .transpose()?;
        sender.set_media_track(track)
    }
    pub fn set_data_channel_message_observation(
        &mut self,
        peer: SessionPeerId,
        enabled: bool,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .set_data_channel_message_observation(enabled)
    }
    pub fn set_local_description_auto_legacy(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, PeerConnection::set_local_description_auto_legacy)
    }
    pub fn set_local_description_legacy(
        &mut self,
        peer: SessionPeerId,
        description: impl Into<crate::SessionDescriptionBytes>,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, |p| p.set_local_description_legacy(description))
    }
    pub fn set_remote_description_legacy(
        &mut self,
        peer: SessionPeerId,
        description: impl Into<crate::SessionDescriptionBytes>,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, |p| p.set_remote_description_legacy(description))
    }
    pub fn add_ice_candidate_sync(
        &mut self,
        peer: SessionPeerId,
        candidate: IceCandidate,
    ) -> Result<IceCandidateDisposition, PeerError> {
        self.check_open()?;
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .add_ice_candidate_sync(candidate)
    }
    pub fn add_ice_candidate_sync_bytes(
        &mut self,
        peer: SessionPeerId,
        candidate: crate::IceCandidateBytes,
    ) -> Result<IceCandidateDisposition, crate::PeerErrorBytes> {
        self.check_open().map_err(|error| crate::PeerErrorBytes {
            kind: error.kind,
            message: error.message.into_bytes(),
        })?;
        self.peers
            .get(&peer)
            .ok_or_else(|| crate::PeerErrorBytes {
                kind: PeerErrorKind::InvalidParameter,
                message: b"resource does not belong to this session".to_vec(),
            })?
            .value
            .add_ice_candidate_sync_bytes(candidate)
    }
    pub fn remove_ice_candidate_bytes(
        &mut self,
        peer: SessionPeerId,
        candidate: crate::IceCandidateBytes,
    ) -> Result<IceCandidateDisposition, crate::PeerErrorBytes> {
        self.check_open().map_err(|error| crate::PeerErrorBytes {
            kind: error.kind,
            message: error.message.into_bytes(),
        })?;
        self.peers
            .get(&peer)
            .ok_or_else(|| crate::PeerErrorBytes {
                kind: PeerErrorKind::InvalidParameter,
                message: b"resource does not belong to this session".to_vec(),
            })?
            .value
            .remove_ice_candidate_bytes(candidate)
    }
    pub fn remove_ice_candidate(
        &mut self,
        peer: SessionPeerId,
        candidate: IceCandidate,
    ) -> Result<IceCandidateDisposition, PeerError> {
        self.check_open()?;
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .remove_ice_candidate(candidate)
    }
    pub fn add_ice_candidate_bytes(
        &mut self,
        peer: SessionPeerId,
        candidate: crate::IceCandidateBytes,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, |value| value.add_ice_candidate_bytes(candidate))
    }
    pub fn create_answer(&mut self, peer: SessionPeerId) -> Result<OperationId, PeerError> {
        self.operation(peer, PeerConnection::create_answer)
    }
    pub fn set_local_description(
        &mut self,
        peer: SessionPeerId,
        description: SessionDescription,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, |p| p.set_local_description(description))
    }
    pub fn set_remote_description(
        &mut self,
        peer: SessionPeerId,
        description: SessionDescription,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, |p| p.set_remote_description(description))
    }
    pub fn set_local_description_bytes(
        &mut self,
        peer: SessionPeerId,
        description: crate::SessionDescriptionBytes,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, |value| value.set_local_description_bytes(description))
    }
    pub fn set_remote_description_bytes(
        &mut self,
        peer: SessionPeerId,
        description: crate::SessionDescriptionBytes,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, |value| {
            value.set_remote_description_bytes(description)
        })
    }
    pub fn add_ice_candidate(
        &mut self,
        peer: SessionPeerId,
        candidate: IceCandidate,
    ) -> Result<OperationId, PeerError> {
        self.operation(peer, |p| p.add_ice_candidate(candidate))
    }
    pub fn request_stats(&mut self, peer: SessionPeerId) -> Result<OperationId, PeerError> {
        self.operation(peer, PeerConnection::request_stats)
    }
    pub fn request_legacy_stats(
        &mut self,
        peer: SessionPeerId,
        level: crate::LegacyStatsOutputLevel,
    ) -> Result<OperationId, crate::LegacyStatsRequestError> {
        self.check_open()?;
        let peer = self.peers.get_mut(&peer).ok_or_else(missing)?;
        Self::submit_operation(peer, |p| p.request_legacy_stats(level))
    }
    pub fn request_legacy_stats_with_track(
        &mut self,
        peer: SessionPeerId,
        selector: Option<SessionMediaTrackId>,
        level: crate::LegacyStatsOutputLevel,
    ) -> Result<OperationId, crate::LegacyStatsRequestError> {
        self.check_open()?;
        let selector = selector
            .map(|id| {
                self.media_tracks
                    .get(&id)
                    .map(|r| r.value.clone())
                    .ok_or_else(missing)
            })
            .transpose()?;
        let peer = self.peers.get_mut(&peer).ok_or_else(missing)?;
        Self::submit_operation(peer, |p| {
            p.request_legacy_stats_with_track(selector.as_ref(), level)
        })
    }
    pub fn request_sender_stats(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<OperationId, PeerError> {
        self.check_open()?;
        let sender = self.senders.get(&sender).ok_or_else(missing)?;
        let peer = self.peers.get_mut(&sender.peer).ok_or_else(missing)?;
        Self::submit_operation(peer, |p| p.request_sender_stats(&sender.value))
    }
    pub fn request_receiver_stats(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<OperationId, PeerError> {
        self.check_open()?;
        let receiver = self.receivers.get(&receiver).ok_or_else(missing)?;
        let peer = self.peers.get_mut(&receiver.peer).ok_or_else(missing)?;
        Self::submit_operation(peer, |p| p.request_receiver_stats(&receiver.value))
    }
    /// A selector may belong to another peer in this session; native selection
    /// then returns an empty report. None preserves the native null selector.
    pub fn request_stats_with_sender_selector(
        &mut self,
        peer: SessionPeerId,
        selector: Option<SessionSenderId>,
    ) -> Result<OperationId, PeerError> {
        self.check_open()?;
        let selector = selector
            .map(|id| self.senders.get(&id).ok_or_else(missing))
            .transpose()?
            .map(|r| &r.value);
        let peer = self.peers.get_mut(&peer).ok_or_else(missing)?;
        Self::submit_operation(peer, |p| p.request_stats_with_sender_selector(selector))
    }
    pub fn request_stats_with_receiver_selector(
        &mut self,
        peer: SessionPeerId,
        selector: Option<SessionReceiverId>,
    ) -> Result<OperationId, PeerError> {
        self.check_open()?;
        let selector = selector
            .map(|id| self.receivers.get(&id).ok_or_else(missing))
            .transpose()?
            .map(|r| &r.value);
        let peer = self.peers.get_mut(&peer).ok_or_else(missing)?;
        Self::submit_operation(peer, |p| p.request_stats_with_receiver_selector(selector))
    }
    pub fn clear_stats_cache(&mut self, peer: SessionPeerId) -> Result<(), PeerError> {
        self.check_open()?;
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .clear_stats_cache();
        Ok(())
    }
    pub fn descriptions(&mut self, peer: SessionPeerId) -> Result<PeerDescriptions, PeerError> {
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .descriptions()
    }

    pub fn peer_ice_snapshot(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<crate::PeerIceSnapshot, PeerError> {
        self.check_open()?;
        Ok(self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .ice_snapshot())
    }
    pub fn peer_ice_connection_state(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<crate::PeerIceConnectionState, PeerError> {
        Ok(self.peer_ice_snapshot(peer)?.legacy_state)
    }
    pub fn peer_standardized_ice_connection_state(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<crate::PeerIceConnectionState, PeerError> {
        Ok(self.peer_ice_snapshot(peer)?.standardized_state)
    }
    pub fn peer_can_trickle_ice_candidates(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<Option<bool>, PeerError> {
        Ok(self.peer_ice_snapshot(peer)?.can_trickle_ice_candidates)
    }

    /// Read actual native state without draining or interpreting peer events.
    /// Retained peer identities remain queryable after close and shutdown.
    pub fn peer_signaling_state(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<crate::SignalingState, PeerError> {
        Ok(self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .signaling_state())
    }

    pub fn peer_connection_state(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<crate::ConnectionState, PeerError> {
        Ok(self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .peer_connection_state())
    }

    pub fn peer_ice_gathering_state(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<crate::IceGatheringState, PeerError> {
        Ok(self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .ice_gathering_state())
    }

    pub fn descriptions_bytes(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<crate::PeerDescriptionBytes, PeerError> {
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .descriptions_bytes()
    }

    /// Activity notification only, not proof of a particular queued event.
    /// Drain owned events/frames, then wait. Signals coalesce and may be spurious.
    /// Polling never enters the engine, pumps work or advances a clock. A single
    /// actor waiter may register; its waker must schedule, not execute engine work.
    /// DTMF and registered transport observers share this readiness source.
    /// First-packet observer queues are explicit-poll-only and do not signal this
    /// readiness source; it is not a waiter for those observer records.
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.readiness.poll(cx)
    }
    pub async fn ready(&mut self) {
        std::future::poll_fn(|cx| self.poll_ready(cx)).await
    }

    pub fn try_peer_event(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<Option<SessionEvent>, PeerError> {
        let value = self.peers.get_mut(&peer).ok_or_else(missing)?;
        let Some(event) = value.value.try_next_event() else {
            return Ok(None);
        };
        let event = match event {
            PeerConnectionEvent::LegacyStats(stats) => {
                value.outstanding.remove(&stats.operation_id);
                SessionEvent::LegacyStats(stats)
            }
            PeerConnectionEvent::Stats(stats) => {
                value.outstanding.remove(&stats.operation_id);
                SessionEvent::Stats(stats)
            }
            PeerConnectionEvent::OperationComplete(done) => {
                value.outstanding.remove(&done.operation_id);
                SessionEvent::OperationComplete(done)
            }
            PeerConnectionEvent::OperationCompleteBytes {
                operation_id,
                description,
            } => {
                value.outstanding.remove(&operation_id);
                SessionEvent::OperationCompleteBytes {
                    operation_id,
                    description,
                }
            }
            PeerConnectionEvent::OperationFailedBytes {
                operation_id,
                error,
            } => {
                value.outstanding.remove(&operation_id);
                SessionEvent::OperationFailedBytes {
                    operation_id,
                    error,
                }
            }
            PeerConnectionEvent::IceSelectedCandidatePairChanged(value) => {
                SessionEvent::IceSelectedCandidatePairChanged(value)
            }
            PeerConnectionEvent::IceCandidate(v) => SessionEvent::IceCandidate(v),
            PeerConnectionEvent::IceCandidateBytes(value) => SessionEvent::IceCandidateBytes(value),
            PeerConnectionEvent::IceCandidateRemoved(value) => {
                SessionEvent::IceCandidateRemoved(value)
            }
            PeerConnectionEvent::IceCandidateRemovedBytes(value) => {
                SessionEvent::IceCandidateRemovedBytes(value)
            }
            PeerConnectionEvent::ConnectionStateChanged(v) => {
                SessionEvent::ConnectionStateChanged(v)
            }
            PeerConnectionEvent::SignalingStateChanged(v) => SessionEvent::SignalingStateChanged(v),
            PeerConnectionEvent::IceGatheringStateChanged(v) => {
                SessionEvent::IceGatheringStateChanged(v)
            }
            PeerConnectionEvent::IceConnectionStateChanged(value) => {
                SessionEvent::IceConnectionStateChanged(value)
            }
            PeerConnectionEvent::StandardizedIceConnectionStateChanged(value) => {
                SessionEvent::StandardizedIceConnectionStateChanged(value)
            }
            PeerConnectionEvent::NegotiationNeeded { event_id } => {
                SessionEvent::NegotiationNeeded { event_id }
            }
            PeerConnectionEvent::IceCandidateError {
                address,
                port,
                url,
                error_code,
                message,
            } => SessionEvent::IceCandidateError {
                address,
                port,
                url,
                error_code,
                message,
            },
            PeerConnectionEvent::IceCandidateErrorBytes {
                address,
                port,
                url,
                error_code,
                message,
            } => SessionEvent::IceCandidateErrorBytes {
                address,
                port,
                url,
                error_code,
                message,
            },
            PeerConnectionEvent::DataChannel(value) => {
                let channel = SessionChannelId(next_id()?);
                let label = value.label();
                self.channels.insert(channel, Resource { peer, value });
                SessionEvent::DataChannel { channel, label }
            }
            PeerConnectionEvent::Track(value) => {
                let transceiver = self.insert_transceiver(peer, value)?;
                let receiver = self.transceivers[&transceiver].value.receiver;
                SessionEvent::Track {
                    transceiver,
                    receiver,
                }
            }
            PeerConnectionEvent::TrackRemoved(value) => {
                let receiver = self.insert_receiver(peer, value)?;
                SessionEvent::TrackRemoved { receiver }
            }
            PeerConnectionEvent::StreamAdded(value) => SessionEvent::StreamAdded {
                stream: self.insert_media_stream(peer, value)?,
            },
            PeerConnectionEvent::StreamRemoved(value) => SessionEvent::StreamRemoved {
                stream: self.insert_media_stream(peer, value)?,
            },
            PeerConnectionEvent::TrackAdded { receiver, streams } => {
                let receiver = self.insert_receiver(peer, receiver)?;
                let streams = streams
                    .into_iter()
                    .map(|s| self.insert_media_stream(peer, s))
                    .collect::<Result<_, _>>()?;
                SessionEvent::TrackAdded { receiver, streams }
            }
            PeerConnectionEvent::RenegotiationNeeded => SessionEvent::RenegotiationNeeded,
            PeerConnectionEvent::IceConnectionReceivingChanged(value) => {
                SessionEvent::IceConnectionReceivingChanged(value)
            }
            PeerConnectionEvent::InterestingUsage { usage_pattern } => {
                SessionEvent::InterestingUsage { usage_pattern }
            }
            PeerConnectionEvent::DataChannelMessage(value) => {
                SessionEvent::DataChannelMessage(value)
            }
            PeerConnectionEvent::Closed => SessionEvent::Closed,
        };
        Ok(Some(event))
    }

    pub fn create_channel(
        &mut self,
        peer: SessionPeerId,
        label: &str,
        config: DataChannelConfiguration,
    ) -> Result<SessionChannelId, crate::PeerErrorBytes> {
        self.create_channel_bytes(peer, label.as_bytes(), Some(config))
    }

    pub fn create_channel_bytes(
        &mut self,
        peer: SessionPeerId,
        label: &[u8],
        config: Option<DataChannelConfiguration>,
    ) -> Result<SessionChannelId, crate::PeerErrorBytes> {
        self.check_open()?;
        let id = SessionChannelId(next_id()?);
        let value = self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .create_data_channel_bytes(label, config)?;
        self.channels.insert(id, Resource { peer, value });
        Ok(id)
    }
    pub fn send(
        &mut self,
        channel: SessionChannelId,
        message: DataChannelMessage,
    ) -> Result<DataChannelSendResult, PeerError> {
        self.check_open()?;
        Ok(self
            .channels
            .get(&channel)
            .ok_or_else(missing)?
            .value
            .send(message))
    }
    /// Admit a bounded native send without blocking on network-thread completion.
    /// Consume `SendComplete` from `try_channel_event` for the terminal result.
    pub fn send_async(
        &mut self,
        channel: SessionChannelId,
        message: DataChannelMessage,
    ) -> Result<crate::DataChannelSendId, PeerError> {
        self.check_open()?;
        self.channels
            .get(&channel)
            .ok_or_else(missing)?
            .value
            .send_async(message)
    }

    /// Independently sampled native cumulative transport counters.
    pub fn channel_counters(
        &mut self,
        channel: SessionChannelId,
    ) -> Result<crate::DataChannelCounters, PeerError> {
        Ok(self
            .channels
            .get(&channel)
            .ok_or_else(missing)?
            .value
            .counters())
    }
    pub fn channel_configuration(
        &mut self,
        channel: SessionChannelId,
    ) -> Result<DataChannelConfiguration, PeerError> {
        Ok(self
            .channels
            .get(&channel)
            .ok_or_else(missing)?
            .value
            .configuration())
    }

    pub fn channel_label(&mut self, channel: SessionChannelId) -> Result<Vec<u8>, PeerError> {
        Ok(self
            .channels
            .get(&channel)
            .ok_or_else(missing)?
            .value
            .label())
    }

    pub fn channel_state(
        &mut self,
        channel: SessionChannelId,
    ) -> Result<crate::DataChannelState, PeerError> {
        Ok(self
            .channels
            .get(&channel)
            .ok_or_else(missing)?
            .value
            .state())
    }

    pub fn channel_reliable(&mut self, channel: SessionChannelId) -> Result<bool, PeerError> {
        Ok(self
            .channels
            .get(&channel)
            .ok_or_else(missing)?
            .value
            .reliable())
    }

    pub fn channel_buffered_amount(&mut self, channel: SessionChannelId) -> Result<u64, PeerError> {
        Ok(self
            .channels
            .get(&channel)
            .ok_or_else(missing)?
            .value
            .buffered_amount())
    }

    pub fn try_channel_event(
        &mut self,
        channel: SessionChannelId,
    ) -> Result<Option<DataChannelEvent>, PeerError> {
        Ok(self
            .channels
            .get(&channel)
            .ok_or_else(missing)?
            .value
            .try_next_event())
    }
    /// Copy the engine's current data-channel error without protocol progress.
    pub fn channel_error(
        &mut self,
        channel: SessionChannelId,
    ) -> Result<Option<DataChannelError>, PeerError> {
        Ok(self
            .channels
            .get(&channel)
            .ok_or_else(missing)?
            .value
            .error())
    }

    /// Register/unregister native callbacks, retaining already-owned events.
    /// This does not pause transport or provide consumption-driven receive credit.
    pub fn set_channel_event_observation(
        &mut self,
        channel: SessionChannelId,
        enabled: bool,
    ) -> Result<(), PeerError> {
        self.channels
            .get_mut(&channel)
            .ok_or_else(missing)?
            .value
            .set_event_observation(enabled);
        Ok(())
    }

    /// Bounds on the owned channel inbox, not SCTP receive-window credit.
    pub fn channel_receive_queue_limits() -> crate::DataChannelReceiveQueueLimits {
        DataChannel::receive_queue_limits()
    }

    /// Engine send-queue capacity, not a negotiated message-size or receive limit.
    pub fn channel_send_queue_capacity() -> u64 {
        DataChannel::send_queue_capacity()
    }

    pub fn close_channel(&mut self, channel: SessionChannelId) -> Result<(), PeerError> {
        self.channels.get(&channel).ok_or_else(missing)?;
        self.factory.check_control_call()?;
        self.channels
            .get_mut(&channel)
            .ok_or_else(missing)?
            .value
            .close()
    }

    /// Start an owned native event-log destination on the peer's engine
    /// sequences. Output callbacks do not need reentrant actor access.
    pub fn start_peer_event_log(
        &mut self,
        peer: SessionPeerId,
        output: crate::EventLogOutput,
        period: Option<crate::EventLogOutputPeriod>,
    ) -> Result<(), crate::EventLogError> {
        if self.closed {
            return Err(crate::EventLogError::Closed);
        }
        self.peers
            .get(&peer)
            .ok_or(crate::EventLogError::Closed)?
            .value
            .start_event_log(output, period)
    }

    /// Wait for native output shutdown. Safe after moving the application actor.
    pub fn stop_peer_event_log(&mut self, peer: SessionPeerId) -> Result<(), crate::EventLogError> {
        if self.closed {
            return Err(crate::EventLogError::Closed);
        }
        self.peers
            .get(&peer)
            .ok_or(crate::EventLogError::Closed)?
            .value
            .stop_event_log()
    }
    /// Forward native bandwidth-estimation constraints without a binding allocator.
    pub fn set_peer_bitrate(
        &mut self,
        peer: SessionPeerId,
        minimum: Option<u32>,
        start: Option<u32>,
        maximum: Option<u32>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .set_bitrate(minimum, start, maximum)
    }
    /// Current binding observation backlog, separate from native transport
    /// buffering and already-returned owned outputs.
    pub fn peer_event_observation(
        &self,
        peer: SessionPeerId,
    ) -> Result<crate::PeerEventObservation, PeerError> {
        Ok(self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .event_observation())
    }

    /// Idempotently retire one producer and release its actor-owned entry.
    /// Existing native tracks can retain their source until their sender is
    /// stopped/detached. Already transmitted input can reach an open receiver.
    /// Previously returned owned output is unaffected.
    pub fn close_source(&mut self, id: SessionSourceId) -> Result<(), PeerError> {
        if !self.sources.contains_key(&id) {
            return Ok(());
        }
        self.factory.check_control_call()?;
        if let Some(source) = self.sources.get_mut(&id) {
            source.close()?;
            self.sources.remove(&id);
        }
        Ok(())
    }

    /// Create an owning raw-video producer for the selected encoder factory.
    pub fn create_raw_video_source(&mut self) -> Result<SessionSourceId, PeerError> {
        self.check_open()?;
        let id = SessionSourceId(next_id()?);
        let source = self.factory.create_video_source()?;
        self.sources.insert(id, Source::RawVideo(source));
        Ok(id)
    }

    /// Native container creation does not change SDP or synchronize receivers.
    pub fn create_media_stream(
        &mut self,
        name: &str,
    ) -> Result<SessionLocalMediaStreamId, PeerError> {
        self.check_open()?;
        let stream = self.factory.create_media_stream(name)?;
        let id = SessionLocalMediaStreamId(next_id()?);
        self.streams.insert(id, stream);
        Ok(id)
    }
    fn retain_stream_track(&mut self, track: StreamTrack) -> Result<SessionTrackId, PeerError> {
        let id = SessionTrackId(next_id()?);
        self.stream_tracks.insert(id, track);
        Ok(id)
    }
    pub fn create_source_track(
        &mut self,
        source: SessionSourceId,
        name: &str,
    ) -> Result<SessionTrackId, PeerError> {
        self.check_open()?;
        let track = match self.sources.get(&source).ok_or_else(missing)? {
            Source::Pcm(source) => {
                StreamTrack::Audio(self.factory.create_audio_track(name, source)?)
            }
            Source::Opus(source) => {
                StreamTrack::Audio(self.factory.create_encoded_audio_track(name, source)?)
            }
            source => StreamTrack::Video(source.video_track(&self.factory, name)?),
        };
        self.retain_stream_track(track)
    }
    pub fn sender_track(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<Option<SessionTrackId>, PeerError> {
        self.check_open()?;
        let sender = &self.senders.get(&sender).ok_or_else(missing)?.value;
        let track = sender
            .audio_track()
            .map(StreamTrack::Audio)
            .or_else(|| sender.track().map(StreamTrack::Video));
        track
            .map(|track| self.retain_stream_track(track))
            .transpose()
    }
    pub fn receiver_track(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<SessionTrackId>, PeerError> {
        self.check_open()?;
        let receiver = &self.receivers.get(&receiver).ok_or_else(missing)?.value;
        let track = receiver
            .audio_track()
            .map(StreamTrack::Audio)
            .or_else(|| receiver.track().map(StreamTrack::Video));
        track
            .map(|track| self.retain_stream_track(track))
            .transpose()
    }
    pub fn local_stream_id(&mut self, id: SessionLocalMediaStreamId) -> Result<String, PeerError> {
        Ok(self.streams.get(&id).ok_or_else(missing)?.id())
    }
    pub fn local_stream_audio_tracks(
        &mut self,
        id: SessionLocalMediaStreamId,
    ) -> Result<Vec<SessionTrackId>, PeerError> {
        let tracks = self.streams.get(&id).ok_or_else(missing)?.audio_tracks();
        tracks
            .into_iter()
            .map(|track| self.retain_stream_track(StreamTrack::Audio(track)))
            .collect()
    }
    pub fn local_stream_video_tracks(
        &mut self,
        id: SessionLocalMediaStreamId,
    ) -> Result<Vec<SessionTrackId>, PeerError> {
        let tracks = self.streams.get(&id).ok_or_else(missing)?.video_tracks();
        tracks
            .into_iter()
            .map(|track| self.retain_stream_track(StreamTrack::Video(track)))
            .collect()
    }
    pub fn find_media_stream_audio_track(
        &mut self,
        id: SessionLocalMediaStreamId,
        name: &str,
    ) -> Result<Option<SessionTrackId>, PeerError> {
        let track = self
            .streams
            .get(&id)
            .ok_or_else(missing)?
            .find_audio_track(name);
        track
            .map(|track| self.retain_stream_track(StreamTrack::Audio(track)))
            .transpose()
    }
    pub fn find_media_stream_video_track(
        &mut self,
        id: SessionLocalMediaStreamId,
        name: &str,
    ) -> Result<Option<SessionTrackId>, PeerError> {
        let track = self
            .streams
            .get(&id)
            .ok_or_else(missing)?
            .find_video_track(name);
        track
            .map(|track| self.retain_stream_track(StreamTrack::Video(track)))
            .transpose()
    }
    pub fn local_add_media_stream_track(
        &mut self,
        stream: SessionLocalMediaStreamId,
        track: SessionTrackId,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        let stream = self.streams.get(&stream).ok_or_else(missing)?;
        Ok(match self.stream_tracks.get(&track).ok_or_else(missing)? {
            StreamTrack::Audio(track) => stream.add_audio_track(track),
            StreamTrack::Video(track) => stream.add_video_track(track),
        })
    }
    pub fn local_remove_media_stream_track(
        &mut self,
        stream: SessionLocalMediaStreamId,
        track: SessionTrackId,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        let stream = self.streams.get(&stream).ok_or_else(missing)?;
        Ok(match self.stream_tracks.get(&track).ok_or_else(missing)? {
            StreamTrack::Audio(track) => stream.remove_audio_track(track),
            StreamTrack::Video(track) => stream.remove_video_track(track),
        })
    }
    pub fn media_stream_tracks_same(
        &mut self,
        a: SessionTrackId,
        b: SessionTrackId,
    ) -> Result<bool, PeerError> {
        Ok(
            match (
                self.stream_tracks.get(&a).ok_or_else(missing)?,
                self.stream_tracks.get(&b).ok_or_else(missing)?,
            ) {
                (StreamTrack::Audio(a), StreamTrack::Audio(b)) => a.same_resource(b),
                (StreamTrack::Video(a), StreamTrack::Video(b)) => a.same_resource(b),
                _ => false,
            },
        )
    }
    pub fn media_stream_track_id(&mut self, id: SessionTrackId) -> Result<String, PeerError> {
        Ok(match self.stream_tracks.get(&id).ok_or_else(missing)? {
            StreamTrack::Audio(track) => track.id(),
            StreamTrack::Video(track) => track.id(),
        })
    }
    pub fn media_stream_track_kind(
        &mut self,
        id: SessionTrackId,
    ) -> Result<crate::MediaType, PeerError> {
        Ok(match self.stream_tracks.get(&id).ok_or_else(missing)? {
            StreamTrack::Audio(_) => crate::MediaType::Audio,
            StreamTrack::Video(_) => crate::MediaType::Video,
        })
    }
    pub fn media_stream_track_enabled(&mut self, id: SessionTrackId) -> Result<bool, PeerError> {
        Ok(match self.stream_tracks.get(&id).ok_or_else(missing)? {
            StreamTrack::Audio(track) => track.enabled(),
            StreamTrack::Video(track) => track.enabled(),
        })
    }
    pub fn set_media_stream_track_enabled(
        &mut self,
        id: SessionTrackId,
        enabled: bool,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        Ok(match self.stream_tracks.get(&id).ok_or_else(missing)? {
            StreamTrack::Audio(track) => track.set_enabled(enabled),
            StreamTrack::Video(track) => track.set_enabled(enabled),
        })
    }
    /// Replace using the actual owning track returned by a container/getter.
    pub fn replace_sender_track(
        &mut self,
        sender: SessionSenderId,
        track: Option<SessionTrackId>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let sender = &self.senders.get(&sender).ok_or_else(missing)?.value;
        match track
            .map(|id| self.stream_tracks.get(&id).ok_or_else(missing))
            .transpose()?
        {
            Some(StreamTrack::Audio(track)) => sender.set_audio_track(Some(track)),
            Some(StreamTrack::Video(track)) => sender.set_track(Some(track)),
            None if sender.media_type() == crate::MediaType::Audio => sender.set_audio_track(None),
            None => sender.set_track(None),
        }
    }
    pub fn observe_media_stream(
        &mut self,
        stream: SessionLocalMediaStreamId,
        capacity: std::num::NonZeroUsize,
    ) -> Result<SessionMediaStreamObserverId, PeerError> {
        self.check_open()?;
        let observer = self
            .streams
            .get(&stream)
            .ok_or_else(missing)?
            .observe_changes(capacity)?;
        let id = SessionMediaStreamObserverId(next_id()?);
        self.local_stream_observers.insert(id, observer);
        Ok(id)
    }
    pub fn try_media_stream_event(
        &mut self,
        id: SessionMediaStreamObserverId,
    ) -> Result<Option<crate::MediaStreamEvent>, PeerError> {
        Ok(self
            .local_stream_observers
            .get(&id)
            .ok_or_else(missing)?
            .try_next_event())
    }
    pub fn take_media_stream_dropped(
        &mut self,
        id: SessionMediaStreamObserverId,
    ) -> Result<u64, PeerError> {
        Ok(self
            .local_stream_observers
            .get(&id)
            .ok_or_else(missing)?
            .take_dropped_events())
    }
    pub fn close_media_stream_observer(
        &mut self,
        id: SessionMediaStreamObserverId,
    ) -> Result<(), PeerError> {
        self.local_stream_observers
            .get(&id)
            .ok_or_else(missing)?
            .close();
        Ok(())
    }
    pub fn release_media_stream_observer(&mut self, id: SessionMediaStreamObserverId) {
        self.local_stream_observers.remove(&id);
    }
    pub fn release_media_stream_track(&mut self, id: SessionTrackId) {
        self.stream_tracks.remove(&id);
    }
    pub fn local_release_media_stream(&mut self, id: SessionLocalMediaStreamId) {
        self.streams.remove(&id);
    }
    fn video_track_handle(&self, id: SessionTrackId) -> Result<&crate::VideoTrack, PeerError> {
        match self.stream_tracks.get(&id).ok_or_else(missing)? {
            StreamTrack::Video(track) => Ok(track),
            _ => Err(missing()),
        }
    }
    fn retain_video_source_view(
        &mut self,
        view: crate::VideoSourceView,
    ) -> Result<SessionVideoSourceViewId, PeerError> {
        let id = SessionVideoSourceViewId(next_id()?);
        self.video_source_views.insert(id, view);
        Ok(id)
    }
    /// View a raw producer without creating another producer or exposing handles.
    pub fn video_source_view(
        &mut self,
        source: SessionSourceId,
    ) -> Result<SessionVideoSourceViewId, PeerError> {
        self.check_open()?;
        let Some(Source::RawVideo(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        self.retain_video_source_view(source.as_view())
    }
    pub fn video_track_source_view(
        &mut self,
        track: SessionTrackId,
    ) -> Result<Option<SessionVideoSourceViewId>, PeerError> {
        self.check_open()?;
        let view = self.video_track_handle(track)?.source();
        view.map(|view| self.retain_video_source_view(view))
            .transpose()
    }
    pub fn video_source_status(
        &mut self,
        view: SessionVideoSourceViewId,
    ) -> Result<crate::VideoSourceStatus, PeerError> {
        Ok(self
            .video_source_views
            .get(&view)
            .ok_or_else(missing)?
            .status())
    }
    /// Adapter admission refusals, distinct from native source statistics.
    pub fn video_source_rejected_sink_requests(
        &mut self,
        view: SessionVideoSourceViewId,
    ) -> Result<Option<u64>, PeerError> {
        Ok(self
            .video_source_views
            .get(&view)
            .ok_or_else(missing)?
            .rejected_sink_requests())
    }
    /// Actual SDK interface equality; distinct proxies may share a producer.
    pub fn video_source_views_same_interface(
        &mut self,
        a: SessionVideoSourceViewId,
        b: SessionVideoSourceViewId,
    ) -> Result<bool, PeerError> {
        Ok(self
            .video_source_views
            .get(&a)
            .ok_or_else(missing)?
            .same_interface(self.video_source_views.get(&b).ok_or_else(missing)?))
    }
    /// Forward a native request, not a promise of a generated frame.
    pub fn request_video_source_refresh(
        &mut self,
        view: SessionVideoSourceViewId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_source_views
            .get(&view)
            .ok_or_else(missing)?
            .request_refresh_frame();
        Ok(())
    }
    pub fn generate_video_source_key_frame(
        &mut self,
        view: SessionVideoSourceViewId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_source_views
            .get(&view)
            .ok_or_else(missing)?
            .generate_key_frame();
        Ok(())
    }
    pub fn process_video_source_constraints(
        &mut self,
        view: SessionVideoSourceViewId,
        constraints: crate::VideoSourceConstraints,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_source_views
            .get(&view)
            .ok_or_else(missing)?
            .process_constraints(constraints);
        Ok(())
    }
    pub fn request_video_track_refresh(&mut self, track: SessionTrackId) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_track_handle(track)?.request_refresh_frame();
        Ok(())
    }
    pub fn video_track_status(
        &mut self,
        track: SessionTrackId,
    ) -> Result<crate::VideoTrackStatus, PeerError> {
        Ok(self.video_track_handle(track)?.status())
    }
    pub fn set_video_track_should_receive(
        &mut self,
        track: SessionTrackId,
        receive: bool,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_track_handle(track)?.set_should_receive(receive);
        Ok(())
    }
    pub fn set_video_track_content_hint(
        &mut self,
        track: SessionTrackId,
        hint: crate::VideoContentHint,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_track_handle(track)?.set_content_hint(hint);
        Ok(())
    }
    pub fn observe_video_source_changes(
        &mut self,
        view: SessionVideoSourceViewId,
        capacity: std::num::NonZeroUsize,
    ) -> Result<SessionVideoObserverId, PeerError> {
        self.check_open()?;
        let observer = self
            .video_source_views
            .get(&view)
            .ok_or_else(missing)?
            .observe_changes(capacity)?;
        let id = SessionVideoObserverId(next_id()?);
        self.video_observers.insert(id, observer);
        Ok(id)
    }
    pub fn observe_video_track_changes(
        &mut self,
        track: SessionTrackId,
        capacity: std::num::NonZeroUsize,
    ) -> Result<SessionVideoObserverId, PeerError> {
        self.check_open()?;
        let observer = self.video_track_handle(track)?.observe_changes(capacity)?;
        let id = SessionVideoObserverId(next_id()?);
        self.video_observers.insert(id, observer);
        Ok(id)
    }
    pub fn try_video_observer_event(
        &mut self,
        id: SessionVideoObserverId,
    ) -> Result<Option<crate::VideoObserverEvent>, PeerError> {
        Ok(self
            .video_observers
            .get(&id)
            .ok_or_else(missing)?
            .try_next_event())
    }
    pub fn take_video_observer_dropped(
        &mut self,
        id: SessionVideoObserverId,
    ) -> Result<u64, PeerError> {
        Ok(self
            .video_observers
            .get(&id)
            .ok_or_else(missing)?
            .take_dropped_events())
    }
    pub fn close_video_observer(&mut self, id: SessionVideoObserverId) -> Result<(), PeerError> {
        self.video_observers.get(&id).ok_or_else(missing)?.close();
        Ok(())
    }
    pub fn release_video_observer(&mut self, id: SessionVideoObserverId) {
        self.video_observers.remove(&id);
    }
    pub fn release_video_source_view(&mut self, id: SessionVideoSourceViewId) {
        self.video_source_views.remove(&id);
    }

    pub fn attach_video_track_sink(
        &mut self,
        track: SessionTrackId,
    ) -> Result<SessionVideoSinkId, PeerError> {
        self.attach_video_track_sink_with_wants(track, &crate::VideoSinkWants::default())
    }
    pub fn attach_video_track_sink_with_wants(
        &mut self,
        track: SessionTrackId,
        wants: &crate::VideoSinkWants,
    ) -> Result<SessionVideoSinkId, PeerError> {
        self.check_open()?;
        let id = SessionVideoSinkId(next_id()?);
        let sink = self
            .video_track_handle(track)?
            .attach_sink_with_wants(wants)?;
        self.video_sinks.insert(id, OwnedVideoSink::Track(sink));
        Ok(id)
    }
    pub fn attach_video_source_sink(
        &mut self,
        source: SessionVideoSourceViewId,
    ) -> Result<SessionVideoSinkId, PeerError> {
        self.attach_video_source_sink_with_wants(source, &crate::VideoSinkWants::default())
    }
    /// A direct source registration, independent of receiver receive-mode
    /// selection. Retains the source after its outer ID is released.
    pub fn attach_video_source_sink_with_wants(
        &mut self,
        source: SessionVideoSourceViewId,
        wants: &crate::VideoSinkWants,
    ) -> Result<SessionVideoSinkId, PeerError> {
        self.check_open()?;
        let id = SessionVideoSinkId(next_id()?);
        let sink = self
            .video_source_views
            .get(&source)
            .ok_or_else(missing)?
            .attach_sink_with_wants(wants)?;
        self.video_sinks.insert(id, OwnedVideoSink::Source(sink));
        Ok(id)
    }
    /// Another owner of the same registration, not a second SDK attachment.
    pub fn alias_video_sink(
        &mut self,
        sink: SessionVideoSinkId,
    ) -> Result<SessionVideoSinkId, PeerError> {
        self.check_open()?;
        let value = self.video_sinks.get(&sink).ok_or_else(missing)?.clone();
        let id = SessionVideoSinkId(next_id()?);
        self.video_sinks.insert(id, value);
        Ok(id)
    }
    pub fn video_sink_wants(
        &mut self,
        sink: SessionVideoSinkId,
    ) -> Result<Option<crate::VideoSinkWants>, PeerError> {
        Ok(self.video_sinks.get(&sink).ok_or_else(missing)?.wants())
    }
    pub fn update_video_sink_wants(
        &mut self,
        sink: SessionVideoSinkId,
        wants: &crate::VideoSinkWants,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_sinks
            .get(&sink)
            .ok_or_else(missing)?
            .update_wants(wants)
    }
    pub fn try_video_sink_native_frame(
        &mut self,
        sink: SessionVideoSinkId,
    ) -> Result<Option<SessionRawVideoFrameId>, PeerError> {
        let frame = self
            .video_sinks
            .get(&sink)
            .ok_or_else(missing)?
            .native_frame();
        frame
            .map(|frame| self.retain_raw_video_frame(frame))
            .transpose()
    }
    pub fn try_video_sink_frame(
        &mut self,
        sink: SessionVideoSinkId,
    ) -> Result<Option<crate::VideoFrame>, PeerError> {
        Ok(self.video_sinks.get(&sink).ok_or_else(missing)?.frame())
    }
    pub fn try_video_sink_event(
        &mut self,
        sink: SessionVideoSinkId,
    ) -> Result<Option<crate::VideoSinkEvent>, PeerError> {
        Ok(self.video_sinks.get(&sink).ok_or_else(missing)?.event())
    }
    pub fn video_sink_dropped_frames(
        &mut self,
        sink: SessionVideoSinkId,
    ) -> Result<u64, PeerError> {
        Ok(self
            .video_sinks
            .get(&sink)
            .ok_or_else(missing)?
            .dropped_frames())
    }
    pub fn video_sink_dropped_events(
        &mut self,
        sink: SessionVideoSinkId,
    ) -> Result<u64, PeerError> {
        Ok(self
            .video_sinks
            .get(&sink)
            .ok_or_else(missing)?
            .dropped_events())
    }
    pub fn close_video_sink(&mut self, sink: SessionVideoSinkId) -> Result<(), PeerError> {
        self.video_sinks.get_mut(&sink).ok_or_else(missing)?.close()
    }
    pub fn release_video_sink(&mut self, sink: SessionVideoSinkId) {
        self.video_sinks.remove(&sink);
    }

    pub fn attach_video_source_encoded_output(
        &mut self,
        source: SessionVideoSourceViewId,
    ) -> Result<SessionVideoSourceEncodedSinkId, PeerError> {
        self.check_open()?;
        let id = SessionVideoSourceEncodedSinkId(next_id()?);
        let sink = self
            .video_source_views
            .get(&source)
            .ok_or_else(missing)?
            .attach_encoded_output()?;
        self.video_source_encoded_sinks.insert(id, sink);
        Ok(id)
    }
    pub fn alias_video_source_encoded_sink(
        &mut self,
        sink: SessionVideoSourceEncodedSinkId,
    ) -> Result<SessionVideoSourceEncodedSinkId, PeerError> {
        self.check_open()?;
        let value = self
            .video_source_encoded_sinks
            .get(&sink)
            .ok_or_else(missing)?
            .clone();
        let id = SessionVideoSourceEncodedSinkId(next_id()?);
        self.video_source_encoded_sinks.insert(id, value);
        Ok(id)
    }
    pub fn try_video_source_encoded_frame(
        &mut self,
        sink: SessionVideoSourceEncodedSinkId,
    ) -> Result<Option<crate::RecordableVideoFrame>, PeerError> {
        Ok(self
            .video_source_encoded_sinks
            .get(&sink)
            .ok_or_else(missing)?
            .try_next_frame())
    }
    pub fn try_video_source_encoded_event(
        &mut self,
        sink: SessionVideoSourceEncodedSinkId,
    ) -> Result<Option<crate::VideoSinkEvent>, PeerError> {
        Ok(self
            .video_source_encoded_sinks
            .get(&sink)
            .ok_or_else(missing)?
            .try_next_event())
    }
    pub fn video_source_encoded_dropped_frames(
        &mut self,
        sink: SessionVideoSourceEncodedSinkId,
    ) -> Result<u64, PeerError> {
        Ok(self
            .video_source_encoded_sinks
            .get(&sink)
            .ok_or_else(missing)?
            .dropped_frames())
    }
    pub fn video_source_encoded_dropped_events(
        &mut self,
        sink: SessionVideoSourceEncodedSinkId,
    ) -> Result<u64, PeerError> {
        Ok(self
            .video_source_encoded_sinks
            .get(&sink)
            .ok_or_else(missing)?
            .dropped_events())
    }
    pub fn close_video_source_encoded_sink(
        &mut self,
        sink: SessionVideoSourceEncodedSinkId,
    ) -> Result<(), PeerError> {
        self.video_source_encoded_sinks
            .get(&sink)
            .ok_or_else(missing)?
            .close();
        Ok(())
    }
    pub fn release_video_source_encoded_sink(&mut self, sink: SessionVideoSourceEncodedSinkId) {
        self.video_source_encoded_sinks.remove(&sink);
    }

    /// Capture an owning interface alias without creating another producer.
    pub fn audio_source_view(
        &mut self,
        source: SessionSourceId,
    ) -> Result<SessionAudioSourceViewId, PeerError> {
        self.check_open()?;
        let view = self
            .sources
            .get(&source)
            .ok_or_else(missing)?
            .audio_source()?
            .as_source_view();
        let id = SessionAudioSourceViewId(next_id()?);
        self.audio_source_views.insert(id, view);
        Ok(id)
    }
    pub fn sender_audio_source_view(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<Option<SessionAudioSourceViewId>, PeerError> {
        self.check_open()?;
        let Some(track) = self
            .senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .audio_track()
        else {
            return Ok(None);
        };
        let Some(view) = track.source() else {
            return Ok(None);
        };
        let id = SessionAudioSourceViewId(next_id()?);
        self.audio_source_views.insert(id, view);
        Ok(Some(id))
    }
    pub fn receiver_audio_source_view(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<SessionAudioSourceViewId>, PeerError> {
        self.check_open()?;
        let Some(track) = self
            .receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .audio_track()
        else {
            return Ok(None);
        };
        let Some(view) = track.source() else {
            return Ok(None);
        };
        let id = SessionAudioSourceViewId(next_id()?);
        self.audio_source_views.insert(id, view);
        Ok(Some(id))
    }
    /// Native per-track processor acquisition, not voice-engine APM inference.
    pub fn sender_audio_processor(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<Option<SessionAudioTrackProcessorId>, PeerError> {
        self.check_open()?;
        let Some(track) = self
            .senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .audio_track()
        else {
            return Ok(None);
        };
        let Some(processor) = track.processor() else {
            return Ok(None);
        };
        let id = SessionAudioTrackProcessorId(next_id()?);
        self.audio_track_processors.insert(id, processor);
        Ok(Some(id))
    }
    pub fn receiver_audio_processor(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<SessionAudioTrackProcessorId>, PeerError> {
        self.check_open()?;
        let Some(track) = self
            .receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .audio_track()
        else {
            return Ok(None);
        };
        let Some(processor) = track.processor() else {
            return Ok(None);
        };
        let id = SessionAudioTrackProcessorId(next_id()?);
        self.audio_track_processors.insert(id, processor);
        Ok(Some(id))
    }
    pub fn audio_processor_statistics(
        &mut self,
        id: SessionAudioTrackProcessorId,
        has_remote_tracks: bool,
    ) -> Result<crate::AudioProcessorStatistics, PeerError> {
        Ok(self
            .audio_track_processors
            .get(&id)
            .ok_or_else(missing)?
            .stats(has_remote_tracks))
    }
    pub fn audio_processors_same(
        &mut self,
        a: SessionAudioTrackProcessorId,
        b: SessionAudioTrackProcessorId,
    ) -> Result<bool, PeerError> {
        Ok(self
            .audio_track_processors
            .get(&a)
            .ok_or_else(missing)?
            .same_resource(self.audio_track_processors.get(&b).ok_or_else(missing)?))
    }
    pub fn release_audio_track_processor(&mut self, id: SessionAudioTrackProcessorId) {
        self.audio_track_processors.remove(&id);
    }

    pub fn audio_source_views_same(
        &mut self,
        a: SessionAudioSourceViewId,
        b: SessionAudioSourceViewId,
    ) -> Result<bool, PeerError> {
        Ok(self
            .audio_source_views
            .get(&a)
            .ok_or_else(missing)?
            .same_resource(self.audio_source_views.get(&b).ok_or_else(missing)?))
    }
    pub fn audio_source_view_state(
        &mut self,
        id: SessionAudioSourceViewId,
    ) -> Result<crate::AudioSourceState, PeerError> {
        Ok(self
            .audio_source_views
            .get(&id)
            .ok_or_else(missing)?
            .state())
    }
    pub fn audio_source_view_is_remote(
        &mut self,
        id: SessionAudioSourceViewId,
    ) -> Result<bool, PeerError> {
        Ok(self
            .audio_source_views
            .get(&id)
            .ok_or_else(missing)?
            .is_remote())
    }
    pub fn audio_source_view_options(
        &mut self,
        id: SessionAudioSourceViewId,
    ) -> Result<crate::AudioSourceOptions, PeerError> {
        Ok(self
            .audio_source_views
            .get(&id)
            .ok_or_else(missing)?
            .options())
    }
    pub fn set_audio_source_view_options(
        &mut self,
        id: SessionAudioSourceViewId,
        options: &crate::AudioSourceOptions,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.audio_source_views
            .get(&id)
            .ok_or_else(missing)?
            .set_options(options)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn audio_source_view_volume(
        &mut self,
        id: SessionAudioSourceViewId,
    ) -> Result<f64, PeerError> {
        Ok(self
            .audio_source_views
            .get(&id)
            .ok_or_else(missing)?
            .volume())
    }
    pub fn set_audio_source_view_volume(
        &mut self,
        id: SessionAudioSourceViewId,
        volume: f64,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.audio_source_views
            .get(&id)
            .ok_or_else(missing)?
            .set_volume(volume)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn observe_audio_source_changes(
        &mut self,
        id: SessionAudioSourceViewId,
        capacity: std::num::NonZeroUsize,
    ) -> Result<SessionAudioObserverId, PeerError> {
        self.observe_audio_source(id, false, capacity)
    }
    pub fn observe_audio_source_volume(
        &mut self,
        id: SessionAudioSourceViewId,
        capacity: std::num::NonZeroUsize,
    ) -> Result<SessionAudioObserverId, PeerError> {
        self.observe_audio_source(id, true, capacity)
    }
    fn observe_audio_source(
        &mut self,
        id: SessionAudioSourceViewId,
        volume: bool,
        capacity: std::num::NonZeroUsize,
    ) -> Result<SessionAudioObserverId, PeerError> {
        self.check_open()?;
        let view = self.audio_source_views.get(&id).ok_or_else(missing)?;
        let observer = if volume {
            view.observe_volume(capacity)?
        } else {
            view.observe_changes(capacity)?
        };
        let id = SessionAudioObserverId(next_id()?);
        self.audio_observers.insert(id, observer);
        Ok(id)
    }
    pub fn observe_receiver_audio_track(
        &mut self,
        receiver: SessionReceiverId,
        capacity: std::num::NonZeroUsize,
    ) -> Result<SessionAudioObserverId, PeerError> {
        self.check_open()?;
        let track = self
            .receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .audio_track()
            .ok_or_else(missing)?;
        let observer = track.observe_changes(capacity)?;
        let id = SessionAudioObserverId(next_id()?);
        self.audio_observers.insert(id, observer);
        Ok(id)
    }
    pub fn set_receiver_audio_track_enabled(
        &mut self,
        receiver: SessionReceiverId,
        enabled: bool,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        Ok(self
            .receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .audio_track()
            .ok_or_else(missing)?
            .set_enabled(enabled))
    }
    pub fn try_audio_observer_event(
        &mut self,
        id: SessionAudioObserverId,
    ) -> Result<Option<crate::AudioObserverEvent>, PeerError> {
        Ok(self
            .audio_observers
            .get(&id)
            .ok_or_else(missing)?
            .try_next_event())
    }
    pub fn take_audio_observer_dropped(
        &mut self,
        id: SessionAudioObserverId,
    ) -> Result<u64, PeerError> {
        Ok(self
            .audio_observers
            .get(&id)
            .ok_or_else(missing)?
            .take_dropped_events())
    }
    /// Unregister but retain already queued events for consumption.
    pub fn close_audio_observer(&mut self, id: SessionAudioObserverId) -> Result<(), PeerError> {
        self.audio_observers.get(&id).ok_or_else(missing)?.close();
        Ok(())
    }
    pub fn release_audio_observer(&mut self, id: SessionAudioObserverId) {
        self.audio_observers.remove(&id);
    }
    /// Release this alias, not the producer or other aliases/observers.
    pub fn release_audio_source_view(&mut self, id: SessionAudioSourceViewId) {
        self.audio_source_views.remove(&id);
    }
    /// Invoke native source SetOptions. This reports admission/invocation, not
    /// an atomic voice-engine reconfiguration or effective processing state.
    pub fn set_audio_source_options(
        &mut self,
        source: SessionSourceId,
        options: &crate::AudioSourceOptions,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.sources
            .get(&source)
            .ok_or_else(missing)?
            .audio_source()?
            .set_options(options)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    /// Native volume call, including native no-op source implementations.
    pub fn set_audio_source_volume(
        &mut self,
        source: SessionSourceId,
        volume: f64,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.sources
            .get(&source)
            .ok_or_else(missing)?
            .audio_source()?
            .set_volume(volume)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn set_audio_track_volume(
        &mut self,
        sender: SessionSenderId,
        volume: f64,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .set_audio_track_volume(volume)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }

    /// Actual native options on an owned PCM or encoded-Opus source.
    pub fn audio_source_options(
        &mut self,
        source: SessionSourceId,
    ) -> Result<crate::AudioSourceOptions, PeerError> {
        self.check_open()?;
        Ok(self
            .sources
            .get(&source)
            .ok_or_else(missing)?
            .audio_source()?
            .options())
    }
    pub fn audio_source_state(
        &mut self,
        source: SessionSourceId,
    ) -> Result<crate::AudioSourceState, PeerError> {
        self.check_open()?;
        Ok(self
            .sources
            .get(&source)
            .ok_or_else(missing)?
            .audio_source()?
            .state())
    }
    pub fn audio_source_is_remote(&mut self, source: SessionSourceId) -> Result<bool, PeerError> {
        self.check_open()?;
        Ok(self
            .sources
            .get(&source)
            .ok_or_else(missing)?
            .audio_source()?
            .is_remote())
    }
    pub fn audio_source_volume(&mut self, source: SessionSourceId) -> Result<f64, PeerError> {
        self.check_open()?;
        Ok(self
            .sources
            .get(&source)
            .ok_or_else(missing)?
            .audio_source()?
            .volume())
    }
    /// Inspect the actual native sender track, not the retained source map.
    pub fn audio_track_status(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<Option<crate::AudioTrackStatus>, PeerError> {
        self.check_open()?;
        Ok(self
            .senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .audio_track_status())
    }

    /// Read the native PCM capture timeline without pumping engine work.
    pub fn pcm_capture_time_now(
        &mut self,
        source: SessionSourceId,
    ) -> Result<std::time::Duration, PeerError> {
        self.check_open()?;
        let Some(Source::Pcm(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        source
            .capture_time_now()
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }

    /// Submit PCM with an actual capture instant mapped into the native domain.
    pub fn push_pcm_at(
        &mut self,
        source: SessionSourceId,
        frame: &crate::AudioPcmFrame,
        capture_time: std::time::Duration,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let Some(Source::Pcm(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        source
            .push_frame_at(frame, capture_time)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }

    fn raw_video_error(value: crate::RawVideoError) -> PeerError {
        error(PeerErrorKind::InvalidParameter, &value.to_string())
    }
    fn retain_raw_video_frame(
        &mut self,
        frame: crate::RawVideoFrame,
    ) -> Result<SessionRawVideoFrameId, PeerError> {
        let id = SessionRawVideoFrameId(next_id()?);
        self.raw_video_frames.insert(id, frame);
        Ok(id)
    }
    fn retain_raw_video_buffer(
        &mut self,
        buffer: crate::RawVideoBuffer,
    ) -> Result<SessionRawVideoBufferId, PeerError> {
        let id = SessionRawVideoBufferId(next_id()?);
        self.raw_video_buffers.insert(id, buffer);
        Ok(id)
    }
    fn retain_writable_i420(
        &mut self,
        buffer: crate::WritableI420Buffer,
    ) -> Result<SessionWritableI420BufferId, PeerError> {
        let id = SessionWritableI420BufferId(next_id()?);
        self.writable_i420_buffers.insert(id, buffer);
        Ok(id)
    }
    /// Construct captive SDK ownership from an ordinary owned packed-I420 value.
    pub fn create_native_video_frame(
        &mut self,
        value: &crate::VideoFrame,
    ) -> Result<SessionRawVideoFrameId, PeerError> {
        self.check_open()?;
        self.retain_raw_video_frame(
            crate::RawVideoFrame::from_legacy(value).map_err(Self::raw_video_error)?,
        )
    }
    pub fn native_video_frame_from_buffer(
        &mut self,
        buffer: SessionRawVideoBufferId,
    ) -> Result<SessionRawVideoFrameId, PeerError> {
        self.check_open()?;
        let frame = crate::RawVideoFrame::from_buffer(
            self.raw_video_buffers.get(&buffer).ok_or_else(missing)?,
        );
        self.retain_raw_video_frame(frame)
    }
    pub fn native_video_frame_from_render_time_ms(
        &mut self,
        buffer: SessionRawVideoBufferId,
        rtp_timestamp: u32,
        milliseconds: i64,
        rotation: crate::VideoRotation,
    ) -> Result<SessionRawVideoFrameId, PeerError> {
        self.check_open()?;
        let frame = crate::RawVideoFrame::from_render_time_ms(
            self.raw_video_buffers.get(&buffer).ok_or_else(missing)?,
            rtp_timestamp,
            milliseconds,
            rotation,
        )
        .map_err(Self::raw_video_error)?;
        self.retain_raw_video_frame(frame)
    }
    /// SDK copy: metadata is independent; immutable buffer ownership is shared.
    pub fn copy_native_video_frame(
        &mut self,
        frame: SessionRawVideoFrameId,
    ) -> Result<SessionRawVideoFrameId, PeerError> {
        let frame = self
            .raw_video_frames
            .get(&frame)
            .ok_or_else(missing)?
            .clone();
        self.retain_raw_video_frame(frame)
    }
    pub fn native_video_frame_metadata(
        &mut self,
        frame: SessionRawVideoFrameId,
    ) -> Result<crate::RawVideoFrameMetadata, PeerError> {
        Ok(self
            .raw_video_frames
            .get(&frame)
            .ok_or_else(missing)?
            .metadata())
    }
    pub fn set_native_video_frame_metadata(
        &mut self,
        frame: SessionRawVideoFrameId,
        value: &crate::RawVideoFrameMetadata,
    ) -> Result<(), PeerError> {
        self.raw_video_frames
            .get_mut(&frame)
            .ok_or_else(missing)?
            .set_metadata(value)
            .map_err(Self::raw_video_error)
    }
    pub fn native_video_frame_buffer(
        &mut self,
        frame: SessionRawVideoFrameId,
    ) -> Result<SessionRawVideoBufferId, PeerError> {
        let buffer = self
            .raw_video_frames
            .get(&frame)
            .ok_or_else(missing)?
            .buffer();
        self.retain_raw_video_buffer(buffer)
    }
    pub fn set_native_video_frame_buffer(
        &mut self,
        frame: SessionRawVideoFrameId,
        buffer: SessionRawVideoBufferId,
    ) -> Result<(), PeerError> {
        self.raw_video_frames
            .get_mut(&frame)
            .ok_or_else(missing)?
            .set_buffer(self.raw_video_buffers.get(&buffer).ok_or_else(missing)?)
            .map_err(Self::raw_video_error)
    }
    pub fn native_video_frame_to_legacy(
        &mut self,
        frame: SessionRawVideoFrameId,
    ) -> Result<crate::VideoFrame, PeerError> {
        self.raw_video_frames
            .get(&frame)
            .ok_or_else(missing)?
            .to_legacy()
            .map_err(Self::raw_video_error)
    }
    pub fn native_video_frame_is_texture(
        &mut self,
        frame: SessionRawVideoFrameId,
    ) -> Result<bool, PeerError> {
        Ok(self
            .raw_video_frames
            .get(&frame)
            .ok_or_else(missing)?
            .is_texture())
    }
    pub fn native_video_frame_size(
        &mut self,
        frame: SessionRawVideoFrameId,
    ) -> Result<u32, PeerError> {
        Ok(self
            .raw_video_frames
            .get(&frame)
            .ok_or_else(missing)?
            .size())
    }
    pub fn native_video_frame_render_time_ms(
        &mut self,
        frame: SessionRawVideoFrameId,
    ) -> Result<i64, PeerError> {
        Ok(self
            .raw_video_frames
            .get(&frame)
            .ok_or_else(missing)?
            .render_time_ms())
    }
    pub fn native_video_frame_update_rect(
        &mut self,
        frame: SessionRawVideoFrameId,
    ) -> Result<crate::VideoUpdateRect, PeerError> {
        Ok(self
            .raw_video_frames
            .get(&frame)
            .ok_or_else(missing)?
            .update_rect())
    }
    pub fn release_native_video_frame(&mut self, frame: SessionRawVideoFrameId) {
        self.raw_video_frames.remove(&frame);
    }
    pub fn alias_native_video_buffer(
        &mut self,
        buffer: SessionRawVideoBufferId,
    ) -> Result<SessionRawVideoBufferId, PeerError> {
        let buffer = self
            .raw_video_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .clone();
        self.retain_raw_video_buffer(buffer)
    }
    pub fn native_video_buffer_info(
        &mut self,
        buffer: SessionRawVideoBufferId,
    ) -> Result<crate::RawVideoBufferInfo, PeerError> {
        Ok(self
            .raw_video_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .info())
    }
    pub fn native_video_buffer_storage_bytes(
        &mut self,
        buffer: SessionRawVideoBufferId,
    ) -> Result<Vec<u8>, PeerError> {
        Ok(self
            .raw_video_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .storage_representation_bytes())
    }
    pub fn native_video_buffer_plane(
        &mut self,
        buffer: SessionRawVideoBufferId,
        plane: crate::RawVideoPlaneKind,
    ) -> Result<crate::RawVideoPlane, PeerError> {
        self.raw_video_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .plane(plane)
            .map_err(Self::raw_video_error)
    }
    pub fn native_video_buffer_to_i420(
        &mut self,
        buffer: SessionRawVideoBufferId,
    ) -> Result<SessionRawVideoBufferId, PeerError> {
        let buffer = self
            .raw_video_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .to_i420()
            .map_err(Self::raw_video_error)?;
        self.retain_raw_video_buffer(buffer)
    }
    pub fn native_video_buffer_get_i420(
        &mut self,
        buffer: SessionRawVideoBufferId,
    ) -> Result<Option<SessionRawVideoBufferId>, PeerError> {
        let buffer = self
            .raw_video_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .get_i420();
        buffer.map(|b| self.retain_raw_video_buffer(b)).transpose()
    }
    pub fn crop_and_scale_native_video_buffer(
        &mut self,
        buffer: SessionRawVideoBufferId,
        crop: crate::VideoUpdateRect,
        width: i32,
        height: i32,
    ) -> Result<SessionRawVideoBufferId, PeerError> {
        let buffer = self
            .raw_video_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .crop_and_scale(crop, width, height)
            .map_err(Self::raw_video_error)?;
        self.retain_raw_video_buffer(buffer)
    }
    pub fn scale_native_video_buffer(
        &mut self,
        buffer: SessionRawVideoBufferId,
        width: i32,
        height: i32,
    ) -> Result<SessionRawVideoBufferId, PeerError> {
        let buffer = self
            .raw_video_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .scale(width, height)
            .map_err(Self::raw_video_error)?;
        self.retain_raw_video_buffer(buffer)
    }
    pub fn mapped_native_video_buffer(
        &mut self,
        buffer: SessionRawVideoBufferId,
        types: &[crate::RawVideoBufferType],
    ) -> Result<Option<SessionRawVideoBufferId>, PeerError> {
        let buffer = self
            .raw_video_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .mapped_buffer(types)
            .map_err(Self::raw_video_error)?;
        buffer.map(|b| self.retain_raw_video_buffer(b)).transpose()
    }
    pub fn prepare_native_video_buffer(
        &mut self,
        buffer: SessionRawVideoBufferId,
        width: usize,
        height: usize,
        identifier: usize,
    ) -> Result<SessionVideoBufferPreparationId, PeerError> {
        let request = self
            .raw_video_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .prepare_mapped_buffer(width, height, identifier);
        let id = SessionVideoBufferPreparationId(next_id()?);
        self.video_buffer_preparations.insert(id, request);
        Ok(id)
    }
    pub fn try_native_video_buffer_prepared(
        &mut self,
        request: SessionVideoBufferPreparationId,
    ) -> Result<Option<usize>, PeerError> {
        Ok(self
            .video_buffer_preparations
            .get(&request)
            .ok_or_else(missing)?
            .try_next_prepared())
    }
    pub fn close_native_video_buffer_preparation(
        &mut self,
        request: SessionVideoBufferPreparationId,
    ) -> Result<(), PeerError> {
        self.video_buffer_preparations
            .get(&request)
            .ok_or_else(missing)?
            .close();
        Ok(())
    }
    pub fn release_native_video_buffer_preparation(
        &mut self,
        request: SessionVideoBufferPreparationId,
    ) {
        self.video_buffer_preparations.remove(&request);
    }
    pub fn release_native_video_buffer(&mut self, buffer: SessionRawVideoBufferId) {
        self.raw_video_buffers.remove(&buffer);
    }
    pub fn create_writable_i420_buffer(
        &mut self,
        width: i32,
        height: i32,
    ) -> Result<SessionWritableI420BufferId, PeerError> {
        self.check_open()?;
        self.retain_writable_i420(
            crate::WritableI420Buffer::new(width, height).map_err(Self::raw_video_error)?,
        )
    }
    pub fn create_writable_i420_buffer_with_strides(
        &mut self,
        width: i32,
        height: i32,
        y: i32,
        u: i32,
        v: i32,
    ) -> Result<SessionWritableI420BufferId, PeerError> {
        self.check_open()?;
        self.retain_writable_i420(
            crate::WritableI420Buffer::with_strides(width, height, y, u, v)
                .map_err(Self::raw_video_error)?,
        )
    }
    pub fn copy_writable_i420_buffer_from_planes(
        &mut self,
        width: i32,
        height: i32,
        y: &crate::VideoPlane,
        u: &crate::VideoPlane,
        v: &crate::VideoPlane,
    ) -> Result<SessionWritableI420BufferId, PeerError> {
        self.check_open()?;
        let buffer = crate::WritableI420Buffer::copy_from_planes(width, height, y, u, v)
            .map_err(Self::raw_video_error)?;
        self.retain_writable_i420(buffer)
    }
    pub fn copy_writable_i420_buffer(
        &mut self,
        buffer: SessionRawVideoBufferId,
    ) -> Result<SessionWritableI420BufferId, PeerError> {
        let buffer = crate::WritableI420Buffer::copy_from(
            self.raw_video_buffers.get(&buffer).ok_or_else(missing)?,
        )
        .map_err(Self::raw_video_error)?;
        self.retain_writable_i420(buffer)
    }
    pub fn rotate_writable_i420_buffer(
        &mut self,
        buffer: SessionRawVideoBufferId,
        rotation: crate::VideoRotation,
    ) -> Result<SessionWritableI420BufferId, PeerError> {
        let buffer = crate::WritableI420Buffer::rotated(
            self.raw_video_buffers.get(&buffer).ok_or_else(missing)?,
            rotation,
        )
        .map_err(Self::raw_video_error)?;
        self.retain_writable_i420(buffer)
    }
    pub fn writable_i420_buffer_info(
        &mut self,
        buffer: SessionWritableI420BufferId,
    ) -> Result<crate::RawVideoBufferInfo, PeerError> {
        Ok(self
            .writable_i420_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .info())
    }
    pub fn writable_i420_buffer_initialized(
        &mut self,
        buffer: SessionWritableI420BufferId,
    ) -> Result<bool, PeerError> {
        Ok(self
            .writable_i420_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .is_initialized())
    }
    pub fn writable_i420_buffer_plane(
        &mut self,
        buffer: SessionWritableI420BufferId,
        plane: crate::RawVideoPlaneKind,
    ) -> Result<crate::RawVideoPlane, PeerError> {
        self.writable_i420_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .plane(plane)
            .map_err(Self::raw_video_error)
    }
    pub fn write_i420_buffer_plane(
        &mut self,
        buffer: SessionWritableI420BufferId,
        plane: crate::RawVideoPlaneKind,
        data: &[u8],
        stride: i32,
    ) -> Result<(), PeerError> {
        self.writable_i420_buffers
            .get_mut(&buffer)
            .ok_or_else(missing)?
            .write_plane(plane, data, stride)
            .map_err(Self::raw_video_error)
    }
    pub fn initialize_i420_buffer(
        &mut self,
        buffer: SessionWritableI420BufferId,
        black: bool,
    ) -> Result<(), PeerError> {
        let buffer = self
            .writable_i420_buffers
            .get_mut(&buffer)
            .ok_or_else(missing)?;
        if black {
            buffer.set_black();
        } else {
            buffer.initialize_data();
        }
        Ok(())
    }
    pub fn crop_and_scale_i420_buffer_from(
        &mut self,
        target: SessionWritableI420BufferId,
        source: SessionRawVideoBufferId,
        crop: crate::VideoUpdateRect,
    ) -> Result<(), PeerError> {
        self.writable_i420_buffers
            .get_mut(&target)
            .ok_or_else(missing)?
            .crop_and_scale_from(
                self.raw_video_buffers.get(&source).ok_or_else(missing)?,
                crop,
            )
            .map_err(Self::raw_video_error)
    }
    pub fn scale_i420_buffer_from(
        &mut self,
        target: SessionWritableI420BufferId,
        source: SessionRawVideoBufferId,
        center_crop: bool,
    ) -> Result<(), PeerError> {
        let source = self.raw_video_buffers.get(&source).ok_or_else(missing)?;
        let target = self
            .writable_i420_buffers
            .get_mut(&target)
            .ok_or_else(missing)?;
        if center_crop {
            target.center_crop_and_scale_from(source)
        } else {
            target.scale_from(source)
        }
        .map_err(Self::raw_video_error)
    }
    pub fn freeze_i420_buffer(
        &mut self,
        buffer: SessionWritableI420BufferId,
    ) -> Result<SessionRawVideoBufferId, PeerError> {
        if !self
            .writable_i420_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .is_initialized()
        {
            return Err(Self::raw_video_error(crate::RawVideoError::Uninitialized));
        }
        let id = SessionRawVideoBufferId(next_id()?);
        let buffer = self
            .writable_i420_buffers
            .remove(&buffer)
            .expect("validated exclusive buffer")
            .freeze()
            .map_err(Self::raw_video_error)?;
        self.raw_video_buffers.insert(id, buffer);
        Ok(id)
    }
    pub fn release_writable_i420_buffer(&mut self, buffer: SessionWritableI420BufferId) {
        self.writable_i420_buffers.remove(&buffer);
    }
    pub fn push_native_video_frame(
        &mut self,
        source: SessionSourceId,
        frame: SessionRawVideoFrameId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let Some(Source::RawVideo(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        source
            .push_native_frame(self.raw_video_frames.get(&frame).ok_or_else(missing)?)
            .map_err(codec_error)
    }

    /// Submit a raw frame with capture time in the native clock's timeline.
    /// The engine retains its own frame-dropping and RTP timestamp semantics.
    pub fn push_raw_video(
        &mut self,
        source: SessionSourceId,
        frame: &crate::VideoFrame,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let Some(Source::RawVideo(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        source.push_frame(frame).map_err(codec_error)
    }

    pub fn create_opus_source(&mut self, channels: u8) -> Result<SessionSourceId, PeerError> {
        self.check_open()?;
        let id = SessionSourceId(next_id()?);
        let source = self.factory.create_encoded_audio_source(channels)?;
        self.sources.insert(id, Source::Opus(source));
        Ok(id)
    }
    pub fn create_h264_source_with_profile(
        &mut self,
        profile: crate::H264SourceProfile,
    ) -> Result<SessionSourceId, PeerError> {
        self.check_open()?;
        let source = self
            .input
            .as_ref()
            .ok_or_else(missing)?
            .create_profile_source_with_readiness(
                &self.factory,
                profile,
                Some(self.readiness.clone()),
            )?;
        let id = SessionSourceId(next_id()?);
        self.sources.insert(id, Source::Video(source));
        Ok(id)
    }
    pub fn create_video_source(&mut self) -> Result<SessionSourceId, PeerError> {
        self.check_open()?;
        let id = SessionSourceId(next_id()?);
        let source = self
            .input
            .as_ref()
            .ok_or_else(|| {
                error(
                    PeerErrorKind::InvalidState,
                    "session has no encoded video format",
                )
            })?
            .create_source_with_readiness(&self.factory, Some(self.readiness.clone()))?;
        self.sources.insert(id, Source::Video(source));
        Ok(id)
    }

    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_playout_initialized(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.playout_initialized())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_recording_initialized(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.recording_initialized())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_playing(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.playing())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_recording(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.recording())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_speaker_initialized(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.speaker_initialized())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_microphone_initialized(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.microphone_initialized())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_playout_available(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.playout_available()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_recording_available(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.recording_available()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_speaker_volume_available(
        &mut self,
    ) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.speaker_volume_available()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_set_speaker_volume(
        &mut self,
        value: u32,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.set_speaker_volume(value)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_speaker_volume(&mut self) -> Result<u32, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.speaker_volume()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_max_speaker_volume(&mut self) -> Result<u32, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.max_speaker_volume()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_min_speaker_volume(&mut self) -> Result<u32, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.min_speaker_volume()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_microphone_volume_available(
        &mut self,
    ) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.microphone_volume_available()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_set_microphone_volume(
        &mut self,
        value: u32,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.set_microphone_volume(value)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_microphone_volume(&mut self) -> Result<u32, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.microphone_volume()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_max_microphone_volume(&mut self) -> Result<u32, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.max_microphone_volume()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_min_microphone_volume(&mut self) -> Result<u32, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.min_microphone_volume()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_speaker_mute_available(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.speaker_mute_available()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_set_speaker_mute(
        &mut self,
        value: bool,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.set_speaker_mute(value)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_speaker_mute(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.speaker_mute()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_microphone_mute_available(
        &mut self,
    ) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.microphone_mute_available()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_set_microphone_mute(
        &mut self,
        value: bool,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.set_microphone_mute(value)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_microphone_mute(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.microphone_mute()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_stereo_playout_available(
        &mut self,
    ) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.stereo_playout_available()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_set_stereo_playout(
        &mut self,
        value: bool,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.set_stereo_playout(value)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_stereo_playout(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.stereo_playout()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_stereo_recording_available(
        &mut self,
    ) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.stereo_recording_available()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_set_stereo_recording(
        &mut self,
        value: bool,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.set_stereo_recording(value)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_stereo_recording(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.stereo_recording()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_built_in_aec_available(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.built_in_aec_available())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_built_in_agc_available(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.built_in_agc_available())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_built_in_ns_available(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.built_in_ns_available())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_enable_built_in_aec(
        &mut self,
        value: bool,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.enable_built_in_aec(value)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_enable_built_in_agc(
        &mut self,
        value: bool,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.enable_built_in_agc(value)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_enable_built_in_ns(
        &mut self,
        value: bool,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.enable_built_in_ns(value)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_platform_voice_processing_path_available(
        &mut self,
    ) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.platform_voice_processing_path_available())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_enable_platform_voice_processing_path(
        &mut self,
        value: bool,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.enable_platform_voice_processing_path(value)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_set_playout_device(
        &mut self,
        value: u16,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.set_playout_device(value)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_set_recording_device(
        &mut self,
        value: u16,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.set_recording_device(value)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_playout_device_count(&mut self) -> Result<u16, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.playout_device_count()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_recording_device_count(&mut self) -> Result<u16, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.recording_device_count()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_playout_underrun_count(&mut self) -> Result<i32, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.playout_underrun_count())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_current_playout_device(&mut self) -> Result<i32, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.current_playout_device())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_current_recording_device(
        &mut self,
    ) -> Result<i32, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.current_recording_device())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_playout_delay_ms(&mut self) -> Result<u16, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.playout_delay_ms()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_active_audio_layer(
        &mut self,
    ) -> Result<crate::AudioDeviceLayer, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        device.active_audio_layer()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_initialized(&mut self) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.initialized())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_stop_on_mute_mode_enabled(
        &mut self,
    ) -> Result<bool, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        let mut device = self.factory.audio_device_module()?;
        Ok(device.stop_on_mute_mode_enabled())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_statistics(
        &mut self,
    ) -> Result<Option<crate::AudioDeviceStatistics>, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        Ok(self.factory.audio_device_module()?.statistics())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_platform_processing_state(
        &mut self,
    ) -> Result<crate::PlatformAudioProcessingState, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        Ok(self
            .factory
            .audio_device_module()?
            .platform_processing_state())
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_name(
        &mut self,
        recording: bool,
        index: u16,
    ) -> Result<crate::AudioDeviceName, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        self.factory
            .audio_device_module()?
            .device_name(recording, index)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn register_audio_device_observer(
        &mut self,
        observer: crate::AudioDeviceObserver,
    ) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        self.factory
            .audio_device_module()?
            .register_observer(observer)
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn unregister_audio_device_observer(&mut self) -> Result<(), crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        self.factory.audio_device_module()?.unregister_observer()
    }
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_platform_processing_topology(
        &mut self,
    ) -> Result<crate::PlatformAudioProcessingTopology, crate::AudioDeviceError> {
        if self.closed {
            return Err(crate::AudioDeviceError::SessionClosed);
        }
        Ok(self
            .factory
            .audio_device_module()?
            .platform_processing_topology())
    }
    pub fn audio_mixer_failure_observer(
        &mut self,
    ) -> Result<Option<crate::AudioMixerFailureObserver>, PeerError> {
        self.check_open()?;
        Ok(self.factory.audio_mixer_failure_observer())
    }
    pub fn audio_encoder_failure_observer(
        &mut self,
    ) -> Result<crate::AudioEncoderSdkFailureObserver, PeerError> {
        self.check_open()?;
        Ok(self.factory.audio_encoder_failure_observer())
    }

    /// Factory-wide diagnostic state, not a claim that hardware is running.
    pub fn audio_processor_config(&mut self) -> Result<crate::AudioProcessorConfig, PeerError> {
        self.check_open()?;
        self.factory
            .audio_processor_config()
            .map_err(|e| error(PeerErrorKind::AudioProcessingAdmission, &e.to_string()))
    }
    pub fn apply_audio_processor_config(
        &mut self,
        config: crate::AudioProcessorConfig,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.factory
            .apply_audio_processor_config(config)
            .map_err(|e| error(PeerErrorKind::AudioProcessingAdmission, &e.to_string()))
    }
    pub fn process_reverse_audio(
        &mut self,
        format: crate::AudioProcessingStream,
        samples: &mut [i16],
    ) -> Result<crate::AudioProcessingStatus, PeerError> {
        self.check_open()?;
        self.factory
            .process_reverse_audio(format, samples)
            .map_err(|e| error(PeerErrorKind::AudioProcessingAdmission, &e.to_string()))
    }
    pub fn set_audio_processing_delay(
        &mut self,
        milliseconds: i32,
    ) -> Result<crate::AudioProcessingStatus, PeerError> {
        self.check_open()?;
        self.factory
            .set_audio_processing_delay(milliseconds)
            .map_err(|e| error(PeerErrorKind::AudioProcessingAdmission, &e.to_string()))
    }
    pub fn audio_processing_state(&mut self) -> Result<crate::AudioProcessingState, PeerError> {
        self.check_open()?;
        self.factory
            .try_audio_processing_state()
            .map_err(|e| error(PeerErrorKind::AudioProcessingAdmission, &e.to_string()))
    }

    #[cfg(feature = "native")]
    pub fn audio_devices(&mut self, recording: bool) -> Result<Vec<crate::AudioDevice>, PeerError> {
        self.check_open()?;
        self.factory.audio_devices(recording)
    }
    #[cfg(feature = "native")]
    pub fn select_audio_device(&mut self, recording: bool, index: u16) -> Result<(), PeerError> {
        self.check_open()?;
        self.factory.select_audio_device(recording, index)
    }

    #[cfg(feature = "native")]
    pub fn set_native_audio_enabled(
        &mut self,
        peer: SessionPeerId,
        recording: bool,
        enabled: bool,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .set_native_audio_enabled(recording, enabled)
    }

    pub fn create_pcm_source(&mut self) -> Result<SessionSourceId, PeerError> {
        self.check_open()?;
        let id = SessionSourceId(next_id()?);
        let source = self.factory.create_audio_source()?;
        self.sources.insert(id, Source::Pcm(source));
        Ok(id)
    }

    pub fn push_pcm(
        &mut self,
        source: SessionSourceId,
        frame: &crate::AudioPcmFrame,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let Some(Source::Pcm(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        source.push_frame(frame).map_err(|e| {
            let kind = match e {
                crate::AudioFrameError::ProcessingFailed(_) => PeerErrorKind::Internal,
                crate::AudioFrameError::UnsupportedProcessing => {
                    PeerErrorKind::UnsupportedOperation
                }
                crate::AudioFrameError::ReentrantCall => PeerErrorKind::InvalidState,
                crate::AudioFrameError::Released => PeerErrorKind::Closed,
                _ => PeerErrorKind::InvalidParameter,
            };
            error(kind, &e.to_string())
        })
    }

    /// Read one producer's native system-clock capture timeline. This does not
    /// claim that the current enqueue instant was the actual capture instant.
    pub fn opus_capture_time_now(
        &mut self,
        source: SessionSourceId,
    ) -> Result<std::time::Duration, PeerError> {
        self.check_open()?;
        let Some(Source::Opus(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        source
            .capture_time_now()
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }

    /// Submit actual capture timing mapped into opus_capture_time_now's domain.
    /// Native packetization/negotiated extension serialization owns wire timing.
    pub fn push_opus_at(
        &mut self,
        source: SessionSourceId,
        frame: &crate::OpusInputFrame,
        capture_time: std::time::Duration,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let Some(Source::Opus(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        source
            .push_opus_at(frame, capture_time)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }

    /// Submit mapped capture timing with independent producer RFC6464 metadata.
    pub fn push_opus_at_with_audio_level(
        &mut self,
        source: SessionSourceId,
        frame: &crate::OpusInputFrame,
        capture_time: std::time::Duration,
        level: crate::OpusAudioLevel,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let Some(Source::Opus(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        source
            .push_opus_at_with_audio_level(frame, capture_time, level)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }

    pub fn push_opus(
        &mut self,
        source: SessionSourceId,
        frame: &crate::OpusInputFrame,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let Some(Source::Opus(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        source
            .push_opus(frame)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    /// Submit producer RFC6464 metadata without exposing sequence-bound handles.
    pub fn push_opus_with_audio_level(
        &mut self,
        source: SessionSourceId,
        frame: &crate::OpusInputFrame,
        level: crate::OpusAudioLevel,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let Some(Source::Opus(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        source
            .push_opus_with_audio_level(frame, level)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn push_video(
        &mut self,
        source: SessionSourceId,
        frame: EncodedVideoAccessUnit,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let Some(Source::Video(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        source.push_encoded(frame).map_err(codec_error)
    }
    /// Atomically admit a bounded three-unit capture batch, not atomic wire publication.
    pub fn push_video_simulcast(
        &mut self,
        source: SessionSourceId,
        frames: [EncodedVideoAccessUnit; 3],
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let Some(Source::Video(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        source.push_simulcast(frames).map_err(codec_error)
    }
    pub fn video_encoding_keyframe_requests(
        &mut self,
        source: SessionSourceId,
    ) -> Result<[bool; 3], PeerError> {
        let Some(Source::Video(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        Ok(source.take_encoding_keyframe_requests())
    }

    pub fn video_feedback(
        &mut self,
        source: SessionSourceId,
    ) -> Result<(bool, Option<VideoRateControl>), PeerError> {
        let Some(Source::Video(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        Ok((source.take_keyframe_request(), source.latest_rate_control()))
    }

    /// Retrieve the latest source-keyed asynchronous video adapter failure.
    pub fn take_video_encoder_error(
        &mut self,
        source: SessionSourceId,
    ) -> Result<Option<CodecError>, PeerError> {
        let Some(Source::Video(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        Ok(source.take_encoder_error())
    }

    /// Construct a baseline owning track without implicitly adding a transceiver.
    pub fn create_opus_track(
        &mut self,
        peer: SessionPeerId,
        source: SessionSourceId,
        track_id: &str,
    ) -> Result<SessionMediaTrackId, PeerError> {
        self.check_open()?;
        self.peers.get(&peer).ok_or_else(missing)?;
        let Some(Source::Opus(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        let track = self.factory.create_encoded_audio_track(track_id, source)?;
        self.insert_media_track(peer, track.as_media_track())
    }
    pub fn create_video_track(
        &mut self,
        peer: SessionPeerId,
        source: SessionSourceId,
        track_id: &str,
    ) -> Result<SessionMediaTrackId, PeerError> {
        self.check_open()?;
        self.peers.get(&peer).ok_or_else(missing)?;
        let Some(Source::Video(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        let track = source.create_track(&self.factory, track_id)?;
        self.insert_media_track(peer, track.as_media_track())
    }
    pub fn publish_pcm(
        &mut self,
        peer: SessionPeerId,
        source: SessionSourceId,
        track_id: &str,
        direction: RtpTransceiverDirection,
        processing: Option<crate::AudioProcessingOptions>,
    ) -> Result<SessionTransceiverId, PeerError> {
        self.check_open()?;
        let target = &self.peers.get(&peer).ok_or_else(missing)?.value;
        if target.inner.closed.get() {
            return Err(error(PeerErrorKind::Closed, "peer is closed"));
        }
        let Some(Source::Pcm(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        let track = self.factory.create_audio_track(track_id, source)?;
        if let Some(options) = processing {
            track.set_audio_processing_options(options)?;
        }
        let value = target.add_audio_transceiver(&track, direction)?;
        self.insert_transceiver(peer, value)
    }

    /// Change the real retained local track's processing choices. Processing
    /// is shared by the voice engine, not isolated independently per track.
    pub fn set_audio_processing_options(
        &mut self,
        sender: SessionSenderId,
        options: crate::AudioProcessingOptions,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let sender = self.senders.get(&sender).ok_or_else(missing)?;
        let peer = self.peers.get(&sender.peer).ok_or_else(missing)?;
        let native_id = sender.value.resource_key();
        let track = peer
            .value
            .inner
            .audio_sender_tracks
            .borrow()
            .get(&native_id)
            .map(|lease| lease.track.clone())
            .ok_or_else(|| {
                error(
                    PeerErrorKind::UnsupportedOperation,
                    "sender has no local audio track",
                )
            })?;
        track.set_audio_processing_options(options)
    }

    #[cfg(feature = "native")]
    pub fn publish_microphone(
        &mut self,
        peer: SessionPeerId,
        track_id: &str,
        direction: RtpTransceiverDirection,
        processing: Option<crate::AudioProcessingOptions>,
    ) -> Result<SessionTransceiverId, PeerError> {
        self.check_open()?;
        let target = &self.peers.get(&peer).ok_or_else(missing)?.value;
        if target.inner.closed.get() {
            return Err(error(PeerErrorKind::Closed, "peer is closed"));
        }
        let track = self.factory.create_microphone_track(track_id)?;
        if let Some(options) = processing {
            track.set_audio_processing_options(options)?;
        }
        let value = target.add_audio_transceiver(&track, direction)?;
        self.insert_transceiver(peer, value)
    }

    pub fn publish_opus(
        &mut self,
        peer: SessionPeerId,
        source: SessionSourceId,
        track_id: &str,
        direction: RtpTransceiverDirection,
    ) -> Result<SessionTransceiverId, PeerError> {
        self.check_open()?;
        let Some(Source::Opus(source)) = self.sources.get(&source) else {
            return Err(missing());
        };
        let track = self.factory.create_encoded_audio_track(track_id, source)?;
        let value = self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .add_audio_transceiver(&track, direction)?;
        self.insert_transceiver(peer, value)
    }
    /// Add a transceiver with no sending source, using the complete SDK init.
    pub fn add_transceiver(
        &mut self,
        peer: SessionPeerId,
        kind: crate::MediaType,
        init: &crate::RtpTransceiverInit,
    ) -> Result<SessionTransceiverId, crate::RtpSenderOperationError> {
        self.check_open()?;
        let value = self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .add_transceiver(kind, init)?;
        Ok(self.insert_transceiver(peer, value)?)
    }

    /// Install an existing owning SDK track, including a receiver's track, into
    /// another transceiver in this session without rebuilding its source.
    /// The unresolved allocator-priority qualification is the same as for
    /// [`PeerConnection::add_media_transceiver_with_init`].
    pub fn add_media_transceiver_with_init(
        &mut self,
        peer: SessionPeerId,
        track: SessionMediaTrackId,
        init: &crate::RtpTransceiverInit,
    ) -> Result<SessionTransceiverId, crate::RtpSenderOperationError> {
        self.check_open()?;
        let track = &self.media_tracks.get(&track).ok_or_else(missing)?.value;
        let value = self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .add_media_transceiver_with_init(track, init)?;
        Ok(self.insert_transceiver(peer, value)?)
    }

    /// Publish audio using all native initialization fields in one operation.
    pub fn publish_audio_with_init(
        &mut self,
        peer: SessionPeerId,
        source: SessionSourceId,
        track_id: &str,
        init: &crate::RtpTransceiverInit,
    ) -> Result<SessionTransceiverId, crate::RtpSenderOperationError> {
        self.check_open()?;
        let source = self
            .sources
            .get(&source)
            .ok_or_else(missing)?
            .audio_source()?;
        let track = self.factory.create_audio_track(track_id, source)?;
        let value = self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .add_audio_transceiver_with_init(&track, init)?;
        Ok(self.insert_transceiver(peer, value)?)
    }

    pub fn publish_video_with_init(
        &mut self,
        peer: SessionPeerId,
        source: SessionSourceId,
        track_id: &str,
        init: &crate::RtpTransceiverInit,
    ) -> Result<SessionTransceiverId, crate::RtpSenderOperationError> {
        self.check_open()?;
        let track = self
            .sources
            .get(&source)
            .ok_or_else(missing)?
            .video_track(&self.factory, track_id)?;
        let value = self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .add_video_transceiver_with_init(&track, init)?;
        Ok(self.insert_transceiver(peer, value)?)
    }
    pub fn publish_video(
        &mut self,
        peer: SessionPeerId,
        source: SessionSourceId,
        track_id: &str,
        direction: RtpTransceiverDirection,
        rids: &[String],
    ) -> Result<SessionTransceiverId, PeerError> {
        self.check_open()?;
        let track = self
            .sources
            .get(&source)
            .ok_or_else(missing)?
            .video_track(&self.factory, track_id)?;
        let value = self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .add_video_transceiver_with_rids(&track, direction, rids)?;
        self.insert_transceiver(peer, value)
    }
    fn insert_sender(
        &mut self,
        peer: SessionPeerId,
        value: RtpSender,
    ) -> Result<SessionSenderId, PeerError> {
        if let Some((&id, _)) = self
            .senders
            .iter()
            .find(|(_, existing)| existing.peer == peer && existing.value.same_resource(&value))
        {
            return Ok(id);
        }
        let id = SessionSenderId(next_id()?);
        self.senders.insert(id, Resource { peer, value });
        Ok(id)
    }

    fn insert_receiver(
        &mut self,
        peer: SessionPeerId,
        value: RtpReceiver,
    ) -> Result<SessionReceiverId, PeerError> {
        if let Some((&id, _)) = self
            .receivers
            .iter()
            .find(|(_, existing)| existing.peer == peer && existing.value.same_resource(&value))
        {
            return Ok(id);
        }
        let id = SessionReceiverId(next_id()?);
        self.receivers.insert(id, Resource { peer, value });
        Ok(id)
    }
    fn insert_transceiver(
        &mut self,
        peer: SessionPeerId,
        value: RtpTransceiver,
    ) -> Result<SessionTransceiverId, PeerError> {
        let receiver = self.insert_receiver(peer, value.receiver())?;
        if let Some((&id, _)) = self.transceivers.iter().find(|(_, existing)| {
            existing.peer == peer && existing.value.value.same_resource(&value)
        }) {
            return Ok(id);
        }
        let sender = self.insert_sender(peer, value.sender())?;
        let id = SessionTransceiverId(next_id()?);
        self.transceivers.insert(
            id,
            Resource {
                peer,
                value: Transceiver {
                    value,
                    sender,
                    receiver,
                },
            },
        );
        Ok(id)
    }
    pub fn sender(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<SessionSenderId, PeerError> {
        Ok(self
            .transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .sender)
    }
    pub fn receiver(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<SessionReceiverId, PeerError> {
        Ok(self
            .transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .receiver)
    }
    fn insert_media_track(
        &mut self,
        peer: SessionPeerId,
        value: crate::MediaTrack,
    ) -> Result<SessionMediaTrackId, PeerError> {
        if let Some((&id, _)) = self
            .media_tracks
            .iter()
            .find(|(_, r)| r.value.same_resource(&value))
        {
            return Ok(id);
        }
        let id = SessionMediaTrackId(next_id()?);
        self.media_tracks.insert(id, Resource { peer, value });
        Ok(id)
    }
    pub fn sender_track_reference(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<Option<SessionMediaTrackId>, PeerError> {
        let sender = self.senders.get(&sender).ok_or_else(missing)?;
        let peer = sender.peer;
        sender
            .value
            .track_reference()
            .map(|track| self.insert_media_track(peer, track))
            .transpose()
    }
    pub fn receiver_track_reference(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<SessionMediaTrackId>, PeerError> {
        let receiver = self.receivers.get(&receiver).ok_or_else(missing)?;
        let peer = receiver.peer;
        receiver
            .value
            .track_reference()
            .map(|track| self.insert_media_track(peer, track))
            .transpose()
    }
    /// Release this actor reference. Accepted native work retains its own lease.
    pub fn release_media_track_reference(
        &mut self,
        track: SessionMediaTrackId,
    ) -> Result<(), PeerError> {
        self.track_observers.remove(&track);
        self.media_tracks.remove(&track).ok_or_else(missing)?;
        Ok(())
    }
    pub fn media_track_id(&mut self, track: SessionMediaTrackId) -> Result<Vec<u8>, PeerError> {
        Ok(self
            .media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .id())
    }
    pub fn media_track_kind(
        &mut self,
        track: SessionMediaTrackId,
    ) -> Result<crate::MediaTrackKind, PeerError> {
        Ok(self
            .media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .kind())
    }
    pub fn media_track_state(
        &mut self,
        track: SessionMediaTrackId,
    ) -> Result<crate::MediaTrackState, PeerError> {
        Ok(self
            .media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .state())
    }
    pub fn media_track_enabled(&mut self, track: SessionMediaTrackId) -> Result<bool, PeerError> {
        Ok(self
            .media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .enabled())
    }
    pub fn set_media_track_enabled(
        &mut self,
        track: SessionMediaTrackId,
        enabled: bool,
    ) -> Result<bool, PeerError> {
        Ok(self
            .media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .set_enabled(enabled))
    }
    /// Register the actual SDK sender slot with explicit bounded polling.
    pub fn register_sender_packet_observer(
        &mut self,
        sender: SessionSenderId,
        capacity: usize,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let resource = self.senders.get(&sender).ok_or_else(missing)?;
        let observer = self
            .sender_observers
            .entry(sender)
            .or_insert_with(|| Resource {
                peer: resource.peer,
                value: resource.value.packet_observer(),
            });
        observer
            .value
            .register_observer(capacity)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn unregister_sender_packet_observer(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<(), PeerError> {
        self.senders.get(&sender).ok_or_else(missing)?;
        if let Some(observer) = self.sender_observers.get(&sender) {
            observer.value.unregister_observer();
        }
        Ok(())
    }
    pub fn take_sender_packet_event(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<Option<crate::RtpPacketEvent>, PeerError> {
        self.senders.get(&sender).ok_or_else(missing)?;
        Ok(self
            .sender_observers
            .get(&sender)
            .and_then(|observer| observer.value.take_event()))
    }
    pub fn take_sender_dropped_packet_events(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<u64, PeerError> {
        self.senders.get(&sender).ok_or_else(missing)?;
        Ok(self
            .sender_observers
            .get(&sender)
            .map_or(0, |observer| observer.value.take_dropped_events()))
    }

    /// Register the actual SDK receiver slot with explicit bounded polling.
    pub fn register_receiver_packet_observer(
        &mut self,
        receiver: SessionReceiverId,
        capacity: usize,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let resource = self.receivers.get(&receiver).ok_or_else(missing)?;
        let observer = self
            .receiver_observers
            .entry(receiver)
            .or_insert_with(|| Resource {
                peer: resource.peer,
                value: resource.value.packet_observer(),
            });
        observer
            .value
            .register_observer(capacity)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn unregister_receiver_packet_observer(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<(), PeerError> {
        self.receivers.get(&receiver).ok_or_else(missing)?;
        if let Some(observer) = self.receiver_observers.get(&receiver) {
            observer.value.unregister_observer();
        }
        Ok(())
    }
    /// Actual SDK callback record for this slot. FirstPacketReceived may fan
    /// out to every same-kind receiver; it is not receiver-SSRC delivery proof.
    pub fn take_receiver_packet_event(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<crate::RtpPacketEvent>, PeerError> {
        self.receivers.get(&receiver).ok_or_else(missing)?;
        Ok(self
            .receiver_observers
            .get(&receiver)
            .and_then(|observer| observer.value.take_event()))
    }
    pub fn take_receiver_dropped_packet_events(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<u64, PeerError> {
        self.receivers.get(&receiver).ok_or_else(missing)?;
        Ok(self
            .receiver_observers
            .get(&receiver)
            .map_or(0, |observer| observer.value.take_dropped_events()))
    }

    /// Preserve native creation failures and renegotiation side effects; no
    /// local handle is imported or exported, even after actor migration.
    pub fn create_sframe_encrypter(
        &mut self,
        sender: SessionSenderId,
        init: &crate::SframeEncrypterInit,
    ) -> Result<SessionSframeEncrypterId, crate::SframeCreateError> {
        self.check_open()?;
        let sender = self.senders.get(&sender).ok_or_else(missing)?;
        let id = SessionSframeEncrypterId(next_id()?);
        let value = sender.value.create_sframe_encrypter(init)?;
        self.sframe_encrypters.insert(
            id,
            Resource {
                peer: sender.peer,
                value,
            },
        );
        Ok(id)
    }
    pub fn create_sframe_decrypter(
        &mut self,
        receiver: SessionReceiverId,
        suite: crate::SframeCipherSuite,
    ) -> Result<SessionSframeDecrypterId, crate::SframeCreateError> {
        self.check_open()?;
        let receiver = self.receivers.get(&receiver).ok_or_else(missing)?;
        let id = SessionSframeDecrypterId(next_id()?);
        let value = receiver.value.create_sframe_decrypter(suite)?;
        self.sframe_decrypters.insert(
            id,
            Resource {
                peer: receiver.peer,
                value,
            },
        );
        Ok(id)
    }
    pub fn sframe_set_encryption_key(
        &mut self,
        value: SessionSframeEncrypterId,
        id: u64,
        key: &[u8],
    ) -> Result<crate::RtcError, PeerError> {
        self.check_open()?;
        Ok(self
            .sframe_encrypters
            .get(&value)
            .ok_or_else(missing)?
            .value
            .set_encryption_key(id, key))
    }
    pub fn sframe_add_decryption_key(
        &mut self,
        value: SessionSframeDecrypterId,
        id: u64,
        key: &[u8],
    ) -> Result<crate::RtcError, PeerError> {
        self.check_open()?;
        Ok(self
            .sframe_decrypters
            .get(&value)
            .ok_or_else(missing)?
            .value
            .add_decryption_key(id, key))
    }
    pub fn sframe_remove_decryption_key(
        &mut self,
        value: SessionSframeDecrypterId,
        id: u64,
    ) -> Result<crate::RtcError, PeerError> {
        self.check_open()?;
        Ok(self
            .sframe_decrypters
            .get(&value)
            .ok_or_else(missing)?
            .value
            .remove_decryption_key(id))
    }
    pub fn release_sframe_encrypter(
        &mut self,
        value: SessionSframeEncrypterId,
    ) -> Result<(), PeerError> {
        self.sframe_encrypters
            .remove(&value)
            .map(|_| ())
            .ok_or_else(missing)
    }
    pub fn release_sframe_decrypter(
        &mut self,
        value: SessionSframeDecrypterId,
    ) -> Result<(), PeerError> {
        self.sframe_decrypters
            .remove(&value)
            .map(|_| ())
            .ok_or_else(missing)
    }
    /// Construct inside the captive graph; no local native handle is imported.
    pub fn create_frame_encryptor(
        &mut self,
        provider: impl crate::FrameEncryptorProvider,
    ) -> Result<SessionFrameEncryptorId, PeerError> {
        self.check_open()?;
        let id = SessionFrameEncryptorId(next_id()?);
        self.frame_encryptors
            .insert(id, crate::FrameEncryptor::from_provider(provider));
        Ok(id)
    }
    pub fn create_frame_decryptor(
        &mut self,
        receiver: SessionReceiverId,
        provider: impl crate::FrameDecryptorProvider,
    ) -> Result<SessionFrameDecryptorId, PeerError> {
        self.check_open()?;
        let receiver = self.receivers.get(&receiver).ok_or_else(missing)?;
        let id = SessionFrameDecryptorId(next_id()?);
        let value = receiver.value.create_frame_decryptor(provider);
        self.frame_decryptors.insert(
            id,
            Resource {
                peer: receiver.peer,
                value,
            },
        );
        Ok(id)
    }
    pub fn set_sender_frame_encryptor(
        &mut self,
        sender: SessionSenderId,
        encryptor: Option<SessionFrameEncryptorId>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let value = encryptor
            .map(|id| self.frame_encryptors.get(&id).ok_or_else(missing))
            .transpose()?;
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .set_frame_encryptor(value);
        Ok(())
    }
    pub fn set_receiver_frame_decryptor(
        &mut self,
        receiver: SessionReceiverId,
        decryptor: Option<SessionFrameDecryptorId>,
    ) -> Result<(), crate::FrameDecryptorInstallError> {
        let binding = crate::FrameDecryptorInstallError::Binding;
        self.check_open().map_err(binding)?;
        let value = decryptor
            .map(|id| {
                self.frame_decryptors
                    .get(&id)
                    .map(|r| &r.value)
                    .ok_or_else(missing)
            })
            .transpose()
            .map_err(binding)?;
        self.receivers
            .get(&receiver)
            .ok_or_else(missing)
            .map_err(binding)?
            .value
            .set_frame_decryptor(value)
    }
    pub fn sender_frame_encryptor(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<Option<SessionFrameEncryptorId>, PeerError> {
        self.check_open()?;
        let Some(value) = self
            .senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .frame_encryptor()
        else {
            return Ok(None);
        };
        if let Some((&id, _)) = self
            .frame_encryptors
            .iter()
            .find(|(_, existing)| existing.same_resource(&value))
        {
            return Ok(Some(id));
        }
        let id = SessionFrameEncryptorId(next_id()?);
        self.frame_encryptors.insert(id, value);
        Ok(Some(id))
    }
    pub fn receiver_frame_decryptor(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<SessionFrameDecryptorId>, PeerError> {
        self.check_open()?;
        let receiver = self.receivers.get(&receiver).ok_or_else(missing)?;
        let Some(value) = receiver.value.frame_decryptor() else {
            return Ok(None);
        };
        if let Some((&id, _)) = self
            .frame_decryptors
            .iter()
            .find(|(_, existing)| existing.value.same_resource(&value))
        {
            return Ok(Some(id));
        }
        let id = SessionFrameDecryptorId(next_id()?);
        self.frame_decryptors.insert(
            id,
            Resource {
                peer: receiver.peer,
                value,
            },
        );
        Ok(Some(id))
    }
    pub fn frame_encryptor_failure(
        &mut self,
        encryptor: SessionFrameEncryptorId,
    ) -> Result<Option<crate::FrameCryptoFailure>, PeerError> {
        self.check_open()?;
        Ok(self
            .frame_encryptors
            .get(&encryptor)
            .ok_or_else(missing)?
            .failure())
    }
    pub fn frame_decryptor_failure(
        &mut self,
        decryptor: SessionFrameDecryptorId,
    ) -> Result<Option<crate::FrameCryptoFailure>, PeerError> {
        self.check_open()?;
        Ok(self
            .frame_decryptors
            .get(&decryptor)
            .ok_or_else(missing)?
            .value
            .failure())
    }
    pub fn frame_encryptor_max_ciphertext_byte_size(
        &mut self,
        encryptor: SessionFrameEncryptorId,
        media: crate::MediaType,
        frame_size: usize,
    ) -> Result<usize, PeerError> {
        self.check_open()?;
        Ok(self
            .frame_encryptors
            .get(&encryptor)
            .ok_or_else(missing)?
            .max_ciphertext_byte_size(media, frame_size))
    }
    pub fn frame_decryptor_max_plaintext_byte_size(
        &mut self,
        decryptor: SessionFrameDecryptorId,
        media: crate::MediaType,
        frame_size: usize,
    ) -> Result<usize, PeerError> {
        self.check_open()?;
        Ok(self
            .frame_decryptors
            .get(&decryptor)
            .ok_or_else(missing)?
            .value
            .max_plaintext_byte_size(media, frame_size))
    }
    pub fn encrypt_frame(
        &mut self,
        encryptor: SessionFrameEncryptorId,
        media: crate::MediaType,
        ssrc: u32,
        additional_data: &[u8],
        frame: &[u8],
        capacity: usize,
    ) -> Result<crate::EncryptedFrame, crate::FrameCryptoOperationError> {
        self.check_open()?;
        self.frame_encryptors
            .get(&encryptor)
            .ok_or_else(missing)?
            .encrypt(media, ssrc, additional_data, frame, capacity)
    }
    pub fn decrypt_frame(
        &mut self,
        decryptor: SessionFrameDecryptorId,
        media: crate::MediaType,
        csrcs: &[u32],
        additional_data: &[u8],
        frame: &[u8],
        capacity: usize,
    ) -> Result<crate::DecryptedFrame, crate::FrameCryptoOperationError> {
        self.check_open()?;
        self.frame_decryptors
            .get(&decryptor)
            .ok_or_else(missing)?
            .value
            .decrypt(media, csrcs, additional_data, frame, capacity)
    }
    /// Release this application identity. An installed SDK reference remains
    /// alive until replaced/cleared or its sender/receiver is destroyed.
    pub fn create_frame_transformer(
        &mut self,
        provider: Arc<dyn crate::FrameTransformerProvider>,
        maximum_in_flight: usize,
    ) -> Result<SessionFrameTransformerId, PeerError> {
        self.check_open()?;
        let id = SessionFrameTransformerId(next_id()?);
        self.frame_transformers.insert(
            id,
            crate::FrameTransformer::from_shared(provider, maximum_in_flight),
        );
        Ok(id)
    }
    pub fn alias_frame_transformer(
        &mut self,
        transformer: SessionFrameTransformerId,
    ) -> Result<SessionFrameTransformerId, PeerError> {
        let value = self
            .frame_transformers
            .get(&transformer)
            .ok_or_else(missing)?
            .clone();
        let id = SessionFrameTransformerId(next_id()?);
        self.frame_transformers.insert(id, value);
        Ok(id)
    }
    pub fn set_sender_frame_transformer(
        &mut self,
        sender: SessionSenderId,
        transformer: Option<SessionFrameTransformerId>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let value = transformer
            .map(|id| self.frame_transformers.get(&id).ok_or_else(missing))
            .transpose()?;
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .set_frame_transformer(value)
    }
    pub fn set_receiver_frame_transformer(
        &mut self,
        receiver: SessionReceiverId,
        transformer: Option<SessionFrameTransformerId>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let value = transformer
            .map(|id| self.frame_transformers.get(&id).ok_or_else(missing))
            .transpose()?;
        self.receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .set_frame_transformer(value)
    }
    pub fn frame_transformer_statistics(
        &mut self,
        transformer: SessionFrameTransformerId,
    ) -> Result<crate::FrameTransformerStatistics, PeerError> {
        Ok(self
            .frame_transformers
            .get(&transformer)
            .ok_or_else(missing)?
            .statistics())
    }
    /// Counter-only evidence can outlive IDs, native hosts and callback userdata.
    pub fn frame_transformer_failure_observer(
        &mut self,
        transformer: SessionFrameTransformerId,
    ) -> Result<crate::FrameTransformerFailureObserver, PeerError> {
        Ok(self
            .frame_transformers
            .get(&transformer)
            .ok_or_else(missing)?
            .failure_observer())
    }
    pub fn close_frame_transformer(
        &mut self,
        transformer: SessionFrameTransformerId,
    ) -> Result<(), PeerError> {
        self.frame_transformers
            .get(&transformer)
            .ok_or_else(missing)?
            .close();
        Ok(())
    }
    pub fn frame_transformer_closed(
        &mut self,
        transformer: SessionFrameTransformerId,
    ) -> Result<bool, PeerError> {
        Ok(self
            .frame_transformers
            .get(&transformer)
            .ok_or_else(missing)?
            .is_closed())
    }
    pub fn release_frame_transformer(&mut self, transformer: SessionFrameTransformerId) {
        self.frame_transformers.remove(&transformer);
    }

    pub fn release_frame_encryptor(
        &mut self,
        encryptor: SessionFrameEncryptorId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.frame_encryptors
            .remove(&encryptor)
            .ok_or_else(missing)?;
        Ok(())
    }
    pub fn release_frame_decryptor(
        &mut self,
        decryptor: SessionFrameDecryptorId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.frame_decryptors
            .remove(&decryptor)
            .ok_or_else(missing)?;
        Ok(())
    }

    /// Actual native GenerateKeyFrame, including audio unsupported results and
    /// the stock stopped/unconfigured video's successful no-op. Native errors
    /// are independent owning values, not a narrowed message/type pair.
    pub fn generate_sender_keyframe(
        &mut self,
        sender: SessionSenderId,
        rids: &[Vec<u8>],
    ) -> Result<(), crate::RtpSenderOperationError> {
        self.check_open()?;
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .generate_keyframe(rids)
    }

    /// Actual primary SDK SSRC; zero is the native not-yet-determined value.
    pub fn sender_ssrc(&mut self, sender: SessionSenderId) -> Result<u32, PeerError> {
        self.check_open()?;
        Ok(self.senders.get(&sender).ok_or_else(missing)?.value.ssrc())
    }

    /// SDK-generated sender identity, distinct from the actor resource ID.
    pub fn sender_native_id(&mut self, sender: SessionSenderId) -> Result<String, PeerError> {
        self.check_open()?;
        Ok(self.senders.get(&sender).ok_or_else(missing)?.value.id())
    }

    pub fn sender_media_type(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<crate::MediaType, PeerError> {
        self.check_open()?;
        Ok(self
            .senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .media_type())
    }

    /// A single native snapshot of stream IDs with their full byte domain.
    pub fn sender_stream_ids(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<Vec<Vec<u8>>, PeerError> {
        self.check_open()?;
        Ok(self
            .senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .stream_id_bytes())
    }

    /// UTF-8 convenience installation through SDK SetStreams. Native
    /// de-duplication applies. This void operation is not a negotiation ack.
    pub fn set_sender_streams(
        &mut self,
        sender: SessionSenderId,
        ids: &[String],
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .set_streams(ids);
        Ok(())
    }

    /// Initial encodings are independent complete SDK value copies, not a
    /// GetParameters transaction or a reliable seed for a reused sender.
    pub fn sender_initial_encodings(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<Vec<crate::RtpEncodingParameters>, PeerError> {
        self.check_open()?;
        Ok(self
            .senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .init_send_encodings())
    }

    /// Entire actual SDK snapshot; each successful SDK get starts a fresh
    /// transaction, invalidating older snapshots for a subsequent native set.
    pub fn get_sender_parameters(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<crate::RtpParameters, PeerError> {
        self.check_open()?;
        Ok(self
            .senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .get_parameters())
    }

    pub fn set_sender_parameters_value(
        &mut self,
        sender: SessionSenderId,
        parameters: &crate::RtpParameters,
    ) -> Result<crate::RtcError, PeerError> {
        self.check_open()?;
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .set_parameters_value(parameters)
    }
    pub fn set_sender_parameters_async(
        &mut self,
        sender: SessionSenderId,
        parameters: &crate::RtpParameters,
    ) -> Result<SessionSenderParametersOperationId, PeerError> {
        self.check_open()?;
        let value = self
            .senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .set_parameters_async(parameters)?;
        let id = SessionSenderParametersOperationId(next_id()?);
        self.sender_parameter_operations.insert(id, value);
        Ok(id)
    }
    pub fn sender_parameters_operation_ready(
        &mut self,
        operation: SessionSenderParametersOperationId,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        Ok(self
            .sender_parameter_operations
            .get(&operation)
            .ok_or_else(missing)?
            .is_ready())
    }
    pub fn take_sender_parameters_result(
        &mut self,
        operation: SessionSenderParametersOperationId,
    ) -> Result<Option<crate::RtcError>, PeerError> {
        self.check_open()?;
        Ok(self
            .sender_parameter_operations
            .get(&operation)
            .ok_or_else(missing)?
            .take_result())
    }
    pub fn release_sender_parameters_operation(
        &mut self,
        operation: SessionSenderParametersOperationId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.sender_parameter_operations
            .remove(&operation)
            .ok_or_else(missing)?;
        Ok(())
    }
    /// Native SSRC/CSRC observations copied through the receiver worker proxy.
    pub fn receiver_sources(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Vec<crate::RtpSource>, PeerError> {
        self.check_open()?;
        Ok(self
            .receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .get_sources())
    }

    /// Entire independently owned SDK receiver snapshot.
    pub fn get_receiver_parameters(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<crate::RtpParameters, PeerError> {
        self.check_open()?;
        Ok(self
            .receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .get_parameters())
    }

    /// Preserves the legacy SDK receiver setter's unsupported false result.
    pub fn set_receiver_parameters(
        &mut self,
        receiver: SessionReceiverId,
        parameters: &crate::RtpParameters,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        Ok(self
            .receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .set_parameters(parameters))
    }

    pub fn set_receiver_jitter_buffer_minimum_delay(
        &mut self,
        receiver: SessionReceiverId,
        seconds: Option<f64>,
    ) -> Result<(), crate::RtpReceiverOperationError> {
        self.check_open()?;
        self.receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .set_jitter_buffer_minimum_delay(seconds)
    }

    pub fn sender_parameters(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<RtpSenderParameters, PeerError> {
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .parameters()
    }
    pub fn set_sender_parameters(
        &mut self,
        sender: SessionSenderId,
        parameters: RtpSenderParameters,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .set_parameters(parameters)
    }
    pub fn header_extensions_to_negotiate(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<Vec<RtpHeaderExtensionCapability>, PeerError> {
        self.transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .header_extensions_to_negotiate()
    }

    /// Full owned SDK preferences without legacy format reconstruction.
    pub fn rtp_codec_preferences(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<Vec<crate::RtpCodecCapability>, PeerError> {
        self.transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .rtp_codec_preferences()
    }

    pub fn header_extension_capabilities_to_negotiate(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<Vec<crate::RtpHeaderExtensionCapabilityValue>, PeerError> {
        self.transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .header_extension_capabilities_to_negotiate()
    }

    pub fn negotiated_header_extension_capabilities(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<Vec<crate::RtpHeaderExtensionCapabilityValue>, PeerError> {
        self.transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .negotiated_header_extension_capabilities()
    }

    pub fn set_rtp_codec_preferences(
        &mut self,
        transceiver: SessionTransceiverId,
        codecs: &[crate::RtpCodecCapability],
    ) -> Result<(), crate::RtpCapabilityInstallError> {
        self.check_open()
            .map_err(crate::RtpCapabilityInstallError::Binding)?;
        self.transceivers
            .get(&transceiver)
            .ok_or_else(|| crate::RtpCapabilityInstallError::Binding(missing()))?
            .value
            .value
            .set_rtp_codec_preferences(codecs)
    }

    pub fn set_header_extension_capabilities_to_negotiate(
        &mut self,
        transceiver: SessionTransceiverId,
        extensions: &[crate::RtpHeaderExtensionCapabilityValue],
    ) -> Result<(), crate::RtpCapabilityInstallError> {
        self.check_open()
            .map_err(crate::RtpCapabilityInstallError::Binding)?;
        self.transceivers
            .get(&transceiver)
            .ok_or_else(|| crate::RtpCapabilityInstallError::Binding(missing()))?
            .value
            .value
            .set_header_extension_capabilities_to_negotiate(extensions)
    }

    pub fn negotiated_header_extensions(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<Vec<RtpHeaderExtensionCapability>, PeerError> {
        self.transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .negotiated_header_extensions()
    }

    pub fn set_header_extensions_to_negotiate(
        &mut self,
        transceiver: SessionTransceiverId,
        extensions: &[RtpHeaderExtensionCapability],
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .set_header_extensions_to_negotiate(extensions)
    }

    pub fn transceiver_media_type(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<crate::MediaType, PeerError> {
        Ok(self
            .transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .media_type())
    }

    pub fn transceiver_stopping(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<bool, PeerError> {
        Ok(self
            .transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .stopping())
    }

    pub fn transceiver_receptive(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<bool, PeerError> {
        Ok(self
            .transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .receptive())
    }

    pub fn fired_direction(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<Option<crate::RtpTransceiverDirectionValue>, PeerError> {
        Ok(self
            .transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .fired_direction())
    }

    pub fn sframe_enabled(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<Option<bool>, PeerError> {
        Ok(self
            .transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .sframe_enabled())
    }

    pub fn direction_value(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<crate::RtpTransceiverDirectionValue, PeerError> {
        Ok(self
            .transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .direction_value())
    }

    pub fn current_direction_value(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<Option<crate::RtpTransceiverDirectionValue>, PeerError> {
        Ok(self
            .transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .current_direction_value())
    }

    pub fn set_direction_value(
        &mut self,
        transceiver: SessionTransceiverId,
        direction: crate::RtpTransceiverDirectionValue,
    ) -> Result<(), crate::RtpTransceiverOperationError> {
        self.check_open()
            .map_err(crate::RtpTransceiverOperationError::Binding)?;
        self.transceivers
            .get(&transceiver)
            .ok_or_else(missing)
            .map_err(crate::RtpTransceiverOperationError::Binding)?
            .value
            .value
            .set_direction_value(direction)
    }

    pub fn stop_standard(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<(), crate::RtpTransceiverOperationError> {
        self.check_open()
            .map_err(crate::RtpTransceiverOperationError::Binding)?;
        self.transceivers
            .get(&transceiver)
            .ok_or_else(missing)
            .map_err(crate::RtpTransceiverOperationError::Binding)?
            .value
            .value
            .stop_standard()
    }

    pub fn transceiver_stopped(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<bool, PeerError> {
        Ok(self
            .transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .stopped())
    }

    pub fn set_direction(
        &mut self,
        transceiver: SessionTransceiverId,
        direction: RtpTransceiverDirection,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .set_direction(direction)
    }
    /// Replace/detach one native audio source. PCM and encoded Opus retain
    /// their selected factory contracts; no codec factory is rebuilt here.
    pub fn replace_audio_source(
        &mut self,
        sender: SessionSenderId,
        source: Option<SessionSourceId>,
        track_id: &str,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let track = source
            .map(|id| match self.sources.get(&id).ok_or_else(missing)? {
                Source::Pcm(source) => self.factory.create_audio_track(track_id, source),
                Source::Opus(source) => self.factory.create_encoded_audio_track(track_id, source),
                _ => Err(missing()),
            })
            .transpose()?;
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .set_audio_track(track.as_ref())
    }

    pub fn replace_video_source(
        &mut self,
        sender: SessionSenderId,
        source: Option<SessionSourceId>,
        track_id: &str,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let track = source
            .map(|id| {
                self.sources
                    .get(&id)
                    .ok_or_else(missing)?
                    .video_track(&self.factory, track_id)
            })
            .transpose()?;
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .set_track(track.as_ref())
    }
    pub fn request_keyframe(&mut self, receiver: SessionReceiverId) -> Result<(), PeerError> {
        self.check_open()?;
        self.receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .request_keyframe()
    }
    pub fn attach_pcm_audio(&mut self, receiver: SessionReceiverId) -> Result<(), PeerError> {
        self.attach_pcm(receiver)
    }

    /// Detach either selected audio receive mode without changing video sinks.
    pub fn detach_audio(&mut self, receiver: SessionReceiverId) -> Result<(), PeerError> {
        if self
            .receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .media_type()
            != crate::MediaType::Audio
        {
            return Err(error(
                PeerErrorKind::InvalidParameter,
                "receiver is not audio",
            ));
        }
        self.close_media_sink(receiver)
    }

    /// Caller-paced headless playout, sharing the exclusive receiver mode.
    pub fn try_pcm_audio_frame(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<crate::DecodedAudioFrame>, PeerError> {
        self.try_pcm_frame(receiver)
    }

    fn receiver_without_sink(
        &self,
        receiver: SessionReceiverId,
    ) -> Result<&RtpReceiver, PeerError> {
        self.check_open()?;
        if self.media_sinks.contains_key(&receiver) {
            return Err(error(
                PeerErrorKind::InvalidState,
                "receiver already has a sink",
            ));
        }
        Ok(&self.receivers.get(&receiver).ok_or_else(missing)?.value)
    }

    /// Select decoded PCM. Headless playout is explicitly pulled by
    /// `try_pcm_frame`, not by actor readiness or a hidden audio device.
    pub fn attach_pcm(&mut self, receiver: SessionReceiverId) -> Result<(), PeerError> {
        let sink = self.receiver_without_sink(receiver)?.attach_audio_sink()?;
        self.media_sinks.insert(receiver, MediaSink::Pcm(sink));
        Ok(())
    }

    pub fn attach_raw_video(&mut self, receiver: SessionReceiverId) -> Result<(), PeerError> {
        self.attach_raw_video_with_wants(receiver, &crate::VideoSinkWants::default())
    }
    pub fn attach_raw_video_with_wants(
        &mut self,
        receiver: SessionReceiverId,
        wants: &crate::VideoSinkWants,
    ) -> Result<(), PeerError> {
        let track = self
            .receiver_without_sink(receiver)?
            .track()
            .ok_or_else(missing)?;
        let sink = track.attach_sink_with_wants(wants)?;
        self.media_sinks.insert(receiver, MediaSink::RawVideo(sink));
        Ok(())
    }

    /// Synchronous decoded-frame observations on the existing raw sink, before
    /// its independent polling retention. Receiver identity is this validated
    /// session binding; decoded VideoFrame callbacks do not supply an SSRC.
    pub fn attach_raw_video_with_observer(
        &mut self,
        receiver: SessionReceiverId,
        observer: impl crate::DecodedVideoFrameObserver,
    ) -> Result<(), PeerError> {
        let track = self
            .receiver_without_sink(receiver)?
            .track()
            .ok_or_else(missing)?;
        let sink = track.attach_sink_with_observer(observer)?;
        self.media_sinks.insert(receiver, MediaSink::RawVideo(sink));
        Ok(())
    }

    pub fn raw_video_observer_failed(
        &self,
        receiver: SessionReceiverId,
    ) -> Result<bool, PeerError> {
        Ok(self.raw_video_sink(receiver)?.observer_failed())
    }

    fn raw_video_sink(&self, receiver: SessionReceiverId) -> Result<&crate::VideoSink, PeerError> {
        match self.media_sinks.get(&receiver) {
            Some(MediaSink::RawVideo(sink) | MediaSink::RawAndEncodedVideo { raw: sink, .. }) => {
                Ok(sink)
            }
            _ => Err(missing()),
        }
    }

    pub fn raw_video_sink_wants(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<crate::VideoSinkWants>, PeerError> {
        let sink = self.raw_video_sink(receiver)?;
        Ok(sink.wants())
    }
    pub fn update_raw_video_sink_wants(
        &mut self,
        receiver: SessionReceiverId,
        wants: &crate::VideoSinkWants,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let sink = self.raw_video_sink(receiver)?;
        sink.update_wants(wants)
    }

    pub fn attach_encoded_audio(&mut self, receiver: SessionReceiverId) -> Result<(), PeerError> {
        let sink = self
            .receiver_without_sink(receiver)?
            .attach_encoded_audio_sink()?;
        self.media_sinks
            .insert(receiver, MediaSink::EncodedAudio(sink));
        Ok(())
    }
    pub fn attach_encoded_video(&mut self, receiver: SessionReceiverId) -> Result<(), PeerError> {
        let sink = self
            .receiver_without_sink(receiver)?
            .attach_ordered_encoded_sink()?;
        self.media_sinks
            .insert(receiver, MediaSink::EncodedVideo(sink));
        Ok(())
    }

    /// Observe ordered encoded video and native-decoded I420 on the SAME
    /// receiver. The ordered tap forwards original frames unchanged to the
    /// configured decoder; this does not select a codec or change interception.
    /// A factory configured for encoded-only receive is refused as for raw video.
    /// Drain the two existing frame APIs independently. Dropped-frame counts
    /// aggregate both observer queues, not native decoder/playout losses.
    pub fn attach_raw_and_encoded_video(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<(), PeerError> {
        let receiver_ref = self.receiver_without_sink(receiver)?;
        let track = receiver_ref.track().ok_or_else(missing)?;
        // Refuse unavailable decoded output BEFORE reserving the native
        // receiver-lifetime encoded observer. Later refusal drops this raw sink.
        let raw = track.attach_sink_with_wants(&crate::VideoSinkWants::default())?;
        let encoded = receiver_ref.attach_ordered_encoded_sink()?;
        self.media_sinks
            .insert(receiver, MediaSink::RawAndEncodedVideo { raw, encoded });
        Ok(())
    }

    /// Combined observation with a synchronous decoded-frame callback installed
    /// before raw attachment. The ordered observer and polling queues retain
    /// their existing behavior, wants and native receiver-lifetime fences.
    pub fn attach_raw_and_encoded_video_with_observer(
        &mut self,
        receiver: SessionReceiverId,
        observer: impl crate::DecodedVideoFrameObserver,
    ) -> Result<(), PeerError> {
        let receiver_ref = self.receiver_without_sink(receiver)?;
        let track = receiver_ref.track().ok_or_else(missing)?;
        let raw = track.attach_sink_with_observer(observer)?;
        let encoded = receiver_ref.attach_ordered_encoded_sink()?;
        self.media_sinks
            .insert(receiver, MediaSink::RawAndEncodedVideo { raw, encoded });
        Ok(())
    }

    /// Observe both actual native callback streams independently of polling
    /// retention. Encoded callback metadata loss and panic retirement remain
    /// explicit; callback order relative to I420 is not fixed.
    pub fn attach_raw_and_encoded_video_with_observers(
        &mut self,
        receiver: SessionReceiverId,
        decoded: impl crate::DecodedVideoFrameObserver,
        encoded: impl crate::EncodedVideoFrameObserver,
    ) -> Result<(), PeerError> {
        let receiver_ref = self.receiver_without_sink(receiver)?;
        let track = receiver_ref.track().ok_or_else(missing)?;
        let raw = track.attach_sink_with_observer(decoded)?;
        let encoded = receiver_ref.attach_ordered_encoded_sink_with_observer(encoded)?;
        self.media_sinks
            .insert(receiver, MediaSink::RawAndEncodedVideo { raw, encoded });
        Ok(())
    }

    pub fn encoded_video_observer_failed(
        &self,
        receiver: SessionReceiverId,
    ) -> Result<bool, PeerError> {
        match self.media_sinks.get(&receiver) {
            Some(
                MediaSink::EncodedVideo(sink) | MediaSink::RawAndEncodedVideo { encoded: sink, .. },
            ) => Ok(sink.observer_failed()),
            _ => Err(missing()),
        }
    }

    /// Detach before releasing the actor's sink. Audio may then switch modes;
    /// encoded-video reattachment retains its native receiver-lifetime limit.
    /// Owned frames already returned to the caller are unaffected.
    pub fn close_media_sink(&mut self, receiver: SessionReceiverId) -> Result<(), PeerError> {
        self.check_open()?;
        self.receivers.get(&receiver).ok_or_else(missing)?;
        if let Some(sink) = self.media_sinks.get_mut(&receiver) {
            sink.close()?;
        }
        self.media_sinks.remove(&receiver);
        Ok(())
    }

    pub fn try_pcm_frame(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<crate::DecodedAudioFrame>, PeerError> {
        let Some(MediaSink::Pcm(sink)) = self.media_sinks.get(&receiver) else {
            return Err(missing());
        };
        Ok(sink.try_next_frame())
    }
    pub fn try_raw_video_native_frame(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<SessionRawVideoFrameId>, PeerError> {
        let sink = self.raw_video_sink(receiver)?;
        let frame = sink.try_next_native_frame();
        frame
            .map(|frame| self.retain_raw_video_frame(frame))
            .transpose()
    }
    pub fn try_raw_video_frame(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<crate::VideoFrame>, PeerError> {
        let sink = self.raw_video_sink(receiver)?;
        Ok(sink.try_next_frame())
    }
    /// Pop a native raw-sink control callback without pumping native work.
    pub fn try_raw_video_sink_event(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<crate::VideoSinkEvent>, PeerError> {
        let sink = self.raw_video_sink(receiver)?;
        Ok(sink.try_next_event())
    }

    pub fn dropped_raw_video_sink_events(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<u64, PeerError> {
        let sink = self.raw_video_sink(receiver)?;
        Ok(sink.dropped_events())
    }

    pub fn try_audio_frame(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<EncodedAudioFrame>, PeerError> {
        let Some(MediaSink::EncodedAudio(sink)) = self.media_sinks.get(&receiver) else {
            return Err(missing());
        };
        Ok(sink.try_next_frame())
    }
    pub fn try_video_frame(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<EncodedReceivedVideoFrame>, PeerError> {
        let Some(
            MediaSink::EncodedVideo(sink) | MediaSink::RawAndEncodedVideo { encoded: sink, .. },
        ) = self.media_sinks.get(&receiver)
        else {
            return Err(missing());
        };
        Ok(sink.try_next_frame())
    }
    /// Raw-video observer retention losses, not native decoder/playout losses.
    /// Kept separate from encoded observation so dual consumers cannot mistake
    /// an encoded queue overflow for a dropped native decoded observation.
    pub fn dropped_raw_video_frames(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<u64, PeerError> {
        Ok(self.raw_video_sink(receiver)?.dropped_frames())
    }

    /// Encoded-video observer retention losses for single or combined mode.
    pub fn dropped_encoded_video_frames(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<u64, PeerError> {
        let Some(
            MediaSink::EncodedVideo(sink) | MediaSink::RawAndEncodedVideo { encoded: sink, .. },
        ) = self.media_sinks.get(&receiver)
        else {
            return Err(missing());
        };
        Ok(sink.dropped_frames())
    }

    pub fn dropped_frames(&mut self, receiver: SessionReceiverId) -> Result<u64, PeerError> {
        Ok(self
            .media_sinks
            .get(&receiver)
            .ok_or_else(missing)?
            .dropped_frames())
    }

    /// Close without removing the peer's observation identity. Accepted pending
    /// operations still yield exactly one outcome through try_peer_event.
    pub fn close_peer(&mut self, peer: SessionPeerId) -> Result<(), PeerError> {
        self.peers.get(&peer).ok_or_else(missing)?;
        // Admission precedes even observer unregistration, which can block on
        // native roles and must not partly mutate a rejected callback close.
        self.factory.check_control_call()?;
        self.connectivity.retire_dtmf_observers(peer);
        // Retire raw observer slots before stopping the peer's resource graph.
        // Already queued records remain available, including after close.
        for observer in self
            .sender_observers
            .values()
            .filter(|value| value.peer == peer)
        {
            observer.value.unregister_observer();
        }
        for observer in self
            .receiver_observers
            .values()
            .filter(|value| value.peer == peer)
        {
            observer.value.unregister_observer();
        }
        self.peers
            .get_mut(&peer)
            .ok_or_else(missing)?
            .value
            .close()?;
        for resource in self.channels.values_mut().filter(|v| v.peer == peer) {
            resource.value.close()?;
        }
        for (&id, resource) in &self.receivers {
            if resource.peer == peer
                && let Some(sink) = self.media_sinks.get_mut(&id)
            {
                sink.close()?;
            }
        }
        Ok(())
    }
    pub fn shutdown(&mut self) -> Result<(), PeerError> {
        if self.closed {
            return Ok(());
        }
        self.factory.check_control_call()?;
        for encoder in self.video_encoders.values_mut() {
            let _ = encoder.release();
        }
        for decoder in self.video_decoders.values_mut() {
            let _ = decoder.release();
        }
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            self.close_peer(peer)?;
        }
        for source in self.sources.values_mut() {
            source.close()?;
        }
        for sink in self.video_sinks.values_mut() {
            sink.close()?;
        }
        for sink in self.video_source_encoded_sinks.values() {
            sink.close();
        }
        for observer in self.video_observers.values() {
            observer.close();
        }
        for observer in self.local_stream_observers.values() {
            observer.close();
        }
        for observer in self.audio_observers.values() {
            observer.close();
        }
        self.sender_parameter_operations.clear();
        self.audio_observers.clear();
        self.audio_source_views.clear();
        self.sources.clear();
        #[cfg(feature = "native")]
        self.desktop_options.clear();
        #[cfg(feature = "native")]
        self.camera_device_info.clear();
        #[cfg(feature = "native")]
        self.camera_options.clear();
        #[cfg(feature = "native")]
        self.desktop_metadata.clear();
        self.closed = true;
        self.readiness.notify();
        Ok(())
    }
}

impl<R: SessionRuntime> Drop for Session<R> {
    fn drop(&mut self) {
        if self.factory.is_empty() {
            return;
        }
        if runtime::retire(self) {
            return;
        }
        let _ = self.shutdown();
        // Field order releases observers/sinks, RTP handles, sources and peers
        // before the factory releases its existing engine role threads.
    }
}
