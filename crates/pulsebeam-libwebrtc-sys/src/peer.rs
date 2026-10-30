use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fmt,
    rc::Rc,
};

use crate::audio::{AudioCodecCapability, AudioSource, AudioTrack, EncodedAudioSource};

use crate::{
    AudioDecoderFactory, AudioEncoderFactory, ControlledPeerDriver, Environment,
    NetworkManagerProvider, NetworkThread, PacketSocketFactoryProvider, SignalingThread,
    VideoDecoderFactoryHandle, VideoEncoderFactoryHandle, WorkerThread,
    data_channel::DataChannel,
    ffi,
    video::{
        RtpReceiver, RtpSender, RtpTransceiver, RtpTransceiverDirection, VideoCodecCapability,
        VideoSource, VideoTrack,
    },
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OperationId(u64);

impl OperationId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SessionDescriptionType {
    Offer = 0,
    ProvisionalAnswer = 1,
    Answer = 2,
    Rollback = 3,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDescription {
    pub kind: SessionDescriptionType,
    pub sdp: String,
}

/// Exact native SDP serialization, including non-UTF-8 protocol bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDescriptionBytes {
    pub kind: SessionDescriptionType,
    pub sdp: Vec<u8>,
}
impl From<SessionDescription> for SessionDescriptionBytes {
    fn from(value: SessionDescription) -> Self {
        Self {
            kind: value.kind,
            sdp: value.sdp.into_bytes(),
        }
    }
}
impl TryFrom<SessionDescriptionBytes> for SessionDescription {
    type Error = std::string::FromUtf8Error;
    fn try_from(value: SessionDescriptionBytes) -> Result<Self, Self::Error> {
        Ok(Self {
            kind: value.kind,
            sdp: String::from_utf8(value.sdp)?,
        })
    }
}
/// Native legacy/standardized ICE state, separate from overall peer connection state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerIceConnectionState {
    New,
    Checking,
    Connected,
    Completed,
    Failed,
    Disconnected,
    Closed,
    Max,
    Native(i32),
}
impl PeerIceConnectionState {
    pub fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::New,
            1 => Self::Checking,
            2 => Self::Connected,
            3 => Self::Completed,
            4 => Self::Failed,
            5 => Self::Disconnected,
            6 => Self::Closed,
            7 => Self::Max,
            value => Self::Native(value),
        }
    }
    pub fn raw(self) -> i32 {
        match self {
            Self::New => 0,
            Self::Checking => 1,
            Self::Connected => 2,
            Self::Completed => 3,
            Self::Failed => 4,
            Self::Disconnected => 5,
            Self::Closed => 6,
            Self::Max => 7,
            Self::Native(value) => value,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerIceSnapshot {
    pub legacy_state: PeerIceConnectionState,
    pub standardized_state: PeerIceConnectionState,
    pub can_trickle_ice_candidates: Option<bool>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PeerDescriptionBytes {
    pub current_local: Option<SessionDescriptionBytes>,
    pub current_remote: Option<SessionDescriptionBytes>,
    pub pending_local: Option<SessionDescriptionBytes>,
    pub pending_remote: Option<SessionDescriptionBytes>,
}

/// An atomic snapshot of negotiated and in-progress local/remote SDP.
/// Descriptions are copied on the signaling thread and owned by the caller.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PeerDescriptions {
    pub current_local: Option<SessionDescription>,
    pub current_remote: Option<SessionDescription>,
    pub pending_local: Option<SessionDescription>,
    pub pending_remote: Option<SessionDescription>,
}

impl PeerDescriptions {
    /// Effective local SDP, preferring an in-progress negotiation over the
    /// current description, as native `local_description()` does.
    pub fn local(&self) -> Option<&SessionDescription> {
        self.pending_local.as_ref().or(self.current_local.as_ref())
    }

    /// Effective remote SDP, preferring the pending description if present.
    pub fn remote(&self) -> Option<&SessionDescription> {
        self.pending_remote
            .as_ref()
            .or(self.current_remote.as_ref())
    }
}

/// Options for native offer/answer generation. Each field is forwarded to the
/// engine, which decides its applicability to the current SDP semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfferAnswerOptions {
    /// Legacy receive count: `None` leaves it unspecified, `Some(0)` disables
    /// receiving and `Some(1)` requests receiving. Other counts are submitted
    /// to native validation rather than silently clamped.
    pub receive_video: Option<u32>,
    pub receive_audio: Option<u32>,
    pub voice_activity_detection: bool,
    pub ice_restart: bool,
    /// Offer BUNDLE, not RTP/RTCP multiplexing.
    pub use_rtp_mux: bool,
    pub raw_packetization_for_video: bool,
    /// Native Plan B simulcast generation option. For Unified Plan, configure
    /// the transceiver's send encodings instead.
    pub simulcast_layers: u32,
    pub use_obsolete_sctp_sdp: bool,
}

impl Default for OfferAnswerOptions {
    fn default() -> Self {
        Self {
            receive_video: None,
            receive_audio: None,
            voice_activity_detection: true,
            ice_restart: false,
            use_rtp_mux: true,
            raw_packetization_for_video: false,
            simulcast_layers: 1,
            use_obsolete_sctp_sdp: false,
        }
    }
}

impl OfferAnswerOptions {
    fn to_native(&self) -> Result<ffi::FfiOfferAnswerOptions, PeerError> {
        let count = |value| {
            i32::try_from(value).map_err(|_| PeerError {
                kind: PeerErrorKind::InvalidRange,
                message: "offer/answer count exceeds native i32 range".into(),
            })
        };
        Ok(ffi::FfiOfferAnswerOptions {
            receive_video: self.receive_video.map(count).transpose()?.unwrap_or(-1),
            receive_audio: self.receive_audio.map(count).transpose()?.unwrap_or(-1),
            voice_activity_detection: self.voice_activity_detection,
            ice_restart: self.ice_restart,
            use_rtp_mux: self.use_rtp_mux,
            raw_packetization_for_video: self.raw_packetization_for_video,
            simulcast_layers: count(self.simulcast_layers)?,
            use_obsolete_sctp_sdp: self.use_obsolete_sctp_sdp,
        })
    }
}

/// Result of the native synchronous candidate operation. Rejection has no
/// native error detail. Acceptance is an acknowledgment, not a promise that
/// a candidate pair is selected or that a matching candidate previously existed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IceCandidateDisposition {
    Accepted,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IceCandidate {
    pub sdp_mid: String,
    pub sdp_mline_index: i32,
    pub candidate: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IceCandidateBytes {
    pub sdp_mid: Vec<u8>,
    pub sdp_mline_index: i32,
    pub candidate: Vec<u8>,
}
impl From<IceCandidate> for IceCandidateBytes {
    fn from(value: IceCandidate) -> Self {
        Self {
            sdp_mid: value.sdp_mid.into_bytes(),
            sdp_mline_index: value.sdp_mline_index,
            candidate: value.candidate.into_bytes(),
        }
    }
}
impl TryFrom<IceCandidateBytes> for IceCandidate {
    type Error = std::string::FromUtf8Error;
    fn try_from(value: IceCandidateBytes) -> Result<Self, Self::Error> {
        Ok(Self {
            sdp_mid: String::from_utf8(value.sdp_mid)?,
            sdp_mline_index: value.sdp_mline_index,
            candidate: String::from_utf8(value.candidate)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalingState {
    Stable,
    HaveLocalOffer,
    HaveLocalProvisionalAnswer,
    HaveRemoteOffer,
    HaveRemoteProvisionalAnswer,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IceGatheringState {
    New,
    Gathering,
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerErrorKind {
    UnsupportedOperation,
    UnsupportedParameter,
    InvalidParameter,
    InvalidRange,
    Syntax,
    InvalidState,
    InvalidModification,
    Network,
    ResourceExhausted,
    Internal,
    Operation,
    Closed,
    NativeConstruction,
    /// Text-only convenience projection failed; native bytes remain available.
    DescriptionNotUtf8,
    MessageNotUtf8,
    UnrepresentableCodecCapability,
    /// Binding admission/qualification failure, not a native unsupported backend.
    AudioProcessingAdmission,
    AudioDeviceAdmission,
}

/// Exact owned native error bytes when text projection is unavailable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerErrorBytes {
    pub kind: PeerErrorKind,
    pub message: Vec<u8>,
}
impl fmt::Display for PeerErrorBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {:?}", self.kind, self.message)
    }
}
impl std::error::Error for PeerErrorBytes {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerError {
    pub kind: PeerErrorKind,
    pub message: String,
}

impl fmt::Display for PeerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for PeerError {}
impl From<PeerError> for PeerErrorBytes {
    fn from(value: PeerError) -> Self {
        Self {
            kind: value.kind,
            message: value.message.into_bytes(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationCompletion {
    pub operation_id: OperationId,
    pub result: Result<Option<SessionDescription>, PeerError>,
}

/// A bounded snapshot of upstream stats. Timestamps are microseconds since
/// the Unix epoch; a missing metric is `None`, never an implicit zero.
/// Native string fields retain their bytes, including non-UTF-8 and NUL.
#[derive(Clone, Debug, PartialEq)]
pub struct PeerStatsSnapshot {
    /// Complete retained SDK report; records below are a compatibility projection.
    pub report: crate::RtcStatsReport,
    pub operation_id: OperationId,
    pub timestamp_us: i64,
    pub records: Vec<PeerStatsRecord>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PeerStatsRecord {
    CandidatePair(CandidatePairStats),
    Transport(TransportStats),
    InboundRtp(InboundRtpStats),
    OutboundRtp(OutboundRtpStats),
    DataChannel(DataChannelStats),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CandidatePairStats {
    pub id: Vec<u8>,
    pub timestamp_us: i64,
    pub transport_id: Option<Vec<u8>>,
    pub local_candidate_id: Option<Vec<u8>>,
    pub remote_candidate_id: Option<Vec<u8>>,
    pub state: Option<Vec<u8>>,
    pub nominated: Option<bool>,
    pub packets_sent: Option<u64>,
    pub packets_received: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub bytes_received: Option<u64>,
    pub current_round_trip_time: Option<f64>,
    pub available_outgoing_bitrate: Option<f64>,
    pub available_incoming_bitrate: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransportStats {
    pub id: Vec<u8>,
    pub timestamp_us: i64,
    pub selected_candidate_pair_id: Option<Vec<u8>>,
    pub ice_state: Option<Vec<u8>>,
    pub dtls_state: Option<Vec<u8>>,
    pub packets_sent: Option<u64>,
    pub packets_received: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub bytes_received: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InboundRtpStats {
    pub id: Vec<u8>,
    pub timestamp_us: i64,
    pub kind: Option<Vec<u8>>,
    pub ssrc: Option<u64>,
    pub transport_id: Option<Vec<u8>>,
    pub mid: Option<Vec<u8>>,
    pub packets_received: Option<u64>,
    pub packets_lost: Option<i64>,
    pub bytes_received: Option<u64>,
    pub jitter: Option<f64>,
    pub frames_received: Option<u64>,
    pub frames_decoded: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutboundRtpStats {
    pub id: Vec<u8>,
    pub timestamp_us: i64,
    pub kind: Option<Vec<u8>>,
    pub ssrc: Option<u64>,
    pub transport_id: Option<Vec<u8>>,
    pub mid: Option<Vec<u8>>,
    pub rid: Option<Vec<u8>>,
    pub packets_sent: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub target_bitrate: Option<f64>,
    pub frames_encoded: Option<u64>,
    pub frames_sent: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DataChannelStats {
    pub id: Vec<u8>,
    pub timestamp_us: i64,
    pub label: Option<Vec<u8>>,
    pub protocol: Option<Vec<u8>>,
    pub state: Option<Vec<u8>>,
    pub data_channel_identifier: Option<i64>,
    pub messages_sent: Option<u64>,
    pub messages_received: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub bytes_received: Option<u64>,
}

/// Native application-configurable bandwidth estimation settings. Enabling
/// probes permits padding on a sending RTX-capable transceiver before media;
/// it does not allocate a sender or guarantee that a probe will be emitted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BandwidthEstimationSettings {
    pub allow_probe_without_media: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataChannelMessageDirection {
    Send,
    Receive,
}
/// Actual peer-wide SDK message-log callback. Native channel IDs are not
/// session resource IDs, and labels do not establish resource identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerDataChannelMessage {
    pub unix_timestamp_ms: i64,
    pub datachannel_id: i32,
    pub label: Vec<u8>,
    pub direction: DataChannelMessageDirection,
    pub message: crate::DataChannelMessage,
}

/// Binding-owned observation backlog, not native transport buffering.
/// Non-advisory control events retain at most 128 records/256 KiB of copied bytes;
/// callback-time advisory snapshots can additionally be retained once per kind.
/// Arrival-handle maps independently share a 128-handle cap. Overflow discards
/// only a new observation, never an accepted operation outcome or native SCTP
/// state; callers must account for missing arrivals/candidates when nonzero.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PeerEventObservation {
    pub dropped_control_events: u64,
    pub coalesced_advisories: u64,
    pub retained_control_events: u32,
    pub admitted_operations: u32,
    pub pending_operations: u32,
}

#[derive(Debug)]
pub enum PeerConnectionEvent {
    RenegotiationNeeded,
    IceConnectionReceivingChanged(bool),
    InterestingUsage {
        usage_pattern: i32,
    },
    DataChannelMessage(PeerDataChannelMessage),
    IceSelectedCandidatePairChanged(crate::IceCandidatePairChange),
    Stats(PeerStatsSnapshot),
    LegacyStats(crate::PeerLegacyStatsSnapshot),
    OperationComplete(OperationCompletion),
    /// One successful native SDP completion whose bytes are not UTF-8. This is
    /// the same terminal operation, not an additional event or native failure.
    OperationCompleteBytes {
        operation_id: OperationId,
        description: SessionDescriptionBytes,
    },
    OperationFailedBytes {
        operation_id: OperationId,
        error: PeerErrorBytes,
    },
    IceCandidate(IceCandidate),
    IceCandidateBytes(IceCandidateBytes),
    IceCandidateRemoved(IceCandidate),
    IceCandidateRemovedBytes(IceCandidateBytes),
    ConnectionStateChanged(ConnectionState),
    SignalingStateChanged(SignalingState),
    IceGatheringStateChanged(IceGatheringState),
    /// Legacy SDK ICE callback state, retained at callback time.
    IceConnectionStateChanged(PeerIceConnectionState),
    /// Distinct standards-compliant SDK ICE callback state.
    StandardizedIceConnectionStateChanged(PeerIceConnectionState),
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
    DataChannel(DataChannel),
    Track(RtpTransceiver),
    TrackRemoved(RtpReceiver),
    StreamAdded(crate::MediaStream),
    StreamRemoved(crate::MediaStream),
    TrackAdded {
        receiver: RtpReceiver,
        streams: Vec<crate::MediaStream>,
    },
    Closed,
}

/// ICE server URLs use `stun:`, `stuns:`, `turn:` or `turns:` URI schemes.
/// TURN UDP/TCP/TLS transport is selected by the URL (`?transport=udp` or
/// `?transport=tcp`); `turns:` requires a valid server TLS certificate.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub password: String,
}

impl fmt::Debug for IceServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IceServer")
            .field("urls", &self.urls)
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum IceTransportPolicy {
    None = 0,
    RelayOnly = 1,
    NoHost = 2,
    #[default]
    All = 3,
}
impl IceTransportPolicy {
    pub(crate) fn from_native(value: u8) -> Self {
        match value {
            0 => Self::None,
            1 => Self::RelayOnly,
            2 => Self::NoHost,
            3 => Self::All,
            _ => unreachable!("unknown pinned native ICE transport policy"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerConfiguration {
    pub ice_candidate_pool_size: u16,
    pub always_negotiate_data_channels: bool,
    pub ice_servers: Vec<IceServer>,
    pub ice_transport_policy: IceTransportPolicy,
    /// Additional PEM-encoded CA trusted for TURN/TLS on this peer. The pinned
    /// WebRTC roots remain trusted, and the server hostname is still checked.
    /// `None` uses only upstream trust anchors.
    pub turn_tls_ca_pem: Option<String>,
}

impl Default for PeerConfiguration {
    fn default() -> Self {
        Self {
            ice_candidate_pool_size: 0,
            always_negotiate_data_channels: true,
            ice_servers: Vec::new(),
            ice_transport_policy: IceTransportPolicy::All,
            turn_tls_ca_pem: None,
        }
    }
}

pub struct PeerConnectionFactoryBuilder {
    environment: Option<Environment>,
    network_thread: Option<NetworkThread>,
    worker_thread: Option<WorkerThread>,
    signaling_thread: Option<SignalingThread>,
    controlled_driver: Option<ControlledPeerDriver>,
    network_manager: Option<NetworkManagerProvider>,
    network_manager_factory: Option<crate::NetworkManagerFactory>,
    network_monitor_factory: Option<std::sync::Arc<dyn crate::NetworkMonitorFactory>>,
    network_predictor_factory: Option<crate::NetworkStatePredictorFactory>,
    fec_factory: Option<crate::FecControllerFactory>,
    network_controller_factory: Option<crate::NetworkControllerFactory>,
    packet_socket_factory: Option<PacketSocketFactoryProvider>,
    socket_factory: Option<crate::PacketSocketFactory>,
    audio_encoder: Option<AudioEncoderFactory>,
    audio_decoder: Option<AudioDecoderFactory>,
    video_encoder: Option<VideoEncoderFactoryHandle>,
    video_decoder: Option<VideoDecoderFactoryHandle>,
    native_audio: bool,
    native_audio_creation: ffi::FfiNativeAudioDeviceCreation,
    #[cfg(all(feature = "native", target_os = "linux"))]
    audio_device_module: Option<crate::AudioDeviceModule>,
    event_logging: bool,
    event_log_factory: Option<std::sync::Arc<dyn crate::RtcEventLogFactory>>,
    encode_metronome: Option<crate::Metronome>,
    decode_metronome: Option<crate::Metronome>,
    audio_processing: Option<crate::AudioProcessingConfig>,
    audio_processor: Option<crate::AudioProcessingModule>,
    audio_processor_builder: Option<crate::BuiltinAudioProcessorBuilder>,
    audio_mixer: Option<crate::AudioMixer>,
    options: crate::FactoryOptions,
    execution_resources: Option<crate::ExecutionResources>,
    readiness: Option<std::sync::Arc<crate::readiness::Readiness>>,
}

impl PeerConnectionFactoryBuilder {
    /// Explicit native Call resource configuration, without changing media
    /// implementations or standalone codec settings. Without registration,
    /// caller-driven factories default to one core and threaded factories use
    /// native host detection. Registering an unset budget preserves host detection.
    pub fn execution_resources(mut self, resources: crate::ExecutionResources) -> Self {
        self.execution_resources = Some(resources);
        self
    }

    /// Inject the full owned congestion-controller contract. The environment
    /// must enable WebRTC-Bwe-InjectedCongestionController; absent injection
    /// preserves stock controller selection.
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
    /// The pinned default SDK path ignores this dependency slot. Build rejects
    /// this registration; select NetworkControllerFactory::goog_cc with the
    /// predictor in GoogCcFactoryConfig and explicit RFC 8888 state instead.
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
    /// Use an owned encode metronome with native worker-sequence tick delivery.
    /// Controlled execution requires a caller-driven provider, without timer
    /// threads or wall-clock progress. Clones share the source, not requests.
    pub fn encode_metronome(mut self, metronome: &crate::Metronome) -> Self {
        self.encode_metronome = Some(metronome.clone());
        self
    }
    pub fn encode_metronome_provider(self, provider: impl crate::MetronomeProvider) -> Self {
        self.encode_metronome(&crate::Metronome::from_provider(provider))
    }
    /// Use an independent owned decode metronome. Its provider must supply
    /// ticks for decode consumers to progress. See `MetronomeProvider`.
    pub fn decode_metronome(mut self, metronome: &crate::Metronome) -> Self {
        self.decode_metronome = Some(metronome.clone());
        self
    }
    pub fn decode_metronome_provider(self, provider: impl crate::MetronomeProvider) -> Self {
        self.decode_metronome(&crate::Metronome::from_provider(provider))
    }

    /// Retain a caller-defined native monitor factory. Native creation rejects
    /// combining this with a custom network manager, including controlled
    /// networking. Monitor callbacks run on the native network thread.
    pub fn network_monitor_factory(mut self, factory: impl crate::NetworkMonitorFactory) -> Self {
        self.network_monitor_factory = Some(std::sync::Arc::new(factory));
        self
    }
    pub(crate) fn shared_network_monitor_factory(
        mut self,
        factory: std::sync::Arc<dyn crate::NetworkMonitorFactory>,
    ) -> Self {
        self.network_monitor_factory = Some(factory);
        self
    }

    /// Supply a caller-defined native event-log factory. This supersedes the
    /// built-in selection. Controlled factories require caller-controlled
    /// implementations of the logger and all retained output destinations.
    pub fn event_log_factory(mut self, factory: impl crate::RtcEventLogFactory) -> Self {
        self.event_log_factory = Some(std::sync::Arc::new(factory));
        self
    }

    /// Select the same owning factory registration used for standalone logs.
    /// An explicit builtin replaces a prior custom registration, and vice versa.
    pub fn event_log_factory_handle(mut self, factory: &crate::RtcEventLogFactoryHandle) -> Self {
        self.event_log_factory = factory.registration();
        self.event_logging = self.event_log_factory.is_none();
        self
    }

    pub(crate) fn shared_event_log_factory(
        mut self,
        factory: std::sync::Arc<dyn crate::RtcEventLogFactory>,
    ) -> Self {
        self.event_log_factory = Some(factory);
        self
    }

    /// Supply the pinned built-in event-log factory. Disabled by default.
    /// Native field trials can disable the implementation. Runtime-bound
    /// managed factories retire its queues on their creator, using native-ready
    /// lifecycle progress for the SDK's synchronous stop operation.
    pub fn event_logging(mut self, enabled: bool) -> Self {
        self.event_logging = enabled;
        self
    }

    /// Apply native factory options before any peer is created.
    pub fn options(mut self, options: crate::FactoryOptions) -> Self {
        self.options = options;
        self
    }

    pub(crate) fn readiness(
        mut self,
        readiness: std::sync::Arc<crate::readiness::Readiness>,
    ) -> Self {
        self.readiness = Some(readiness);
        self
    }

    pub fn environment(mut self, environment: Environment) -> Self {
        self.environment = Some(environment);
        self
    }

    /// Use one caller-pumped WebRTC thread for all peer roles. Requires an
    /// environment built with this driver's clock and cooperative queues.
    pub fn controlled_driver(mut self, driver: &ControlledPeerDriver) -> Self {
        self.controlled_driver = Some(driver.clone());
        self
    }

    pub fn network_thread(mut self, thread: NetworkThread) -> Self {
        self.network_thread = Some(thread);
        self
    }

    pub fn worker_thread(mut self, thread: WorkerThread) -> Self {
        self.worker_thread = Some(thread);
        self
    }

    pub fn signaling_thread(mut self, thread: SignalingThread) -> Self {
        self.signaling_thread = Some(thread);
        self
    }

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

    pub fn network_manager(mut self, provider: NetworkManagerProvider) -> Self {
        self.network_manager = Some(provider);
        self
    }

    /// Select a general factory. Products are created on the selected SDK
    /// network role with its actual incoming Environment. Distinct from the
    /// existing endpoint-backed packet_socket_factory selection.
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
    pub fn packet_socket_factory(mut self, provider: PacketSocketFactoryProvider) -> Self {
        self.packet_socket_factory = Some(provider);
        self
    }

    /// Use the native artifact's platform recording and playout device module
    /// instead of the default headless device. Core artifacts reject this at
    /// factory construction; native artifacts can still fail if platform
    /// device initialization is unavailable. No device is opened by default.
    #[cfg(feature = "native")]
    pub fn native_audio(mut self, enabled: bool) -> Self {
        #[cfg(target_os = "linux")]
        {
            self.audio_device_module = None;
        }
        self.native_audio = enabled;
        self
    }

    /// Create WebRTC's software audio processing module with the requested
    /// initial AEC, noise suppression and gain-control configuration. This
    /// affects decoded PCM, never directly supplied encoded Opus packets.
    /// Explicit Linux/native backend and actual SDK creation method. Construction
    /// occurs on the retained factory worker; nullable and Init errors are not
    /// replaced with a headless/default device. Does not start recording.
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn native_audio_device(mut self, config: crate::AudioDeviceCreationConfig) -> Self {
        self.audio_device_module = None;
        self.native_audio = true;
        self.native_audio_creation = config.ffi();
        self
    }
    /// Transfer the exact owning ADM and adopt its retained worker. Do not also
    /// supply a different worker. The SDK applies its ordinary Init/default
    /// device/stereo/processing options and replaces the audio transport; inspect
    /// or configure the actual installed module after construction.
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_module(mut self, module: crate::AudioDeviceModule) -> Self {
        self.native_audio = true;
        self.audio_device_module = Some(module);
        self
    }
    pub(crate) fn native_audio_creation_wire(
        mut self,
        creation: ffi::FfiNativeAudioDeviceCreation,
    ) -> Self {
        self.native_audio_creation = creation;
        self
    }
    /// Transfer this exact owning mixer, including retained source registrations,
    /// into the native media engine. No mutable standalone alias survives.
    pub fn audio_mixer(mut self, mixer: crate::AudioMixer) -> Self {
        self.audio_mixer = Some(mixer);
        self
    }
    /// Install this exact owning APM. No standalone mutable alias survives.
    /// Currently admitted for caller-driven headless PCM only. Native device
    /// callback failure/publication integration remains unqualified.
    pub fn audio_processor(mut self, processor: crate::AudioProcessingModule) -> Self {
        self.audio_processor = Some(processor);
        self.audio_processor_builder = None;
        self.audio_processing = None;
        self
    }
    /// Build with this factory's actual environment, then transfer that product.
    pub fn audio_processor_builder(mut self, builder: crate::BuiltinAudioProcessorBuilder) -> Self {
        self.audio_processor_builder = Some(builder);
        self.audio_processor = None;
        self.audio_processing = None;
        self
    }
    /// Last explicit processing selection wins.
    pub fn audio_processing(mut self, config: crate::AudioProcessingConfig) -> Self {
        self.audio_processor = None;
        self.audio_processor_builder = None;
        self.audio_processing = Some(config);
        self
    }

    pub fn audio_encoder_factory(mut self, factory: AudioEncoderFactory) -> Self {
        self.audio_encoder = Some(factory);
        self
    }

    pub fn audio_decoder_factory(mut self, factory: AudioDecoderFactory) -> Self {
        self.audio_decoder = Some(factory);
        self
    }

    pub fn video_encoder_provider(
        self,
        provider: impl crate::VideoEncoderFactoryProvider,
    ) -> Result<Self, crate::CodecError> {
        Ok(self.video_encoder_factory(VideoEncoderFactoryHandle::from_provider(provider)?))
    }
    pub fn video_decoder_provider(
        self,
        provider: impl crate::VideoDecoderFactoryProvider,
    ) -> Result<Self, crate::CodecError> {
        Ok(self.video_decoder_factory(VideoDecoderFactoryHandle::from_provider(provider)?))
    }
    pub fn video_encoder_factory(mut self, factory: VideoEncoderFactoryHandle) -> Self {
        self.video_encoder = Some(factory);
        self
    }

    pub fn video_decoder_factory(mut self, factory: VideoDecoderFactoryHandle) -> Self {
        self.video_decoder = Some(factory);
        self
    }
    pub fn build(mut self) -> Result<PeerConnectionFactory, PeerError> {
        let _turn = self
            .controlled_driver
            .as_ref()
            .map(ControlledPeerDriver::owner_turn);
        #[cfg(all(feature = "native", target_os = "linux"))]
        if self.audio_device_module.is_some() && self.worker_thread.is_some() {
            return Err(PeerError { kind: PeerErrorKind::AudioDeviceAdmission, message: "an owning ADM adopts its construction worker; a separate factory worker is incompatible".into() });
        }
        let has_audio_processing = self.audio_processing.is_some()
            || self.audio_processor.is_some()
            || self.audio_processor_builder.is_some();
        if self.native_audio
            && (self.audio_processor.is_some() || self.audio_processor_builder.is_some())
        {
            return Err(PeerError { kind: PeerErrorKind::AudioProcessingAdmission, message: "owning APM native-device integration is not yet qualified: physical capture ignores wrapper/callback failure outcomes; use caller-driven headless PCM".into() });
        }
        // Native media needs both video factory dependencies. An omitted side
        // keeps the stock default, independently of the explicitly selected
        // side. This is construction-time selection, never Create fallback.
        if self.video_encoder.is_some() && self.video_decoder.is_none() {
            self.video_decoder = Some(
                VideoDecoderFactoryHandle::builtin()
                    .map_err(|e| native_build_error(e.to_string()))?,
            );
        } else if self.video_decoder.is_some() && self.video_encoder.is_none() {
            self.video_encoder = Some(
                VideoEncoderFactoryHandle::builtin()
                    .map_err(|e| native_build_error(e.to_string()))?,
            );
        }
        if self.network_manager.is_some() && self.network_manager_factory.is_some() {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "endpoint network manager and manager factory are mutually exclusive"
                    .into(),
            });
        }
        if (self.network_manager.is_some() || self.network_manager_factory.is_some())
            && self.network_monitor_factory.is_some()
        {
            return Err(PeerError { kind: PeerErrorKind::InvalidParameter,
                message: "network manager and network monitor factory are mutually exclusive native dependencies".into() });
        }
        if self.packet_socket_factory.is_some() && self.socket_factory.is_some() {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "endpoint and general socket factories are mutually exclusive".into(),
            });
        }
        let controlled_socket_factory = self
            .socket_factory
            .as_ref()
            .is_some_and(crate::PacketSocketFactory::is_controlled);
        if self
            .socket_factory
            .as_ref()
            .is_some_and(crate::PacketSocketFactory::creation_failed)
        {
            return Err(PeerError {
                kind: PeerErrorKind::NativeConstruction,
                message: "socket factory capability query failed".into(),
            });
        }
        if self.socket_factory.is_some()
            && controlled_socket_factory != self.controlled_driver.is_some()
        {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "socket factory execution selection does not match the peer driver".into(),
            });
        }
        let controlled_manager_factory = self.controlled_driver.is_some()
            && self
                .network_manager_factory
                .as_ref()
                .is_some_and(crate::NetworkManagerFactory::is_controlled);
        if self.controlled_driver.is_some()
            && self.network_manager_factory.is_some()
            && !controlled_manager_factory
        {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "controlled peers require a caller-controlled network manager factory"
                    .into(),
            });
        }
        // Reserve before constructing any production queues or threads. This
        // also prevents a controlled world from being acquired concurrently.
        let production_lease = if self.controlled_driver.is_none() {
            let lease = ffi::new_production_lease();
            if lease.is_null() {
                return Err(PeerError {
                    kind: PeerErrorKind::InvalidState,
                    message: "production factories cannot coexist with controlled execution".into(),
                });
            }
            Some(lease)
        } else {
            None
        };
        if self.controlled_driver.is_some()
            && let (Some(manager), Some(sockets)) =
                (&self.network_manager, &self.packet_socket_factory)
            && (!manager.same_endpoint(sockets) || !sockets.supports_udp())
        {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "controlled network and packet providers must share an open endpoint"
                    .into(),
            });
        }
        if (self.native_audio || has_audio_processing)
            && self
                .audio_encoder
                .as_ref()
                .is_some_and(AudioEncoderFactory::supports_opus_frames)
        {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "Opus-frame factories cannot share a platform microphone or PCM audio processing module".into(),
            });
        }
        if self.network_predictor_factory.is_some() {
            return Err(PeerError { kind: PeerErrorKind::InvalidParameter, message: "the pinned SDK does not consume the standalone peer-factory predictor slot; select NetworkControllerFactory::goog_cc with GoogCcFactoryConfig and explicit RFC 8888 negotiation state".into() });
        }
        let environment = match self.environment {
            Some(environment) => environment,
            None => Environment::builder().build().map_err(native_build_error)?,
        };
        if self.network_controller_factory.is_some()
            && !ffi::network_controller_injection_enabled(environment.native())
        {
            return Err(PeerError { kind: PeerErrorKind::InvalidParameter, message: "injected network controllers require WebRTC-Bwe-InjectedCongestionController enabled in the factory environment".into() });
        }
        if let Some(driver) = &self.controlled_driver {
            if !driver.is_current()
                || !environment.uses_controlled_clock(driver.clock())
                || self.network_thread.is_some()
                || self.worker_thread.is_some()
                || self.signaling_thread.is_some()
                || !(controlled_manager_factory
                    || self
                        .network_manager
                        .as_ref()
                        .is_some_and(|provider| provider.is_controlled_by(driver)))
                || !(controlled_socket_factory
                    || self
                        .packet_socket_factory
                        .as_ref()
                        .is_some_and(|provider| provider.is_controlled_by(driver)))
            {
                return Err(PeerError {
                    kind: PeerErrorKind::InvalidParameter,
                    message: "controlled peers need their creator thread, matching cooperative clock/queues and packet providers, and no separate WebRTC threads".into(),
                });
            }
        } else if self
            .network_manager
            .as_ref()
            .is_some_and(NetworkManagerProvider::is_controlled)
            || self
                .packet_socket_factory
                .as_ref()
                .is_some_and(PacketSocketFactoryProvider::is_controlled)
        {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "controlled packet providers require a matching controlled peer driver"
                    .into(),
            });
        }
        let environment = match &self.controlled_driver {
            Some(driver) => environment
                .bind_driver(driver)
                .map_err(native_build_error)?,
            None => environment,
        };
        let network_thread = match (self.network_thread, &self.controlled_driver) {
            (Some(thread), _) => thread,
            (None, Some(driver)) => NetworkThread::from_native(driver.borrow_thread()),
            (None, None) => NetworkThread::start().map_err(native_build_error)?,
        };
        #[cfg(all(feature = "native", target_os = "linux"))]
        let (audio_device_module, device_worker, audio_device_environment) =
            match self.audio_device_module {
                Some(module) => {
                    let (native, worker, environment) = module.into_factory();
                    (native, Some(worker), Some(environment))
                }
                None => (cxx::UniquePtr::null(), None, None),
            };
        #[cfg(not(all(feature = "native", target_os = "linux")))]
        let (audio_device_module, device_worker, audio_device_environment) = (
            cxx::UniquePtr::<ffi::NativeAudioDevice>::null(),
            None::<WorkerThread>,
            None::<Environment>,
        );
        let worker_thread = match (
            self.worker_thread.or(device_worker),
            &self.controlled_driver,
        ) {
            (Some(thread), _) => thread,
            (None, Some(driver)) => WorkerThread::from_native(driver.borrow_thread()),
            (None, None) => WorkerThread::start().map_err(native_build_error)?,
        };
        let signaling_thread = match (self.signaling_thread, &self.controlled_driver) {
            (Some(thread), _) => thread,
            (None, Some(driver)) => SignalingThread::from_native(driver.borrow_thread()),
            (None, None) => SignalingThread::start().map_err(native_build_error)?,
        };
        let readiness = self
            .readiness
            .map(|state| ffi::new_readiness(Box::new(crate::readiness::RustReadiness(state))));
        let mut message = String::new();
        // SAFETY: native construction copies the readiness shared owner. Every
        // other non-null pointer is retained in FactoryInner and its
        // native factory is destroyed before those dependencies are dropped.
        let event_log_factory = self.event_log_factory.map(|factory| {
            ffi::new_rtc_event_log_factory(Box::new(crate::RustRtcEventLogFactory(
                Some(factory),
                environment.clone(),
            )))
        });
        let audio_processor = match (self.audio_processor, self.audio_processor_builder) {
            (Some(processor), _) => Some(processor),
            (_, Some(builder)) => Some(builder.build(&environment).map_err(|e| PeerError {
                kind: PeerErrorKind::AudioProcessingAdmission,
                message: e.to_string(),
            })?),
            _ => None,
        };
        let installed_audio_processor = audio_processor
            .map(crate::AudioProcessingModule::into_installed)
            .transpose()
            .map_err(|e| PeerError {
                kind: PeerErrorKind::AudioProcessingAdmission,
                message: e.to_string(),
            })?
            .unwrap_or_else(cxx::UniquePtr::null);
        let audio_mixer_failure = self
            .audio_mixer
            .as_ref()
            .map(crate::AudioMixer::failure_observer);
        let native = unsafe {
            ffi::new_peer_connection_factory(
                environment.native(),
                network_thread.native(),
                worker_thread.native(),
                signaling_thread.native(),
                optional_ptr(self.network_manager.as_ref().map(|value| value.native())),
                Box::new(crate::RustNetworkManagerFactory(
                    self.network_manager_factory,
                    environment.clone(),
                )),
                Box::new(crate::RustNetworkMonitorFactory(
                    self.network_monitor_factory,
                )),
                Box::new(crate::RustNetworkStatePredictorFactory(
                    self.network_predictor_factory,
                )),
                Box::new(crate::RustFecControllerFactory(
                    self.fec_factory,
                    Some(environment.clone()),
                )),
                Box::new(crate::RustNetworkControllerFactory(
                    self.network_controller_factory,
                    Some(environment.clone()),
                )),
                optional_ptr(
                    self.packet_socket_factory
                        .as_ref()
                        .map(|value| value.native()),
                ),
                Box::new(crate::RustPacketSocketFactory(
                    self.socket_factory,
                    environment.clone(),
                )),
                optional_ptr(self.audio_encoder.as_ref().map(|value| value.native())),
                optional_ptr(self.audio_decoder.as_ref().map(|value| value.native())),
                optional_ptr(self.video_encoder.as_ref().map(|value| value.native())),
                optional_ptr(self.video_decoder.as_ref().map(|value| value.native())),
                Box::new(crate::RustVideoFactoryEnvironment(environment.clone())),
                environment.uses_system_clock() && production_lease.is_some(),
                self.native_audio,
                &self.native_audio_creation,
                audio_device_module,
                self.event_logging,
                Box::new(crate::RustEventLogRetention {
                    _environment: environment.clone(),
                }),
                optional_ptr(
                    event_log_factory
                        .as_ref()
                        .and_then(|factory| factory.as_ref()),
                ),
                self.encode_metronome
                    .as_ref()
                    .map_or_else(crate::Metronome::absent, crate::Metronome::bridge),
                self.decode_metronome
                    .as_ref()
                    .map_or_else(crate::Metronome::absent, crate::Metronome::bridge),
                optional_ptr(readiness.as_ref().and_then(|value| value.as_ref())),
                crate::ExecutionResources::resolve(
                    self.execution_resources,
                    self.controlled_driver.is_some(),
                )
                .codec_core_budget_bits(),
                &self.audio_processing.map_or(
                    ffi::FfiAudioProcessingConfig {
                        enabled: false,
                        echo_cancellation: false,
                        noise_suppression: 0,
                        gain_control: 0,
                    },
                    crate::AudioProcessingConfig::ffi,
                ),
                self.audio_mixer
                    .map_or_else(cxx::UniquePtr::null, crate::AudioMixer::into_native),
                installed_audio_processor,
                &mut message,
            )
        };
        if native.is_null() {
            return Err(PeerError {
                kind: if message.starts_with("owning APM admission") {
                    PeerErrorKind::AudioProcessingAdmission
                } else if message.starts_with("owning ADM admission") {
                    PeerErrorKind::AudioDeviceAdmission
                } else {
                    PeerErrorKind::NativeConstruction
                },
                message,
            });
        }
        ffi::factory_set_options(
            native.as_ref().expect("validated peer factory"),
            &self.options.ffi(),
        );
        let inner = Rc::new(FactoryInner {
            native,
            _environment: environment,
            _audio_device_environment: audio_device_environment,
            _network_thread: network_thread,
            _worker_thread: worker_thread,
            _signaling_thread: signaling_thread,
            _controlled_driver: self.controlled_driver,
            audio_mixer_failure,
            _network_manager: self.network_manager,
            _packet_socket_factory: self.packet_socket_factory,
            _audio_encoder: self.audio_encoder,
            _audio_decoder: self.audio_decoder,
            _video_encoder: self.video_encoder,
            _video_decoder: self.video_decoder,
            _production_lease: production_lease,
        });
        if let Some(driver) = &inner._controlled_driver {
            driver.register_factory(&inner);
        }
        Ok(PeerConnectionFactory(inner))
    }
}

impl Default for PeerConnectionFactoryBuilder {
    fn default() -> Self {
        Self {
            environment: None,
            network_thread: None,
            worker_thread: None,
            signaling_thread: None,
            controlled_driver: None,
            execution_resources: None,
            network_manager: None,
            network_manager_factory: None,
            network_monitor_factory: None,
            network_predictor_factory: None,
            fec_factory: None,
            network_controller_factory: None,
            packet_socket_factory: None,
            socket_factory: None,
            audio_encoder: None,
            audio_decoder: None,
            video_encoder: None,
            video_decoder: None,
            native_audio: false,
            native_audio_creation: ffi::FfiNativeAudioDeviceCreation {
                layer: 0,
                bypass: false,
                helper: true,
            },
            #[cfg(all(feature = "native", target_os = "linux"))]
            audio_device_module: None,
            event_logging: false,
            event_log_factory: None,
            encode_metronome: None,
            decode_metronome: None,
            audio_processing: None,
            audio_processor: None,
            audio_processor_builder: None,
            audio_mixer: None,
            options: crate::FactoryOptions::default(),
            readiness: None,
        }
    }
}

pub(crate) struct FactoryInner {
    native: cxx::UniquePtr<ffi::NativePeerConnectionFactory>,
    _environment: Environment,
    _audio_device_environment: Option<Environment>,
    _network_thread: NetworkThread,
    _worker_thread: WorkerThread,
    _signaling_thread: SignalingThread,
    audio_mixer_failure: Option<crate::AudioMixerFailureObserver>,
    _network_manager: Option<NetworkManagerProvider>,
    _packet_socket_factory: Option<PacketSocketFactoryProvider>,
    _audio_encoder: Option<AudioEncoderFactory>,
    _audio_decoder: Option<AudioDecoderFactory>,
    _video_encoder: Option<VideoEncoderFactoryHandle>,
    _video_decoder: Option<VideoDecoderFactoryHandle>,
    // Keep the actual controlled role alive through retained provider drops.
    _controlled_driver: Option<ControlledPeerDriver>,
    // Last to drop, after native factory, role threads and retained providers.
    _production_lease: Option<cxx::UniquePtr<ffi::NativeProductionLease>>,
}

impl Drop for FactoryInner {
    fn drop(&mut self) {
        let _turn = self
            ._controlled_driver
            .as_ref()
            .map(ControlledPeerDriver::owner_turn);
        drop(std::mem::replace(&mut self.native, cxx::UniquePtr::null()));
    }
}
impl FactoryInner {
    pub(crate) fn native(&self) -> &ffi::NativePeerConnectionFactory {
        self.native.as_ref().unwrap()
    }
    pub(crate) fn uses_controlled_execution(&self) -> bool {
        self._controlled_driver.is_some()
    }

    pub(crate) fn check_control_call(&self) -> Result<(), PeerError> {
        if crate::provider_panic::callback_active() {
            return Err(PeerError { kind: PeerErrorKind::InvalidState, message: "cannot synchronously control the session from a native provider/context callback".into() });
        }
        if !ffi::factory_control_call_allowed(self.native.as_ref().expect("live peer factory")) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "this native role cannot synchronously control the session on signaling"
                    .into(),
            });
        }
        Ok(())
    }
    pub(crate) fn encoded_video_receive_only(&self) -> bool {
        self._video_decoder
            .as_ref()
            .is_some_and(VideoDecoderFactoryHandle::is_encoded_receive_only)
    }

    pub(crate) fn media_capture_time(&self) -> Option<std::time::Duration> {
        if self._controlled_driver.is_some() {
            self.controlled_time()
        } else if self._controlled_driver.is_none() && self._environment.uses_system_clock() {
            Some(self._environment.now())
        } else {
            // A custom non-controlled clock does not establish the required
            // mapping to WebRTC's process TimeMillis capture-clock domain.
            None
        }
    }

    pub(crate) fn controlled_time(&self) -> Option<std::time::Duration> {
        self._controlled_driver
            .as_ref()
            .map(|driver| driver.clock().now())
    }
}

/// A sequence-bound owner for modular WebRTC peer-connection construction.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::PeerConnectionFactory>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::PeerConnectionFactory>();
/// ```
pub struct PeerConnectionFactory(Rc<FactoryInner>);

impl PeerConnectionFactory {
    // Tests dispatch on these exact graph-owned roles, not a fresh lookalike
    // thread or an identity registry. No production/public API is introduced.
    #[cfg(test)]
    pub(crate) fn post_owned_role_for_test(
        &self,
        role: u8,
        task: impl FnOnce() + Send + 'static,
    ) -> bool {
        match role {
            0 => self.0._network_thread.post(task),
            1 => self.0._worker_thread.post(task),
            2 => self.0._signaling_thread.post(task),
            _ => panic!("invalid test role"),
        }
    }
    pub(crate) fn requires_deferred_teardown(&self) -> bool {
        ffi::factory_requires_deferred_teardown(self.native())
    }
    pub(crate) fn codec_owner(&self) -> Rc<FactoryInner> {
        self.0.clone()
    }
    pub(crate) fn codec_environment(&self) -> &Environment {
        &self.0._environment
    }
    pub(crate) fn codec_encoder_factory(&self) -> Option<&VideoEncoderFactoryHandle> {
        self.0._video_encoder.as_ref()
    }
    pub(crate) fn codec_decoder_factory(&self) -> Option<&VideoDecoderFactoryHandle> {
        self.0._video_decoder.as_ref()
    }
    pub(crate) fn check_control_call(&self) -> Result<(), PeerError> {
        self.0.check_control_call()
    }

    pub fn clock_timestamp(&self) -> crate::Timestamp {
        self.0._environment.timestamp()
    }

    /// Owned native field-trial view retained by this factory's environment.
    pub fn field_trials(&self) -> crate::FieldTrialsSnapshot {
        self.0._environment.field_trials()
    }

    fn native(&self) -> &ffi::NativePeerConnectionFactory {
        self.0.native.as_ref().expect("validated peer factory")
    }

    /// Entire synchronous SDK sender capabilities, with no live peer needed.
    /// Includes codecs, complete extension capabilities and FEC metadata.
    pub fn get_rtp_sender_capabilities(&self, kind: crate::MediaType) -> crate::RtpCapabilities {
        crate::RtpCapabilities::from_native(ffi::factory_rtp_capabilities_value(
            self.native(),
            kind.to_native(),
            true,
        ))
    }

    /// Entire synchronous SDK receiver capabilities through the signaling thread.
    /// Unknown/data kinds retain the concrete SDK's empty-value result.
    pub fn get_rtp_receiver_capabilities(&self, kind: crate::MediaType) -> crate::RtpCapabilities {
        crate::RtpCapabilities::from_native(ffi::factory_rtp_capabilities_value(
            self.native(),
            kind.to_native(),
            false,
        ))
    }

    /// First rejected installed encoder control or unsafe factory operation.
    /// The observer retains only the shared failure record and survives retirement.
    #[cfg(all(feature = "native", target_os = "linux"))]
    pub fn audio_device_module(
        &self,
    ) -> Result<crate::InstalledAudioDevice, crate::AudioDeviceError> {
        crate::InstalledAudioDevice::from_native(
            ffi::audio_device_from_factory(self.native()),
            self.0.clone(),
        )
    }
    pub fn audio_mixer_failure_observer(&self) -> Option<crate::AudioMixerFailureObserver> {
        self.0.audio_mixer_failure.clone()
    }
    pub fn audio_encoder_failure_observer(&self) -> crate::AudioEncoderSdkFailureObserver {
        crate::AudioEncoderSdkFailureObserver::from_native(
            ffi::factory_audio_encoder_failure_observer(self.native()),
        )
    }

    /// Read WebRTC's factory-wide audio processing state. Unknown status and
    /// platform availability are not proof that a device effect is active.
    /// Actual current config of the owning selection, not its constructor echo.
    /// Requires audio_processor or audio_processor_builder selection.
    pub fn audio_processor_config(
        &self,
    ) -> Result<crate::AudioProcessorConfig, crate::AudioProcessorError> {
        let mut error = 0;
        let config = ffi::factory_audio_processor_config(self.native(), &mut error);
        crate::AudioProcessingModule::admission(error)?;
        Ok(crate::AudioProcessorConfig::from_ffi(config))
    }
    pub fn apply_audio_processor_config(
        &self,
        config: crate::AudioProcessorConfig,
    ) -> Result<(), crate::AudioProcessorError> {
        crate::AudioProcessingModule::admission(ffi::factory_audio_processor_apply(
            self.native(),
            &config.ffi(),
        ))
    }
    /// Explicit caller render input on the same selected APM. Headless playout
    /// pulls do not call this automatically. Prefix/tail behavior matches the
    /// standalone i16 API; callback/admission failure commits no caller PCM.
    pub fn process_reverse_audio(
        &self,
        format: crate::AudioProcessingStream,
        samples: &mut [i16],
    ) -> Result<crate::AudioProcessingStatus, crate::AudioProcessorError> {
        if format.channels > u16::MAX as usize {
            return Err(crate::AudioProcessorError::InvalidDimensions);
        }
        let count = format.sample_count()?;
        if samples.len() < count {
            return Err(crate::AudioProcessorError::BufferTooShort);
        }
        let mut error = 0;
        let status = ffi::factory_audio_processor_reverse(
            self.native(),
            format.sample_rate_hz,
            format.channels,
            &mut samples[..count],
            &mut error,
        );
        crate::AudioProcessingModule::admission(error)?;
        Ok(crate::AudioProcessingStatus(status))
    }
    /// Caller-supplied capture/render delay, preserving native clamping/warnings.
    pub fn set_audio_processing_delay(
        &self,
        milliseconds: i32,
    ) -> Result<crate::AudioProcessingStatus, crate::AudioProcessorError> {
        let mut error = 0;
        let status = ffi::factory_audio_processor_delay(self.native(), milliseconds, &mut error);
        crate::AudioProcessingModule::admission(error)?;
        Ok(crate::AudioProcessingStatus(status))
    }
    pub fn try_audio_processing_state(
        &self,
    ) -> Result<crate::AudioProcessingState, crate::AudioProcessorError> {
        let state = self.audio_processing_state();
        match state.failure {
            Some(error) => Err(error),
            None => Ok(state),
        }
    }
    /// Inspect failure before interpreting component values, or use the
    /// fallible try_audio_processing_state projection.
    pub fn audio_processing_state(&self) -> crate::AudioProcessingState {
        ffi::factory_audio_processing_state(self.native()).into()
    }

    /// Create a native owning track container, not SDP routing or media sync.
    pub fn create_media_stream(&self, id: &str) -> Result<crate::LocalMediaStream, PeerError> {
        let native = ffi::create_media_stream(self.native(), id);
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::NativeConstruction,
                message: "native media stream creation failed".into(),
            });
        }
        Ok(crate::LocalMediaStream::from_native(native, self.0.clone()))
    }

    /// Enumerate input or output devices for this native-audio factory.
    /// No hardware is required: an empty list is a valid result.
    #[cfg(feature = "native")]
    pub fn audio_devices(&self, recording: bool) -> Result<Vec<crate::AudioDevice>, PeerError> {
        let mut devices = Vec::new();
        let mut error = String::new();
        if !ffi::factory_audio_devices(self.native(), recording, &mut devices, &mut error) {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: error,
            });
        }
        Ok(devices
            .into_iter()
            .map(|device| crate::AudioDevice {
                index: device.index,
                name: device.name,
                id: device.id,
            })
            .collect())
    }

    /// Select a currently enumerated recording or playout device. The native
    /// ADM decides whether switching is possible while audio is active.
    #[cfg(feature = "native")]
    pub fn select_audio_device(&self, recording: bool, index: u16) -> Result<(), PeerError> {
        let mut error = String::new();
        ffi::factory_select_audio_device(self.native(), recording, index, &mut error)
            .then_some(())
            .ok_or(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: error,
            })
    }

    /// Create a microphone track backed by the selected platform ADM. This
    /// requires a native-audio factory and does not use injected PCM.
    #[cfg(feature = "native")]
    pub fn create_microphone_track(&self, id: &str) -> Result<AudioTrack, PeerError> {
        if self.uses_opus_frames() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "Opus-frame factories cannot encode microphone PCM".into(),
            });
        }
        if id.is_empty() || id.as_bytes().contains(&0) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "invalid audio track id".into(),
            });
        }
        let native = ffi::create_microphone_track(self.native(), id);
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "platform microphone requires an available native audio device".into(),
            });
        }
        Ok(AudioTrack::microphone(native, self.0.clone()))
    }
    pub fn builder() -> PeerConnectionFactoryBuilder {
        PeerConnectionFactoryBuilder::default()
    }

    /// Replace the native options used for subsequently created peers.
    /// Existing peers retain the options copied at their construction. Native
    /// `SetOptions` is infallible and has no effective-state getter.
    pub fn set_options(&self, options: crate::FactoryOptions) {
        ffi::factory_set_options(self.native(), &options.ffi());
    }

    pub fn create_peer_connection(
        &self,
        configuration: PeerConfiguration,
    ) -> Result<PeerConnection, PeerError> {
        self.create_peer_connection_with_configuration(configuration.into())
    }

    /// Create a peer using the complete owned native configuration.
    pub fn create_peer_connection_with_configuration(
        &self,
        configuration: crate::RtcConfiguration,
    ) -> Result<PeerConnection, PeerError> {
        self.create_peer_connection_with_dependencies(
            configuration,
            crate::PeerConnectionDependencies::default(),
        )
    }

    /// Create a peer with owned executable dependencies. Per-peer field trials
    /// override only this peer's environment; factory defaults remain intact.
    pub fn create_peer_connection_with_dependencies(
        &self,
        configuration: crate::RtcConfiguration,
        dependencies: crate::PeerConnectionDependencies,
    ) -> Result<PeerConnection, PeerError> {
        let _turn = self
            .0
            ._controlled_driver
            .as_ref()
            .map(ControlledPeerDriver::owner_turn);
        if configuration.turn_tls_ca_pem.as_deref() == Some("") {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "TURN TLS CA certificate cannot be empty".into(),
            });
        }
        let config = configuration.to_native();
        let controlled_id = self
            .0
            ._controlled_driver
            .as_ref()
            .map(|driver| {
                driver.allocate_peer_id().ok_or_else(|| PeerError {
                    kind: PeerErrorKind::InvalidState,
                    message: "controlled peer identifier exhausted".into(),
                })
            })
            .transpose()?;
        let mut message = String::new();
        let mut native_error_kind = 10;
        let native = ffi::create_peer_connection(
            self.0.native.as_ref().expect("validated peer factory"),
            config.as_ref().expect("owned native configuration"),
            Box::new(crate::RustFieldTrials(dependencies.field_trials)),
            crate::RustTlsCertificateVerifier::new(dependencies.tls_certificate_verifier),
            crate::RustLocalNetworkPermissionFactory::new(
                dependencies.local_network_permission_factory,
            ),
            Box::new(crate::RustCertificateGenerator(
                dependencies.certificate_generator,
            )),
            Box::new(crate::RustAsyncDnsFactory(
                dependencies.async_dns_resolver_factory,
            )),
            Box::new(crate::RustVideoBitrateAllocatorFactory(
                dependencies.video_bitrate_allocator_factory,
                Some(self.0._environment.clone()),
            )),
            &mut native_error_kind,
            &mut message,
        );
        if native.is_null() {
            Err(PeerError {
                kind: error_kind(native_error_kind),
                message,
            })
        } else {
            let inner = Rc::new(PeerInner {
                native,
                _factory: self.0.clone(),
                controlled_id,
                closed: Cell::new(false),
                pending_legacy_selector: RefCell::new(None),
                local_streams: RefCell::new(Vec::new()),
                stream_inputs: RefCell::new(HashMap::new()),
                sender_tracks: RefCell::new(HashMap::new()),
                audio_sender_tracks: RefCell::new(HashMap::new()),
                sender_observers: RefCell::new(HashMap::new()),
                receiver_observers: RefCell::new(HashMap::new()),
                dtmf_senders: RefCell::new(HashMap::new()),
                dtls_transports: RefCell::new(HashMap::new()),
                sctp_transports: RefCell::new(HashMap::new()),
            });
            if let Some(driver) = &self.0._controlled_driver {
                driver.register_peer(&inner);
            }
            Ok(PeerConnection {
                inner,
                next_operation_id: Cell::new(1),
            })
        }
    }

    pub(crate) fn owns_video_source(&self, source: &VideoSource) -> bool {
        source.belongs_to(&self.0)
    }

    pub(crate) fn uses_video_encoder(&self, encoder: &VideoEncoderFactoryHandle) -> bool {
        self.0
            ._video_encoder
            .as_ref()
            .is_some_and(|selected| selected.same_provider(encoder))
    }

    /// Create a real SDK stream. IDs preserve bytes, including embedded NUL.
    pub fn create_local_media_stream(&self, id: &[u8]) -> Result<crate::MediaStream, PeerError> {
        let native = ffi::peer_factory_create_stream(self.0.native(), id);
        if native.is_null() {
            return Err(native_build_error("failed to create media stream"));
        }
        Ok(crate::MediaStream::from_native(native, self.0.clone()))
    }

    pub fn create_audio_source(&self) -> Result<AudioSource, PeerError> {
        if self.uses_opus_frames() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "Opus-frame factories cannot encode raw PCM; use a separate peer factory"
                    .into(),
            });
        }
        self.create_audio_source_inner()
    }

    fn create_audio_source_inner(&self) -> Result<AudioSource, PeerError> {
        let mut failure = 0;
        let native = ffi::create_audio_source(
            self.0.native.as_ref().expect("validated peer factory"),
            &mut failure,
        );
        if failure == 1 {
            Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "custom PCM cannot share a processing capture thread with native devices; use a microphone track or a separate headless factory".into(),
            })
        } else if native.is_null() {
            Err(native_build_error("failed to create audio source"))
        } else {
            Ok(AudioSource::from_native(native, self.0.clone()))
        }
    }

    /// Create an Opus source with its own delivery/stream identity. `channels`
    /// must match the Opus packets and the negotiated mono/stereo format.
    pub fn create_encoded_audio_source(
        &self,
        channels: u8,
    ) -> Result<EncodedAudioSource, PeerError> {
        if !self.uses_opus_frames() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "Opus-frame input requires an Opus-frame encoder factory".into(),
            });
        }
        if !matches!(channels, 1 | 2) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "encoded Opus channels must be 1 or 2".into(),
            });
        }
        let native = ffi::create_encoded_audio_source(
            self.0.native.as_ref().expect("validated peer factory"),
            channels,
        );
        if native.is_null() {
            Err(native_build_error("failed to create encoded audio source"))
        } else {
            Ok(EncodedAudioSource::from_source(
                AudioSource::from_native(native, self.0.clone()),
                channels,
            ))
        }
    }

    fn uses_opus_frames(&self) -> bool {
        self.0
            ._audio_encoder
            .as_ref()
            .is_some_and(AudioEncoderFactory::supports_opus_frames)
    }

    pub fn create_encoded_audio_track(
        &self,
        id: &str,
        source: &EncodedAudioSource,
    ) -> Result<AudioTrack, PeerError> {
        self.create_audio_track(id, source.source())
            .map(AudioTrack::mark_opus_frames)
    }

    pub fn create_audio_track(
        &self,
        id: &str,
        source: &AudioSource,
    ) -> Result<AudioTrack, PeerError> {
        if id.is_empty() || !source.belongs_to(&self.0) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "audio track requires a nonempty id and a source from this factory".into(),
            });
        }
        let native = ffi::create_audio_track(
            self.0.native.as_ref().expect("validated peer factory"),
            source.native(),
            id,
        );
        if native.is_null() {
            Err(native_build_error("failed to create audio track"))
        } else {
            Ok(AudioTrack::local(native, self.0.clone(), source))
        }
    }

    /// Enumerate displays on supported native artifacts. No display server is
    /// required by CI, and an empty list is valid on a headless host.
    #[cfg(feature = "native")]
    pub fn screen_sources(&self) -> Result<Vec<crate::ScreenSource>, PeerError> {
        self.desktop_sources(false)
    }

    /// Enumerate windows on X11 or ask the system portal to select a target
    /// on Wayland. Portal placeholder IDs are not unrestricted window lists.
    #[cfg(feature = "native")]
    pub fn window_sources(&self) -> Result<Vec<crate::WindowSource>, PeerError> {
        self.desktop_sources(true)
    }

    #[cfg(feature = "native")]
    fn desktop_sources(&self, window: bool) -> Result<Vec<crate::ScreenSource>, PeerError> {
        self.require_native_capture()?;
        let mut screens = Vec::new();
        let mut error = String::new();
        if !ffi::screen_sources(window, &mut screens, &mut error) {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: error,
            });
        }
        Ok(screens
            .into_iter()
            .map(|screen| crate::ScreenSource {
                id: screen.id,
                name: screen.name,
                display_id: screen.display_id,
            })
            .collect())
    }

    /// Construct real SDK options. defaults initializes native platform
    /// resources; false is the empty SDK constructor. selected_environment
    /// chooses this factory's actual Environment rather than the SDK global clock.
    #[cfg(feature = "native")]
    pub fn desktop_capture_options(
        &self,
        defaults: bool,
        selected_environment: bool,
    ) -> Result<crate::DesktopCaptureOptions, PeerError> {
        self.require_native_capture()?;
        let native = ffi::desktop_options_new(
            self.native(),
            self.0._environment.native(),
            defaults,
            selected_environment,
        );
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "SDK desktop options unavailable".into(),
            });
        }
        let clock = if selected_environment {
            self.0._environment.clock().map_err(|e| PeerError {
                kind: PeerErrorKind::InvalidState,
                message: e.to_string(),
            })?
        } else {
            crate::Clock::real_time().map_err(|e| PeerError {
                kind: PeerErrorKind::InvalidState,
                message: e.to_string(),
            })?
        };
        Ok(crate::DesktopCaptureOptions::new(
            native,
            self.0.clone(),
            clock,
        ))
    }
    /// Use the selected SDK screen/window/generic factory without fallback or
    /// source-list filtering. None preserves the native unselected/default
    /// state; Some is forwarded even for SDK restore-token identities.
    #[cfg(feature = "native")]
    pub fn create_desktop_capture(
        &self,
        options: &crate::DesktopCaptureOptions,
        kind: crate::DesktopCaptureKind,
        source_id: Option<i64>,
    ) -> Result<crate::ScreenCapture, PeerError> {
        self.create_desktop_capture_with_callbacks(options, kind, source_id, None, None)
    }
    /// Install the actual SDK callback and delegated observer before selection
    /// and Start, so synchronous initialization notifications are not missed.
    #[cfg(feature = "native")]
    pub fn create_desktop_capture_with_callbacks(
        &self,
        options: &crate::DesktopCaptureOptions,
        kind: crate::DesktopCaptureKind,
        source_id: Option<i64>,
        receiver: Option<&crate::DesktopCaptureReceiverHandle>,
        observer: Option<&crate::DesktopSelectionObserverHandle>,
    ) -> Result<crate::ScreenCapture, PeerError> {
        self.create_desktop_capture_with_providers(
            options, kind, source_id, receiver, observer, None,
        )
    }
    /// A selected memory factory is rejected before source creation: the pinned
    /// Linux SDK capturers inherit the unsupported/no-op setter. Other callback
    /// registrations are installed before native selection and Start.
    #[cfg(feature = "native")]
    pub fn create_desktop_capture_with_providers(
        &self,
        options: &crate::DesktopCaptureOptions,
        kind: crate::DesktopCaptureKind,
        source_id: Option<i64>,
        receiver: Option<&crate::DesktopCaptureReceiverHandle>,
        observer: Option<&crate::DesktopSelectionObserverHandle>,
        memory_factory: Option<&crate::DesktopMemoryFactoryHandle>,
    ) -> Result<crate::ScreenCapture, PeerError> {
        self.require_native_capture()?;
        options.compatible(&self.0)?;
        if memory_factory.is_some() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "pinned Linux capturers do not consume SharedMemoryFactory".into(),
            });
        }
        let source = self.create_video_source()?;
        let mut message = String::new();
        let native = ffi::create_desktop_capture(
            source.native(),
            options.value(),
            kind.tag(),
            source_id.is_some(),
            source_id.unwrap_or_default(),
            crate::desktop_receiver::RustDesktopCaptureReceiver::new(receiver),
            crate::desktop_receiver::RustDesktopSelectionObserver::new(observer),
            &mut message,
        );
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::Operation,
                message,
            });
        }
        Ok(crate::ScreenCapture::new(native, source))
    }
    /// Select a display for caller-paced capture into a regular video track.
    #[cfg(feature = "native")]
    pub fn open_screen(&self, screen_id: i64) -> Result<crate::ScreenCapture, PeerError> {
        self.open_desktop(screen_id, false)
    }

    /// Select a window for caller-paced capture into a regular video track.
    /// The system portal owns the consent UI on Wayland.
    #[cfg(feature = "native")]
    pub fn open_window(&self, window_id: i64) -> Result<crate::WindowCapture, PeerError> {
        self.open_desktop(window_id, true)
    }

    #[cfg(feature = "native")]
    fn open_desktop(&self, id: i64, window: bool) -> Result<crate::ScreenCapture, PeerError> {
        self.require_native_capture()?;
        let source = self.create_video_source()?;
        let mut error = String::new();
        let native = ffi::open_screen(source.native(), id, window, &mut error);
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: error,
            });
        }
        Ok(crate::ScreenCapture::new(native, source))
    }

    /// Create the real empty SDK camera-options value. Its Linux backend flags
    /// initially remain false; no default device or session is initialized.
    #[cfg(feature = "native")]
    pub fn create_camera_options(&self) -> Result<crate::CameraCaptureOptions, PeerError> {
        self.require_native_capture()?;
        let native = ffi::camera_options_new(self.native());
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "SDK camera options unavailable".into(),
            });
        }
        Ok(crate::CameraCaptureOptions::new(native, self.0.clone()))
    }

    /// Create a stable SDK DeviceInfo product. A native null result stays
    /// None, distinct from a live product reporting zero devices.
    #[cfg(feature = "native")]
    pub fn create_camera_device_info(&self) -> Result<Option<crate::CameraDeviceInfo>, PeerError> {
        self.require_native_capture()?;
        let native = ffi::camera_info_new(self.native());
        Ok((!native.is_null()).then(|| crate::CameraDeviceInfo::new(native, self.0.clone())))
    }
    /// Use the SDK options-taking DeviceInfo overload, retaining an independent
    /// options copy through the product's final native release. Native null is None.
    #[cfg(feature = "native")]
    pub fn create_camera_device_info_with_options(
        &self,
        options: &crate::CameraCaptureOptions,
    ) -> Result<Option<crate::CameraDeviceInfo>, PeerError> {
        self.require_native_capture()?;
        options.consumable(&self.0)?;
        let selected = options.try_clone()?;
        let native = ffi::camera_info_new_with_options(self.native(), selected.value());
        Ok((!native.is_null())
            .then(|| crate::CameraDeviceInfo::new(native, self.0.clone()).with_options(selected)))
    }
    /// List physical cameras on supported native artifacts. Empty is valid
    /// and does not indicate an error on a headless host.
    #[cfg(feature = "native")]
    pub fn camera_devices(&self) -> Result<Vec<crate::CameraDevice>, PeerError> {
        self.require_native_capture()?;
        let mut devices = Vec::new();
        let mut error = String::new();
        if !ffi::camera_devices(&mut devices, &mut error) {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: error,
            });
        }
        Ok(devices
            .into_iter()
            .map(|device| crate::CameraDevice {
                name: device.name,
                id: device.id,
                product_id: device.product_id,
            })
            .collect())
    }

    /// Complete capabilities without filtering signed/default/interlace fields.
    #[cfg(feature = "native")]
    pub fn camera_capabilities(
        &self,
        device_id: &str,
    ) -> Result<Vec<crate::CameraSettings>, crate::CameraControlError> {
        self.require_native_capture()
            .map_err(crate::CameraControlError::Binding)?;
        let reply =
            ffi::camera_device_control(device_id, 0, &crate::CameraSettings::default().native());
        crate::CameraControlError::check(reply.status, reply.native_result)?;
        Ok(reply
            .capabilities
            .into_iter()
            .map(crate::CameraSettings::from_native)
            .collect())
    }
    #[cfg(feature = "native")]
    pub fn camera_orientation(
        &self,
        device_id: &str,
    ) -> Result<crate::VideoRotation, crate::CameraControlError> {
        self.require_native_capture()
            .map_err(crate::CameraControlError::Binding)?;
        let reply =
            ffi::camera_device_control(device_id, 1, &crate::CameraSettings::default().native());
        crate::CameraControlError::check(reply.status, reply.native_result)?;
        Ok(crate::VideoRotation::from_degrees(reply.orientation))
    }
    /// Native matched capability index plus complete returned settings.
    #[cfg(feature = "native")]
    pub fn best_camera_capability(
        &self,
        device_id: &str,
        requested: &crate::CameraSettings,
    ) -> Result<(i32, crate::CameraSettings), crate::CameraControlError> {
        self.require_native_capture()
            .map_err(crate::CameraControlError::Binding)?;
        let reply = ffi::camera_device_control(device_id, 2, &requested.native());
        crate::CameraControlError::check(reply.status, reply.native_result)?;
        let value = reply
            .capabilities
            .into_iter()
            .next()
            .expect("successful native capability match");
        Ok((
            reply.matched_index,
            crate::CameraSettings::from_native(value),
        ))
    }
    /// Enumerate formats advertised by a camera. Refresh after device changes.
    #[cfg(feature = "native")]
    pub fn camera_formats(&self, device_id: &str) -> Result<Vec<crate::CameraFormat>, PeerError> {
        self.require_native_capture()?;
        if device_id.is_empty() || device_id.contains('\0') {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "invalid camera device identifier".into(),
            });
        }
        let mut formats = Vec::new();
        let mut error = String::new();
        if !ffi::camera_formats(device_id, &mut formats, &mut error) {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: error,
            });
        }
        Ok(formats
            .into_iter()
            .map(|format| crate::CameraFormat {
                width: format.width,
                height: format.height,
                max_fps: format.max_fps,
                pixel_format: crate::CameraPixelFormat::from_native(format.pixel_format),
            })
            .collect())
    }

    /// Create a native camera without starting it or selecting a substitute
    /// capability. Use start_device with complete CameraSettings explicitly.
    #[cfg(feature = "native")]
    pub fn create_camera(&self, device_id: &str) -> Result<crate::CameraCapture, PeerError> {
        self.require_native_capture()?;
        let source = self.create_video_source()?;
        let mut message = String::new();
        let native = ffi::create_camera(source.native(), device_id, &mut message);
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::Operation,
                message,
            });
        }
        Ok(crate::CameraCapture::new(native, source))
    }
    /// Use SDK Create(options, id), without starting or choosing a capability.
    /// The selected options and descriptor lease outlive their original owner.
    #[cfg(feature = "native")]
    pub fn create_camera_with_options(
        &self,
        options: &crate::CameraCaptureOptions,
        device_id: &str,
    ) -> Result<crate::CameraCapture, PeerError> {
        self.require_native_capture()?;
        options.consumable(&self.0)?;
        let selected = options.try_clone()?;
        let source = self.create_video_source()?;
        let mut message = String::new();
        let native = ffi::create_camera_with_options(
            source.native(),
            selected.value(),
            device_id,
            &mut message,
        );
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::Operation,
                message,
            });
        }
        Ok(crate::CameraCapture::new(native, source).with_options(selected))
    }
    /// Open a selected camera into a regular local video source. Camera
    /// start/stop are explicit; no physical camera is required for CI.
    #[cfg(feature = "native")]
    pub fn open_camera(
        &self,
        device_id: &str,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<crate::CameraCapture, PeerError> {
        self.require_native_capture()?;
        if device_id.is_empty()
            || device_id.as_bytes().contains(&0)
            || width == 0
            || width > 4096
            || height == 0
            || height > 4096
            || fps == 0
            || fps > 120
        {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "invalid camera device id or capture settings".into(),
            });
        }
        let source = self.create_video_source()?;
        let mut error = String::new();
        let native = ffi::open_camera(source.native(), device_id, width, height, fps, &mut error);
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: error,
            });
        }
        Ok(crate::CameraCapture::new(native, source))
    }

    // These methods explicitly select a physical backend. The caller accepts
    // its external effects, regardless of the surrounding runtime selection.
    #[cfg(feature = "native")]
    fn require_native_capture(&self) -> Result<(), PeerError> {
        self.check_control_call()
    }

    pub fn create_video_source(&self) -> Result<VideoSource, PeerError> {
        let native =
            ffi::create_video_source(self.0.native.as_ref().expect("validated peer factory"));
        if native.is_null() {
            Err(native_build_error("failed to create video source"))
        } else {
            Ok(VideoSource::from_native(native, self.0.clone()))
        }
    }

    pub fn create_video_track(
        &self,
        id: &str,
        source: &VideoSource,
    ) -> Result<VideoTrack, PeerError> {
        if id.is_empty() || !source.belongs_to(&self.0) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "video track requires a nonempty id and a source from this factory".into(),
            });
        }
        let native = ffi::create_video_track(
            self.0.native.as_ref().expect("validated peer factory"),
            source.native(),
            id,
        );
        if native.is_null() {
            Err(native_build_error("failed to create video track"))
        } else {
            Ok(VideoTrack::local(native, self.0.clone(), source))
        }
    }
}

/// A sequence-bound peer connection with a caller-polled owned event queue.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::PeerConnection>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::PeerConnection>();
/// ```
pub struct PeerConnection {
    pub(crate) inner: Rc<PeerInner>,
    next_operation_id: Cell<u64>,
}

pub(crate) struct SenderTrackLease<T> {
    // Keep the actual SDK sender alive so its address cannot be reused as a
    // key while this lease remains cached. No Rust peer edge creates a cycle.
    _sender: cxx::UniquePtr<ffi::NativeRtpSender>,
    pub(crate) track: T,
}
impl<T> SenderTrackLease<T> {
    pub(crate) fn new(sender: &RtpSender, track: T) -> Self {
        Self {
            _sender: ffi::rtp_sender_copy(sender.native()),
            track,
        }
    }
}

pub(crate) struct PeerInner {
    native: cxx::UniquePtr<ffi::NativePeerConnection>,
    pub(crate) _factory: Rc<FactoryInner>,
    pub(crate) controlled_id: Option<u64>,
    pub(crate) closed: Cell<bool>,
    pending_legacy_selector: RefCell<Option<(OperationId, crate::MediaTrack)>>,
    local_streams: RefCell<Vec<crate::MediaStream>>,
    pub(crate) stream_inputs: RefCell<HashMap<usize, SenderTrackLease<crate::MediaTrack>>>,
    pub(crate) sender_tracks: RefCell<HashMap<usize, SenderTrackLease<VideoTrack>>>,
    pub(crate) audio_sender_tracks: RefCell<HashMap<usize, SenderTrackLease<AudioTrack>>>,
    pub(crate) sender_observers:
        RefCell<HashMap<usize, std::rc::Weak<crate::rtp_observers::SenderObserverInner>>>,
    pub(crate) receiver_observers:
        RefCell<HashMap<usize, std::rc::Weak<crate::rtp_observers::ReceiverObserverInner>>>,
    pub(crate) dtmf_senders: RefCell<HashMap<usize, std::rc::Weak<crate::dtmf::DtmfInner>>>,
    pub(crate) dtls_transports: RefCell<HashMap<usize, std::rc::Weak<crate::transport::DtlsInner>>>,
    pub(crate) sctp_transports: RefCell<HashMap<usize, std::rc::Weak<crate::transport::SctpInner>>>,
}

impl Drop for PeerInner {
    fn drop(&mut self) {
        let _turn = self
            ._factory
            ._controlled_driver
            .as_ref()
            .map(ControlledPeerDriver::owner_turn);
        self.closed.set(true);
        drop(std::mem::replace(&mut self.native, cxx::UniquePtr::null()));
    }
}

/// Binding admission diagnostics for the actual Call owned by this peer.
/// Counts are saturating and are not native RTCStats packet counters. A closed
/// Call retains its observations; a peer with no created Call returns None.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallAdmissionStats {
    pub live: bool,
    /// Durable bounds retained from the exact native Call creation clock.
    /// None means its identity/bounds could not be established, not a sample.
    pub clock_time_bounds: Option<crate::ClockTimeBounds>,
    /// RTP rejected for a nonfinite arrival before SDK delivery.
    /// Control/RTCP packets are not counted. Finite values are passed unchanged;
    /// internal clock/history arithmetic remains libwebrtc's responsibility.
    pub rtp_arrival_rejections: u64,
}

impl PeerConnection {
    /// Invoke native reconfiguration. The SDK posts application to its worker
    /// role and may restart estimation. Success is not proof of emitted probes.
    pub fn reconfigure_bandwidth_estimation(
        &self,
        settings: BandwidthEstimationSettings,
    ) -> Result<(), PeerError> {
        if ffi::peer_reconfigure_bandwidth_estimation(
            self.native(),
            settings.allow_probe_without_media,
        ) {
            Ok(())
        } else {
            Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            })
        }
    }
    /// Install/replace the owning SDK peer-wide message observer. This does
    /// not replace channel observers or provide message-consumption credit.
    /// Disable fences network replacement from ordinary callers. Caller-owned
    /// controlled roles must run their own posted SDK task. Calls from a native
    /// network callback are rejected to avoid reversing a blocking invoke edge.
    pub fn set_data_channel_message_observation(&self, enabled: bool) -> Result<(), PeerError> {
        match ffi::peer_observe_channel_messages(self.native(), enabled) {
            0 => Ok(()),
            1 => Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            }),
            2 => Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "observer replacement cannot block its own SDK network callback".into(),
            }),
            _ => unreachable!("native channel observer admission"),
        }
    }
    fn stream_mutation(
        &self,
        stream: &crate::MediaStream,
        remove: bool,
    ) -> Result<bool, PeerError> {
        if !Rc::ptr_eq(stream.factory(), &self.inner._factory) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "stream must belong to this peer factory".into(),
            });
        }
        match ffi::peer_mutate_stream(self.native(), stream.native(), remove) {
            0 => {
                if !remove {
                    stream.register_peer(&self.inner);
                    self.inner.local_streams.borrow_mut().push(stream.clone());
                }
                self.inner.refresh_stream_inputs();
                if remove {
                    let actual = self.local_streams()?;
                    self.inner
                        .local_streams
                        .borrow_mut()
                        .retain(|s| actual.iter().any(|v| v.same_resource(s)));
                }
                Ok(true)
            }
            1 => Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            }),
            2 => Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "legacy stream operations require native Plan B semantics".into(),
            }),
            3 => Ok(false),
            _ => unreachable!("native stream mutation status"),
        }
    }
    /// Native Plan B AddStream acceptance, not proof of negotiation or media.
    pub fn add_stream(&self, stream: &crate::MediaStream) -> Result<bool, PeerError> {
        self.stream_mutation(stream, false)
    }
    /// Invoke native Plan B RemoveStream. The SDK has no removal result.
    pub fn remove_stream(&self, stream: &crate::MediaStream) -> Result<(), PeerError> {
        self.stream_mutation(stream, true).map(|_| ())
    }
    fn streams(&self, remote: bool) -> Result<Vec<crate::MediaStream>, PeerError> {
        let list = ffi::peer_streams(self.native(), remote);
        if list.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "legacy stream collections require native Plan B semantics".into(),
            });
        }
        Ok((0..ffi::media_streams_len(&list))
            .map(|i| {
                let stream = crate::MediaStream::from_native(
                    ffi::media_streams_get(&list, i),
                    self.inner._factory.clone(),
                );
                self.inner
                    .local_streams
                    .borrow()
                    .iter()
                    .find(|s| s.same_resource(&stream))
                    .cloned()
                    .unwrap_or(stream)
            })
            .collect())
    }
    pub fn local_streams(&self) -> Result<Vec<crate::MediaStream>, PeerError> {
        self.streams(false)
    }
    pub fn remote_streams(&self) -> Result<Vec<crate::MediaStream>, PeerError> {
        self.streams(true)
    }

    pub fn call_admission_stats(&self) -> Option<CallAdmissionStats> {
        let value = ffi::peer_call_admission_stats(self.inner.native());
        let bounds = value.clock_bounds;
        let clock_time_bounds = if bounds.known {
            crate::FiniteTimestamp::from_microseconds(bounds.minimum_us)
                .ok()
                .zip(crate::FiniteTimestamp::from_microseconds(bounds.maximum_us).ok())
                .and_then(|(minimum, maximum)| crate::ClockTimeBounds::new(minimum, maximum).ok())
        } else {
            None
        };
        value.has_call.then_some(CallAdmissionStats {
            live: value.live,
            clock_time_bounds,
            rtp_arrival_rejections: value.rtp_arrival_rejections,
        })
    }
    /// Native owning SCTP transport, absent before one is created. Repeated
    /// getters and its DTLS associations share native observer registrations.
    /// Native MID lookup. No transport is guessed from a sender or SDP parse.
    pub fn dtls_transport_by_mid(&self, mid: &[u8]) -> Option<crate::DtlsTransport> {
        crate::DtlsTransport::from_native(
            ffi::peer_dtls_transport_by_mid(self.native(), mid),
            self.inner.clone(),
        )
    }

    pub fn sctp_transport(&self) -> Option<crate::SctpTransport> {
        crate::SctpTransport::from_native(
            ffi::peer_sctp_transport(self.inner.native()),
            self.inner.clone(),
        )
    }

    /// Owned effective native configuration, including retained certificates.
    pub fn configuration(&self) -> crate::RtcConfiguration {
        let native = ffi::peer_get_configuration(self.inner.native());
        crate::RtcConfiguration::from_native(
            native.as_ref().expect("native configuration snapshot"),
        )
    }

    /// Apply native SetConfiguration semantics. Unsupported changes return
    /// InvalidModification. Failed updates do not change the effective config.
    pub fn set_configuration(&self, config: crate::RtcConfiguration) -> Result<(), PeerError> {
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        if config.turn_tls_ca_pem.as_deref() == Some("") {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "TURN TLS CA certificate cannot be empty".into(),
            });
        }
        let native = config.to_native();
        let mut message = String::new();
        let status = ffi::peer_set_configuration(
            self.inner.native(),
            native.as_ref().expect("native configuration"),
            &mut message,
        );
        if status == 0 {
            Ok(())
        } else {
            Err(PeerError {
                kind: error_kind(status),
                message,
            })
        }
    }

    /// Stable identifier within the controlled world, allocated in peer
    /// creation order. Owned media outputs carry this recipient identity.
    /// Ordinary production peers have no controlled identity.
    pub fn controlled_id(&self) -> Option<u64> {
        self.inner.controlled_id
    }

    /// Audio RTP codecs available for sending on this peer.
    pub fn audio_sender_capabilities(&self) -> Result<Vec<AudioCodecCapability>, PeerError> {
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        Ok(
            ffi::peer_audio_codec_capabilities(self.inner.factory_native(), true)
                .into_iter()
                .map(AudioCodecCapability::from_ffi)
                .collect(),
        )
    }

    /// Audio RTP codecs available for receiving on this peer.
    pub fn audio_receiver_capabilities(&self) -> Result<Vec<AudioCodecCapability>, PeerError> {
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        Ok(
            ffi::peer_audio_codec_capabilities(self.inner.factory_native(), false)
                .into_iter()
                .map(AudioCodecCapability::from_ffi)
                .collect(),
        )
    }

    /// Video RTP codecs available for sending on this peer (including RTX/RED when supported).
    pub fn video_sender_capabilities(&self) -> Result<Vec<VideoCodecCapability>, PeerError> {
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        ffi::peer_video_codec_capabilities(self.inner.factory_native(), true)
            .into_iter()
            .map(VideoCodecCapability::from_ffi)
            .collect()
    }

    /// Video RTP codecs available for receiving on this peer.
    pub fn video_receiver_capabilities(&self) -> Result<Vec<VideoCodecCapability>, PeerError> {
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        ffi::peer_video_codec_capabilities(self.inner.factory_native(), false)
            .into_iter()
            .map(VideoCodecCapability::from_ffi)
            .collect()
    }

    /// Returns a stable snapshot of this peer's video transceivers.
    pub fn video_transceivers(&self) -> Result<Vec<RtpTransceiver>, PeerError> {
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        Ok(self
            .transceivers()?
            .into_iter()
            .filter(|v| v.media_type() == crate::MediaType::Video)
            .collect())
    }

    /// Returns video sender handles for the current transceiver snapshot.
    pub fn video_senders(&self) -> Result<Vec<RtpSender>, PeerError> {
        Ok(self
            .video_transceivers()?
            .into_iter()
            .map(|transceiver| transceiver.sender())
            .collect())
    }

    /// Returns the current audio transceiver snapshot.
    pub fn audio_transceivers(&self) -> Result<Vec<RtpTransceiver>, PeerError> {
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        Ok(self
            .transceivers()?
            .into_iter()
            .filter(|v| v.media_type() == crate::MediaType::Audio)
            .collect())
    }

    /// Returns audio receiver handles for the current transceiver snapshot.
    pub fn audio_receivers(&self) -> Result<Vec<RtpReceiver>, PeerError> {
        Ok(self
            .audio_transceivers()?
            .into_iter()
            .map(|transceiver| transceiver.receiver())
            .collect())
    }

    /// Returns video receiver handles for the current transceiver snapshot.
    pub fn video_receivers(&self) -> Result<Vec<RtpReceiver>, PeerError> {
        Ok(self
            .video_transceivers()?
            .into_iter()
            .map(|transceiver| transceiver.receiver())
            .collect())
    }

    /// Add an audio or video transceiver without a sending track. All native
    /// initialization fields are forwarded, including stream IDs and initial
    /// encodings. Use a default init for the SDK's no-init overload semantics.
    /// Native capability and parameter errors remain owning `RtcError` values.
    pub fn add_transceiver(
        &self,
        kind: crate::MediaType,
        init: &crate::RtpTransceiverInit,
    ) -> Result<RtpTransceiver, crate::RtpSenderOperationError> {
        self.inner._factory.check_control_call()?;
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            }
            .into());
        }
        let mut valid = false;
        let mut error = cxx::UniquePtr::null();
        let native = ffi::peer_add_transceiver_value(
            self.inner.native(),
            kind.to_native(),
            init.native(),
            &mut valid,
            &mut error,
        );
        if !valid {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "unsafe native transceiver initialization or non-Unified-Plan peer".into(),
            }
            .into());
        }
        if native.is_null() {
            return Err(crate::RtpSenderOperationError::Native(
                crate::RtcError::from_native(error),
            ));
        }
        Ok(RtpTransceiver::from_native(native, self.inner.clone()))
    }

    /// Install an owning SDK track reference, including a received track, with
    /// the complete native initialization value. The track must share this
    /// factory; native errors retain their full fields. Local typed sources keep
    /// their existing admission and lifetime checks.
    ///
    /// Aggregate allocator arithmetic for arbitrary bitrate-priority groups and
    /// audio allocation field-trial overrides is not yet safety-qualified. See
    /// `docs/rtp-value-contracts.md`; successful creation is not that proof.
    pub fn add_media_transceiver_with_init(
        &self,
        track: &crate::MediaTrack,
        init: &crate::RtpTransceiverInit,
    ) -> Result<RtpTransceiver, crate::RtpSenderOperationError> {
        self.inner._factory.check_control_call()?;
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            }
            .into());
        }
        if !Rc::ptr_eq(track.factory(), &self.inner._factory) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "track must belong to this peer factory".into(),
            }
            .into());
        }
        if let Some(video) = track.local_video() {
            return self.add_video_transceiver_with_init(video, init);
        }
        if let Some(audio) = track.local_audio() {
            return self.add_audio_transceiver_with_init(audio, init);
        }
        let mut valid = false;
        let mut error = cxx::UniquePtr::null();
        let native = ffi::peer_add_media_transceiver_value(
            self.inner.native(),
            track.native(),
            init.native(),
            &mut valid,
            &mut error,
        );
        if !valid {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "unsafe native transceiver initialization or non-Unified-Plan peer".into(),
            }
            .into());
        }
        if native.is_null() {
            return Err(crate::RtpSenderOperationError::Native(
                crate::RtcError::from_native(error),
            ));
        }
        let value = RtpTransceiver::from_native(native, self.inner.clone());
        let sender = value.sender();
        // MediaTrack's remote owner retains the factory, not its originating
        // PeerInner. A forwarding sender must not create peer ownership cycles.
        self.inner.stream_inputs.borrow_mut().insert(
            sender.resource_key(),
            SenderTrackLease::new(&sender, track.clone()),
        );
        Ok(value)
    }

    /// Install the entire native initialization value in one AddTransceiver.
    /// The supplied value remains unchanged on both rejection and acceptance.
    pub fn add_video_transceiver_with_init(
        &self,
        track: &VideoTrack,
        init: &crate::RtpTransceiverInit,
    ) -> Result<RtpTransceiver, crate::RtpSenderOperationError> {
        self.inner._factory.check_control_call()?;
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            }
            .into());
        }
        let fail = |message: &str| PeerError {
            kind: PeerErrorKind::InvalidParameter,
            message: message.into(),
        };
        if !track.is_local_to(&self.inner._factory) {
            return Err(fail("track must belong to this factory").into());
        }
        if track.is_direct_encoded() {
            let encodings = init.send_encodings();
            if (track.is_direct_simulcast() && encodings.len() != 3)
                || (!track.is_direct_simulcast() && encodings.len() > 1)
            {
                return Err(fail(
                    "initial encoding count is unsupported by the direct source profile",
                )
                .into());
            }
            for encoding in encodings {
                if encoding
                    .scalability_mode_bytes()
                    .as_deref()
                    .is_some_and(|mode| !(track.is_direct_l1t3() && mode == b"L1T3"))
                    || (!track.is_direct_simulcast()
                        && (encoding.scale_resolution_down_to().is_some()
                            || encoding
                                .scale_resolution_down_by()
                                .is_some_and(|scale| scale != 1.0)))
                    || encoding
                        .num_temporal_layers()
                        .is_some_and(|layers| layers != if track.is_direct_l1t3() { 3 } else { 1 })
                {
                    return Err(fail(
                        "initial encoding fields are unsupported by the direct source profile",
                    )
                    .into());
                }
            }
        }
        let mut valid = false;
        let mut error = cxx::UniquePtr::null();
        let native = ffi::peer_add_video_transceiver_value(
            self.inner.native(),
            track.native(),
            init.native(),
            &mut valid,
            &mut error,
        );
        if !valid {
            return Err(
                fail("unsafe native transceiver initialization or non-Unified-Plan peer").into(),
            );
        }
        if native.is_null() {
            return Err(crate::RtpSenderOperationError::Native(
                crate::RtcError::from_native(error),
            ));
        }
        let value = RtpTransceiver::from_native(native, self.inner.clone());
        self.inner.sender_tracks.borrow_mut().insert(
            value.sender().resource_key(),
            SenderTrackLease::new(&value.sender(), track.clone()),
        );
        track.register_encoded_sender(&self.inner);
        Ok(value)
    }
    pub fn add_video_transceiver(
        &self,
        track: &VideoTrack,
        direction: RtpTransceiverDirection,
    ) -> Result<RtpTransceiver, PeerError> {
        self.add_video_transceiver_with_rids(track, direction, &[])
    }

    /// Configure outbound video encoding identities before negotiation. An
    /// empty RID slice uses the upstream default single encoding. RIDs are
    /// immutable after construction; update bitrate, enabled state, scaling,
    /// and supported modes through the sender's parameter snapshot instead.
    pub fn add_video_transceiver_with_rids(
        &self,
        track: &VideoTrack,
        direction: RtpTransceiverDirection,
        rids: &[String],
    ) -> Result<RtpTransceiver, PeerError> {
        self.inner._factory.check_control_call()?;
        if self.configuration().options.sdp_semantics != crate::SdpSemantics::UnifiedPlan {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "AddTransceiver requires Unified Plan in the native SDK".into(),
            });
        }
        if !track.is_local_to(&self.inner._factory) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "video track must belong to this peer factory".into(),
            });
        }
        if track.is_direct_encoded()
            && if track.is_direct_simulcast() {
                rids.len() != 3
            } else {
                rids.len() > 1
            }
        {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedParameter,
                message: "encoded H264 input cannot use multiple RIDs".into(),
            });
        }
        let mut seen = std::collections::HashSet::new();
        if rids.iter().any(|rid| {
            rid.len() > 16
                || rid.is_empty()
                || !rid.bytes().all(|ch| ch.is_ascii_alphanumeric())
                || !seen.insert(rid)
        }) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message:
                    "video RIDs must be unique ASCII alphanumeric identifiers of 1 to 16 bytes"
                        .into(),
            });
        }
        let mut error_type = 0;
        let mut message = String::new();
        let native = ffi::peer_add_video_transceiver(
            self.inner.native(),
            track.native(),
            direction as u8,
            rids,
            &mut error_type,
            &mut message,
        );
        if native.is_null() {
            Err(PeerError {
                kind: error_kind(error_type),
                message,
            })
        } else {
            let transceiver = RtpTransceiver::from_native(native, self.inner.clone());
            let sender = transceiver.sender();
            self.inner.sender_tracks.borrow_mut().insert(
                sender.resource_key(),
                SenderTrackLease::new(&sender, track.clone()),
            );
            track.register_encoded_sender(&self.inner);
            Ok(transceiver)
        }
    }

    /// Install a local audio track and all native initialization fields.
    /// Native encoding normalization and errors are not replaced by binding
    /// policy. The track and its factory remain retained by the sender.
    pub fn add_audio_transceiver_with_init(
        &self,
        track: &AudioTrack,
        init: &crate::RtpTransceiverInit,
    ) -> Result<RtpTransceiver, crate::RtpSenderOperationError> {
        self.inner._factory.check_control_call()?;
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            }
            .into());
        }
        if !track.belongs_to(&self.inner._factory) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "audio track must belong to this peer factory".into(),
            }
            .into());
        }
        track.check_peer_dependency(&self.inner)?;
        let mut valid = false;
        let mut error = cxx::UniquePtr::null();
        let native = ffi::peer_add_audio_transceiver_value(
            self.inner.native(),
            track.native(),
            init.native(),
            &mut valid,
            &mut error,
        );
        if !valid {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "unsafe native transceiver initialization or non-Unified-Plan peer".into(),
            }
            .into());
        }
        if native.is_null() {
            return Err(crate::RtpSenderOperationError::Native(
                crate::RtcError::from_native(error),
            ));
        }
        let transceiver = RtpTransceiver::from_native(native, self.inner.clone());
        let sender = transceiver.sender();
        self.inner.audio_sender_tracks.borrow_mut().insert(
            sender.resource_key(),
            SenderTrackLease::new(&sender, track.clone()),
        );
        Ok(transceiver)
    }

    /// Adds a local audio track to this peer's negotiated RTP session.
    pub fn add_audio_transceiver(
        &self,
        track: &AudioTrack,
        direction: RtpTransceiverDirection,
    ) -> Result<RtpTransceiver, PeerError> {
        self.inner._factory.check_control_call()?;
        if self.configuration().options.sdp_semantics != crate::SdpSemantics::UnifiedPlan {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "AddTransceiver requires Unified Plan in the native SDK".into(),
            });
        }
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            });
        }
        if !track.belongs_to(&self.inner._factory) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "audio track must belong to this peer factory".into(),
            });
        }
        track.check_peer_dependency(&self.inner)?;
        let mut error_type = 0;
        let mut message = String::new();
        let native = ffi::peer_add_audio_transceiver(
            self.inner.native(),
            track.native(),
            direction as u8,
            &mut error_type,
            &mut message,
        );
        if native.is_null() {
            Err(PeerError {
                kind: error_kind(error_type),
                message,
            })
        } else {
            let transceiver = RtpTransceiver::from_native(native, self.inner.clone());
            let sender = transceiver.sender();
            self.inner.audio_sender_tracks.borrow_mut().insert(
                sender.resource_key(),
                SenderTrackLease::new(&sender, track.clone()),
            );
            Ok(transceiver)
        }
    }

    pub fn remove_track(&self, sender: &RtpSender) -> Result<(), PeerError> {
        if !Rc::ptr_eq(&self.inner, &sender.peer) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "RTP sender belongs to another peer".into(),
            });
        }
        let mut error_type = 0;
        let mut message = String::new();
        if ffi::peer_remove_track(
            self.inner.native(),
            sender.native(),
            &mut error_type,
            &mut message,
        ) {
            self.inner
                .sender_tracks
                .borrow_mut()
                .remove(&sender.resource_key());
            self.inner
                .audio_sender_tracks
                .borrow_mut()
                .remove(&sender.resource_key());
            self.inner.refresh_stream_inputs();
            Ok(())
        } else {
            Err(PeerError {
                kind: error_kind(error_type),
                message,
            })
        }
    }

    /// Admit one operation, retaining its slot until the terminal event is
    /// consumed. At most 64 operations/results are retained per peer. Closed
    /// or exhausted submissions fail synchronously and produce no event.
    /// Only one offer/answer/description setter may have a pending native
    /// callback per peer, for safe shutdown of the pinned SDP operations chain.
    pub fn create_offer(&self) -> Result<OperationId, PeerError> {
        self.create_offer_with_options(OfferAnswerOptions::default())
    }

    /// Request new ICE credentials and candidate gathering in the next offer.
    /// The operation completes through `OperationComplete` like a normal offer.
    pub fn create_ice_restart_offer(&self) -> Result<OperationId, PeerError> {
        self.create_offer_with_options(OfferAnswerOptions {
            ice_restart: true,
            ..Default::default()
        })
    }

    /// Generate SDP with the complete native offer/answer option set.
    /// Success admits work, not SDP: native validation and generation outcomes
    /// arrive in the corresponding `OperationComplete` event.
    pub fn create_offer_with_options(
        &self,
        options: OfferAnswerOptions,
    ) -> Result<OperationId, PeerError> {
        let options = options.to_native()?;
        let id = self.next_operation();
        Self::operation_admission(id, ffi::peer_create_offer(self.native(), id.0, &options))
    }

    pub fn create_answer(&self) -> Result<OperationId, PeerError> {
        self.create_answer_with_options(OfferAnswerOptions::default())
    }

    pub fn create_answer_with_options(
        &self,
        options: OfferAnswerOptions,
    ) -> Result<OperationId, PeerError> {
        let options = options.to_native()?;
        let id = self.next_operation();
        Self::operation_admission(id, ffi::peer_create_answer(self.native(), id.0, &options))
    }

    /// Mark ICE for restart and trigger native negotiation-needed processing.
    /// Subsequent offers use new credentials until a restart is negotiated.
    /// Unlike `create_ice_restart_offer`, this does not create an SDP operation.
    pub fn restart_ice(&self) -> Result<(), PeerError> {
        if ffi::peer_restart_ice(self.native()) {
            Ok(())
        } else {
            Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            })
        }
    }

    /// Revalidate a queued `NegotiationNeeded` ID against the current native
    /// operations chain and negotiation state before delivering it to a user.
    /// A stale ID or a nonempty operations chain returns `false`.
    pub fn should_fire_negotiation_needed(&self, event_id: u32) -> Result<bool, PeerError> {
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            });
        }
        Ok(ffi::peer_should_fire_negotiation_needed(
            self.native(),
            event_id,
        ))
    }

    fn operation_admission(id: OperationId, status: u8) -> Result<OperationId, PeerError> {
        match status {
            0 => Ok(id),
            1 => Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            }),
            2 => Err(PeerError {
                kind: PeerErrorKind::ResourceExhausted,
                message: "consume operation outcomes before submitting more work (limit 64)".into(),
            }),
            3 => Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "previous stats result not yet consumed".into(),
            }),
            4 => Err(PeerError {
                kind: PeerErrorKind::ResourceExhausted,
                message: "await the pending native SDP operation before submitting another".into(),
            }),
            _ => unreachable!("invalid native operation admission status"),
        }
    }

    /// One actual signaling-thread snapshot. No native state is reconstructed
    /// from callback history, SDP text or another state enum. Closed peers retain
    /// their native state observations while the owning handle is alive.
    pub fn ice_snapshot(&self) -> PeerIceSnapshot {
        let value = ffi::peer_ice_snapshot(self.native());
        PeerIceSnapshot {
            legacy_state: PeerIceConnectionState::from_raw(value.legacy_state),
            standardized_state: PeerIceConnectionState::from_raw(value.standardized_state),
            can_trickle_ice_candidates: value.has_can_trickle.then_some(value.can_trickle),
        }
    }
    pub fn ice_connection_state(&self) -> PeerIceConnectionState {
        self.ice_snapshot().legacy_state
    }
    pub fn standardized_ice_connection_state(&self) -> PeerIceConnectionState {
        self.ice_snapshot().standardized_state
    }
    pub fn can_trickle_ice_candidates(&self) -> Option<bool> {
        self.ice_snapshot().can_trickle_ice_candidates
    }

    /// Current native signaling state sampled on its signaling role, not a
    /// reconstruction from queued observations or description text.
    pub fn signaling_state(&self) -> SignalingState {
        signaling_state(ffi::peer_native_state(self.native(), 0))
    }

    /// Current overall native connection state. This is distinct from both
    /// legacy and standardized ICE connection state and remains readable after close.
    pub fn peer_connection_state(&self) -> ConnectionState {
        connection_state(ffi::peer_native_state(self.native(), 1))
    }

    pub fn ice_gathering_state(&self) -> IceGatheringState {
        gathering_state(ffi::peer_native_state(self.native(), 2))
    }

    pub fn descriptions(&self) -> Result<PeerDescriptions, PeerError> {
        let bytes = self.descriptions_bytes()?;
        let project = |value: Option<SessionDescriptionBytes>| {
            value
                .map(SessionDescription::try_from)
                .transpose()
                .map_err(|_| PeerError {
                    kind: PeerErrorKind::DescriptionNotUtf8,
                    message: "native SDP is not UTF-8; use descriptions_bytes".into(),
                })
        };
        Ok(PeerDescriptions {
            current_local: project(bytes.current_local)?,
            current_remote: project(bytes.current_remote)?,
            pending_local: project(bytes.pending_local)?,
            pending_remote: project(bytes.pending_remote)?,
        })
    }
    pub fn descriptions_bytes(&self) -> Result<PeerDescriptionBytes, PeerError> {
        let mut native = Vec::new();
        match ffi::peer_descriptions(self.native(), &mut native) {
            0 => {
                let mut result = PeerDescriptionBytes::default();
                for entry in native {
                    let description = Some(SessionDescriptionBytes {
                        kind: sdp_type(entry.kind),
                        sdp: entry.sdp,
                    });
                    match entry.slot {
                        0 => result.current_local = description,
                        1 => result.current_remote = description,
                        2 => result.pending_local = description,
                        3 => result.pending_remote = description,
                        _ => unreachable!("native adapter returned an invalid SDP slot"),
                    }
                }
                Ok(result)
            }
            1 => Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            }),
            _ => Err(PeerError {
                kind: PeerErrorKind::Internal,
                message: "failed to serialize peer descriptions".into(),
            }),
        }
    }

    /// Let native signaling state choose an offer or answer, generate it and
    /// apply it without SDP munging. Completion may occur during initiation,
    /// but remains owned until the corresponding event is consumed. Read
    /// `descriptions()` after success to obtain the resulting SDP.
    pub fn set_local_description_auto(&self) -> Result<OperationId, PeerError> {
        self.set_local_description_auto_impl(false)
    }
    /// Deprecated SDK observer overload. The SDK posts its completion later.
    pub fn set_local_description_auto_legacy(&self) -> Result<OperationId, PeerError> {
        self.set_local_description_auto_impl(true)
    }
    fn set_local_description_auto_impl(&self, legacy: bool) -> Result<OperationId, PeerError> {
        let id = self.next_operation();
        Self::operation_admission(
            id,
            ffi::peer_set_local_description_auto(self.native(), id.0, legacy),
        )
    }

    pub fn set_local_description(
        &self,
        description: SessionDescription,
    ) -> Result<OperationId, PeerError> {
        self.set_local_description_bytes(description.into())
    }
    pub fn set_local_description_bytes(
        &self,
        description: SessionDescriptionBytes,
    ) -> Result<OperationId, PeerError> {
        self.set_local_description_impl(description, false)
    }
    /// Use the pinned deprecated SetSessionDescriptionObserver overload.
    /// Completion is delayed by native code, not synthesized by this wrapper.
    pub fn set_local_description_legacy(
        &self,
        description: impl Into<SessionDescriptionBytes>,
    ) -> Result<OperationId, PeerError> {
        self.set_local_description_impl(description.into(), true)
    }
    fn set_local_description_impl(
        &self,
        description: SessionDescriptionBytes,
        legacy: bool,
    ) -> Result<OperationId, PeerError> {
        let id = self.next_operation();
        let status = ffi::peer_set_local_description(
            self.native(),
            id.0,
            description.kind as u8,
            &description.sdp,
            legacy,
        );
        Self::operation_admission(id, status)
    }

    pub fn set_remote_description(
        &self,
        description: SessionDescription,
    ) -> Result<OperationId, PeerError> {
        self.set_remote_description_bytes(description.into())
    }
    pub fn set_remote_description_bytes(
        &self,
        description: SessionDescriptionBytes,
    ) -> Result<OperationId, PeerError> {
        self.set_remote_description_impl(description, false)
    }
    /// Use the pinned deprecated remote-description observer overload.
    pub fn set_remote_description_legacy(
        &self,
        description: impl Into<SessionDescriptionBytes>,
    ) -> Result<OperationId, PeerError> {
        self.set_remote_description_impl(description.into(), true)
    }
    fn set_remote_description_impl(
        &self,
        description: SessionDescriptionBytes,
        legacy: bool,
    ) -> Result<OperationId, PeerError> {
        let id = self.next_operation();
        let status = ffi::peer_set_remote_description(
            self.native(),
            id.0,
            description.kind as u8,
            &description.sdp,
            legacy,
        );
        Self::operation_admission(id, status)
    }

    pub fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<OperationId, PeerError> {
        self.add_ice_candidate_bytes(candidate.into())
    }
    pub fn add_ice_candidate_bytes(
        &self,
        candidate: IceCandidateBytes,
    ) -> Result<OperationId, PeerError> {
        let id = self.next_operation();
        Self::operation_admission(
            id,
            ffi::peer_add_ice_candidate(
                self.native(),
                id.0,
                &candidate.sdp_mid,
                candidate.sdp_mline_index,
                &candidate.candidate,
            ),
        )
    }

    /// Native synchronous candidate admission. Unlike `add_ice_candidate`,
    /// this is not chained after pending SDP operations and produces no
    /// completion event. Parsing failures are typed errors; native `false`
    /// remains a rejection without an invented error reason.
    pub fn add_ice_candidate_sync(
        &self,
        candidate: IceCandidate,
    ) -> Result<IceCandidateDisposition, PeerError> {
        self.modify_ice_candidate(candidate, false)
    }

    /// Remove a remote candidate using the native synchronous API. Acceptance
    /// does not establish whether that candidate was previously present.
    pub fn remove_ice_candidate(
        &self,
        candidate: IceCandidate,
    ) -> Result<IceCandidateDisposition, PeerError> {
        self.modify_ice_candidate(candidate, true)
    }

    pub fn add_ice_candidate_sync_bytes(
        &self,
        candidate: IceCandidateBytes,
    ) -> Result<IceCandidateDisposition, PeerErrorBytes> {
        self.modify_ice_candidate_bytes(candidate, false)
    }
    pub fn remove_ice_candidate_bytes(
        &self,
        candidate: IceCandidateBytes,
    ) -> Result<IceCandidateDisposition, PeerErrorBytes> {
        self.modify_ice_candidate_bytes(candidate, true)
    }
    fn modify_ice_candidate(
        &self,
        candidate: IceCandidate,
        remove: bool,
    ) -> Result<IceCandidateDisposition, PeerError> {
        self.modify_ice_candidate_bytes(candidate.into(), remove)
            .map_err(|error| match String::from_utf8(error.message) {
                Ok(message) => PeerError {
                    kind: error.kind,
                    message,
                },
                Err(_) => PeerError {
                    kind: PeerErrorKind::MessageNotUtf8,
                    message: "native candidate error is not UTF-8; use byte candidate methods"
                        .into(),
                },
            })
    }
    fn modify_ice_candidate_bytes(
        &self,
        candidate: IceCandidateBytes,
        remove: bool,
    ) -> Result<IceCandidateDisposition, PeerErrorBytes> {
        let mut message = Vec::new();
        match ffi::peer_modify_ice_candidate(
            self.native(),
            remove,
            &candidate.sdp_mid,
            candidate.sdp_mline_index,
            &candidate.candidate,
            &mut message,
        ) {
            0 => Ok(IceCandidateDisposition::Accepted),
            1 => Err(PeerErrorBytes {
                kind: PeerErrorKind::Closed,
                message: b"peer connection is closed".to_vec(),
            }),
            2 => Err(PeerErrorBytes {
                kind: PeerErrorKind::Syntax,
                message,
            }),
            3 => Ok(IceCandidateDisposition::Rejected),
            _ => unreachable!("invalid native candidate outcome"),
        }
    }

    /// Request one typed stats snapshot. Only one request may be outstanding
    /// until its event is taken, keeping queued snapshots bounded. A request
    /// rejected here has no asynchronous completion.
    pub fn request_stats(&self) -> Result<OperationId, PeerError> {
        let id = self.next_operation();
        Self::operation_admission(id, ffi::peer_request_stats(self.native(), id.0))
    }

    pub fn request_sender_stats(&self, sender: &RtpSender) -> Result<OperationId, PeerError> {
        self.request_stats_with_sender_selector(Some(sender))
    }
    /// Null and foreign selectors yield the SDK's empty report, not a binding
    /// error or the complete peer report. Only target-peer admission is checked.
    pub fn request_stats_with_sender_selector(
        &self,
        selector: Option<&RtpSender>,
    ) -> Result<OperationId, PeerError> {
        let id = self.next_operation();
        let status = match selector {
            Some(sender) => ffi::peer_request_sender_stats(self.native(), sender.native(), id.0),
            None => ffi::peer_request_stats_with_empty_selector(self.native(), false, id.0),
        };
        Self::operation_admission(id, status)
    }
    pub fn request_receiver_stats(&self, receiver: &RtpReceiver) -> Result<OperationId, PeerError> {
        self.request_stats_with_receiver_selector(Some(receiver))
    }
    pub fn request_stats_with_receiver_selector(
        &self,
        selector: Option<&RtpReceiver>,
    ) -> Result<OperationId, PeerError> {
        let id = self.next_operation();
        let status = match selector {
            Some(receiver) => {
                ffi::peer_request_receiver_stats(self.native(), receiver.native(), id.0)
            }
            None => ffi::peer_request_stats_with_empty_selector(self.native(), true, id.0),
        };
        Self::operation_admission(id, status)
    }
    /// Deprecated native all-track stats. Value references are copied on
    /// signaling; this does not route through modern stats or reinterpret JSON.
    pub fn request_legacy_stats(
        &self,
        level: crate::LegacyStatsOutputLevel,
    ) -> Result<OperationId, crate::LegacyStatsRequestError> {
        let id = self.next_operation();
        self.request_legacy_stats_at(id, None, level)
    }
    /// Forward the actual optional SDK track selector. Native false means no
    /// operation was accepted and no callback/result is fabricated. A true
    /// request retains the selected track's own source/factory roles until
    /// the native continuation has retired, including across close/drop.
    pub fn request_legacy_stats_with_track(
        &self,
        selector: Option<&crate::MediaTrack>,
        level: crate::LegacyStatsOutputLevel,
    ) -> Result<OperationId, crate::LegacyStatsRequestError> {
        self.request_legacy_stats_at(self.next_operation(), selector, level)
    }
    fn request_legacy_stats_at(
        &self,
        id: OperationId,
        selector: Option<&crate::MediaTrack>,
        level: crate::LegacyStatsOutputLevel,
    ) -> Result<OperationId, crate::LegacyStatsRequestError> {
        let status = match selector {
            Some(track) => ffi::peer_request_legacy_stats_with_track(
                self.native(),
                track.native(),
                level as u8,
                id.0,
            ),
            None => ffi::peer_request_legacy_stats(self.native(), level as u8, id.0),
        };
        if status == 5 {
            return Err(crate::LegacyStatsRequestError::NativeRejected);
        }
        let admitted =
            Self::operation_admission(id, status).map_err(crate::LegacyStatsRequestError::from)?;
        *self.inner.pending_legacy_selector.borrow_mut() =
            selector.map(|track| (admitted, track.clone()));
        Ok(admitted)
    }
    pub fn clear_stats_cache(&self) {
        ffi::peer_clear_stats_cache(self.native());
    }

    /// Start native event logging, consuming the owned output on success or
    /// failure. `None` selects the engine's default period. Requires an event
    /// log factory at construction; native kill-switch trials remain effective.
    pub fn start_event_log(
        &self,
        output: crate::EventLogOutput,
        period: Option<crate::EventLogOutputPeriod>,
    ) -> Result<(), crate::EventLogError> {
        if self.inner.closed.get() {
            return Err(crate::EventLogError::Closed);
        }
        let milliseconds = period
            .map(crate::EventLogOutputPeriod::milliseconds)
            .transpose()?;
        match ffi::peer_start_event_log(
            self.native(),
            output.into_native(),
            milliseconds.is_some(),
            milliseconds.unwrap_or(0),
        ) {
            0 => Ok(()),
            1 => Err(crate::EventLogError::Closed),
            2 => Err(crate::EventLogError::InactiveOutput),
            error if error >= 16 => crate::event_log_product::status(error - 16),
            _ => Err(crate::EventLogError::NativeRejected),
        }
    }

    /// Stop and wait until native logging has released the output. Only after
    /// return is it safe to read, replace, or modify the destination file.
    pub fn stop_event_log(&self) -> Result<(), crate::EventLogError> {
        if self.inner.closed.get() {
            return Err(crate::EventLogError::Closed);
        }
        match ffi::peer_stop_event_log(self.native()) {
            0 => Ok(()),
            1 => Err(crate::EventLogError::Closed),
            error => crate::event_log_product::status(error - 16),
        }
    }

    /// Apply libwebrtc's bandwidth-estimation constraints. Values are bits per
    /// second; absent fields are forwarded as native unset values. This
    /// does not bypass the native allocator or promise a per-encoding rate.
    /// Native validation reports inconsistent bounds and closed-state errors.
    pub fn set_bitrate(
        &self,
        minimum: Option<u32>,
        start: Option<u32>,
        maximum: Option<u32>,
    ) -> Result<(), PeerError> {
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            });
        }
        let convert = |value: Option<u32>| -> Result<i32, PeerError> {
            value
                .map(i32::try_from)
                .transpose()
                .map(|value| value.unwrap_or(-1))
                .map_err(|_| PeerError {
                    kind: PeerErrorKind::InvalidParameter,
                    message: "bitrate exceeds native i32 range".into(),
                })
        };
        let (minimum, start, maximum) = (convert(minimum)?, convert(start)?, convert(maximum)?);
        let mut error_type = 0;
        let mut message = String::new();
        ffi::peer_set_bitrate(
            self.native(),
            minimum,
            start,
            maximum,
            &mut error_type,
            &mut message,
        )
        .then_some(())
        .ok_or_else(|| PeerError {
            kind: error_kind(error_type),
            message,
        })
    }

    /// Take an owned observation. State notifications coalesce at callback
    /// time; they are not a complete transition history. Accepted operation
    /// outcomes remain available until consumed, including after close.
    pub fn try_next_event(&self) -> Option<PeerConnectionEvent> {
        let event = ffi::peer_take_event(self.native());
        let mut selector = self.inner.pending_legacy_selector.borrow_mut();
        if matches!(event.kind, 1 | 16)
            && selector
                .as_ref()
                .is_some_and(|(id, _)| id.0 == event.operation_id)
        {
            // Native publishes legacy DTOs only beyond its SDK-capture fence.
            selector.take();
        }
        drop(selector);
        event_from_ffi(event, &self.inner)
    }

    pub fn event_observation(&self) -> PeerEventObservation {
        let value = ffi::peer_event_observation(self.native());
        PeerEventObservation {
            dropped_control_events: value.dropped_control_events,
            coalesced_advisories: value.coalesced_advisories,
            retained_control_events: value.retained_control_events,
            admitted_operations: value.admitted_operations,
            pending_operations: value.pending_operations,
        }
    }

    /// Enable or disable WebRTC's native microphone recording or speaker
    /// playout for the audio state shared by peers from this factory. WebRTC
    /// starts the selected device only while matching media streams exist.
    /// This accepts the request; device startup failures inside WebRTC are not
    /// reported synchronously by the upstream API. Headless peers reject it.
    #[cfg(feature = "native")]
    pub fn set_native_audio_enabled(
        &self,
        recording: bool,
        enabled: bool,
    ) -> Result<(), PeerError> {
        if self.inner.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer connection is closed".into(),
            });
        }
        let mut error = String::new();
        ffi::peer_set_native_audio_enabled(self.native(), recording, enabled, &mut error)
            .then_some(())
            .ok_or(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: error,
            })
    }

    pub fn close(&mut self) -> Result<(), PeerError> {
        if self.inner.closed.get() {
            return Ok(());
        }
        let driver = self.inner._factory._controlled_driver.as_ref();
        // Foreign encoder/readiness callbacks may have no native queue TLS.
        // The callback loan itself, not a thread-name/identity guess, forbids
        // synchronous teardown that could wait for this same producer.
        if crate::provider_panic::callback_active() {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "cannot close a peer from an active native provider/context callback; retry on a caller-owned control queue".into(),
            });
        }
        if !ffi::peer_control_call_allowed(self.native()) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "this native role cannot synchronously close a peer on signaling".into(),
            });
        }
        let _turn = driver.map(ControlledPeerDriver::owner_turn);
        // Admit once before native Close can reenter user providers.
        self.inner.closed.set(true);
        if ffi::close_peer_connection(self.native()) {
            self.inner.pending_legacy_selector.borrow_mut().take();
            self.inner.sender_tracks.borrow_mut().clear();
            self.inner.audio_sender_tracks.borrow_mut().clear();
            self.inner.local_streams.borrow_mut().clear();
            self.inner.stream_inputs.borrow_mut().clear();
            Ok(())
        } else {
            self.inner.closed.set(false);
            Err(PeerError {
                kind: PeerErrorKind::Internal,
                message: "failed to close peer connection".into(),
            })
        }
    }

    fn next_operation(&self) -> OperationId {
        let current = self.next_operation_id.get();
        self.next_operation_id
            .set(current.checked_add(1).unwrap_or(1));
        OperationId(current)
    }

    fn native(&self) -> &ffi::NativePeerConnection {
        self.inner.native()
    }
}

impl PeerInner {
    pub(crate) fn refresh_stream_inputs(self: &Rc<Self>) {
        if self.closed.get()
            || (self.local_streams.borrow().is_empty() && self.stream_inputs.borrow().is_empty())
        {
            return;
        }
        let mut sources: Vec<_> = self
            .stream_inputs
            .borrow()
            .values()
            .map(|v| v.track.clone())
            .collect();
        for stream in self.local_streams.borrow().iter() {
            sources.extend(stream.source_leases());
        }
        let mut inputs = HashMap::new();
        for sender in self.sender_snapshot() {
            let native = ffi::rtp_sender_track_reference(sender.native());
            if native.is_null() {
                continue;
            }
            let actual = crate::MediaTrack::from_native(
                native,
                crate::media_track::MediaTrackOwner::Factory {
                    _factory: self._factory.clone(),
                },
            );
            if let Some(source) = sources.iter().find(|source| source.same_resource(&actual)) {
                if let Some(video) = source.local_video() {
                    video.register_encoded_sender(self);
                }
                inputs.insert(
                    sender.resource_key(),
                    SenderTrackLease::new(&sender, source.clone()),
                );
            }
        }
        *self.stream_inputs.borrow_mut() = inputs;
    }
    pub(crate) fn controlled_time(&self) -> Option<std::time::Duration> {
        self._factory
            ._controlled_driver
            .as_ref()
            .map(|driver| driver.clock().now())
    }

    pub(crate) fn factory_native(&self) -> &ffi::NativePeerConnectionFactory {
        self._factory
            .native
            .as_ref()
            .expect("validated peer factory")
    }

    pub(crate) fn native(&self) -> &ffi::NativePeerConnection {
        self.native.as_ref().expect("validated peer connection")
    }
}

fn native_build_error(error: impl fmt::Display) -> PeerError {
    PeerError {
        kind: PeerErrorKind::NativeConstruction,
        message: error.to_string(),
    }
}

fn optional_ptr<T>(value: Option<&T>) -> *const T {
    value.map_or(std::ptr::null(), |value| value as *const T)
}

struct StatFields<'a>(&'a [ffi::FfiStatsField]);

impl StatFields<'_> {
    fn field(&self, name: &str, kind: u8) -> Option<&ffi::FfiStatsField> {
        self.0
            .iter()
            .find(|field| field.name == name && field.kind == kind)
    }

    fn text(&self, name: &str) -> Option<Vec<u8>> {
        self.field(name, 1).map(|field| field.text.clone())
    }

    fn unsigned(&self, name: &str) -> Option<u64> {
        self.field(name, 2).map(|field| field.unsigned_value)
    }

    fn signed(&self, name: &str) -> Option<i64> {
        self.field(name, 3).map(|field| field.signed_value)
    }

    fn decimal(&self, name: &str) -> Option<f64> {
        self.field(name, 4).map(|field| field.decimal)
    }

    fn flag(&self, name: &str) -> Option<bool> {
        self.field(name, 5).map(|field| field.flag)
    }
}

fn stats_record_from_ffi(record: ffi::FfiStatsRecord) -> PeerStatsRecord {
    let fields = StatFields(&record.fields);
    let id = record.id;
    let timestamp_us = record.timestamp_us;
    match record.kind {
        1 => PeerStatsRecord::CandidatePair(CandidatePairStats {
            id,
            timestamp_us,
            transport_id: fields.text("transport_id"),
            local_candidate_id: fields.text("local_candidate_id"),
            remote_candidate_id: fields.text("remote_candidate_id"),
            state: fields.text("state"),
            nominated: fields.flag("nominated"),
            packets_sent: fields.unsigned("packets_sent"),
            packets_received: fields.unsigned("packets_received"),
            bytes_sent: fields.unsigned("bytes_sent"),
            bytes_received: fields.unsigned("bytes_received"),
            current_round_trip_time: fields.decimal("current_round_trip_time"),
            available_outgoing_bitrate: fields.decimal("available_outgoing_bitrate"),
            available_incoming_bitrate: fields.decimal("available_incoming_bitrate"),
        }),
        2 => PeerStatsRecord::Transport(TransportStats {
            id,
            timestamp_us,
            selected_candidate_pair_id: fields.text("selected_candidate_pair_id"),
            ice_state: fields.text("ice_state"),
            dtls_state: fields.text("dtls_state"),
            packets_sent: fields.unsigned("packets_sent"),
            packets_received: fields.unsigned("packets_received"),
            bytes_sent: fields.unsigned("bytes_sent"),
            bytes_received: fields.unsigned("bytes_received"),
        }),
        3 => PeerStatsRecord::InboundRtp(InboundRtpStats {
            id,
            timestamp_us,
            kind: fields.text("kind"),
            ssrc: fields.unsigned("ssrc"),
            transport_id: fields.text("transport_id"),
            mid: fields.text("mid"),
            packets_received: fields.unsigned("packets_received"),
            packets_lost: fields.signed("packets_lost"),
            bytes_received: fields.unsigned("bytes_received"),
            jitter: fields.decimal("jitter"),
            frames_received: fields.unsigned("frames_received"),
            frames_decoded: fields.unsigned("frames_decoded"),
        }),
        4 => PeerStatsRecord::OutboundRtp(OutboundRtpStats {
            id,
            timestamp_us,
            kind: fields.text("kind"),
            ssrc: fields.unsigned("ssrc"),
            transport_id: fields.text("transport_id"),
            mid: fields.text("mid"),
            rid: fields.text("rid"),
            packets_sent: fields.unsigned("packets_sent"),
            bytes_sent: fields.unsigned("bytes_sent"),
            target_bitrate: fields.decimal("target_bitrate"),
            frames_encoded: fields.unsigned("frames_encoded"),
            frames_sent: fields.unsigned("frames_sent"),
        }),
        5 => PeerStatsRecord::DataChannel(DataChannelStats {
            id,
            timestamp_us,
            label: fields.text("label"),
            protocol: fields.text("protocol"),
            state: fields.text("state"),
            data_channel_identifier: fields.signed("data_channel_identifier"),
            messages_sent: fields.unsigned("messages_sent"),
            messages_received: fields.unsigned("messages_received"),
            bytes_sent: fields.unsigned("bytes_sent"),
            bytes_received: fields.unsigned("bytes_received"),
        }),
        _ => unreachable!("native stats record kind"),
    }
}

fn event_from_ffi(event: ffi::FfiPeerEvent, peer: &Rc<PeerInner>) -> Option<PeerConnectionEvent> {
    match event.kind {
        17 => Some(PeerConnectionEvent::IceSelectedCandidatePairChanged(
            crate::IceCandidatePairChange::from_native(event.pair_change),
        )),
        0 => None,
        16 => Some(PeerConnectionEvent::LegacyStats(
            crate::PeerLegacyStatsSnapshot::from_native(
                OperationId(event.operation_id),
                event.legacy_stats_reports,
            ),
        )),
        12 => Some(PeerConnectionEvent::Stats(PeerStatsSnapshot {
            report: crate::RtcStatsReport::from_native(event.stats_report),
            operation_id: OperationId(event.operation_id),
            timestamp_us: event.stats_timestamp_us,
            records: event
                .stats_records
                .into_iter()
                .map(stats_record_from_ffi)
                .collect(),
        })),
        1 => {
            let operation_id = OperationId(event.operation_id);
            if event.error_type != 0 {
                let kind = error_kind(event.error_type);
                return match String::from_utf8(event.message) {
                    Ok(message) => Some(PeerConnectionEvent::OperationComplete(
                        OperationCompletion {
                            operation_id,
                            result: Err(PeerError { kind, message }),
                        },
                    )),
                    Err(error) => Some(PeerConnectionEvent::OperationFailedBytes {
                        operation_id,
                        error: PeerErrorBytes {
                            kind,
                            message: error.into_bytes(),
                        },
                    }),
                };
            }
            if !event.has_description {
                return Some(PeerConnectionEvent::OperationComplete(
                    OperationCompletion {
                        operation_id,
                        result: Ok(None),
                    },
                ));
            }
            let kind = sdp_type(event.sdp_type);
            match String::from_utf8(event.sdp) {
                Ok(sdp) => Some(PeerConnectionEvent::OperationComplete(
                    OperationCompletion {
                        operation_id,
                        result: Ok(Some(SessionDescription { kind, sdp })),
                    },
                )),
                Err(error) => Some(PeerConnectionEvent::OperationCompleteBytes {
                    operation_id,
                    description: SessionDescriptionBytes {
                        kind,
                        sdp: error.into_bytes(),
                    },
                }),
            }
        }
        2 | 15 => {
            let removed = event.kind == 15;
            let bytes = IceCandidateBytes {
                sdp_mid: event.sdp_mid,
                sdp_mline_index: event.sdp_mline_index,
                candidate: event.candidate,
            };
            Some(match IceCandidate::try_from(bytes.clone()) {
                Ok(value) if removed => PeerConnectionEvent::IceCandidateRemoved(value),
                Ok(value) => PeerConnectionEvent::IceCandidate(value),
                Err(_) if removed => PeerConnectionEvent::IceCandidateRemovedBytes(bytes),
                Err(_) => PeerConnectionEvent::IceCandidateBytes(bytes),
            })
        }
        3 => Some(PeerConnectionEvent::ConnectionStateChanged(
            connection_state(event.state),
        )),
        4 => Some(PeerConnectionEvent::SignalingStateChanged(signaling_state(
            event.state,
        ))),
        5 => Some(PeerConnectionEvent::IceGatheringStateChanged(
            gathering_state(event.state),
        )),
        13 => Some(PeerConnectionEvent::IceConnectionStateChanged(
            PeerIceConnectionState::from_raw(event.ice_state),
        )),
        14 => Some(PeerConnectionEvent::StandardizedIceConnectionStateChanged(
            PeerIceConnectionState::from_raw(event.ice_state),
        )),
        6 => Some(PeerConnectionEvent::NegotiationNeeded {
            event_id: event.event_id,
        }),
        7 => match String::from_utf8(event.message) {
            Ok(message) => Some(PeerConnectionEvent::IceCandidateError {
                address: event.address,
                port: event.port,
                url: event.url,
                error_code: event.error_code,
                message,
            }),
            Err(error) => Some(PeerConnectionEvent::IceCandidateErrorBytes {
                address: event.address,
                port: event.port,
                url: event.url,
                error_code: event.error_code,
                message: error.into_bytes(),
            }),
        },
        8 => Some(PeerConnectionEvent::Closed),
        9 => {
            let native = ffi::peer_take_data_channel(peer.native(), event.operation_id);
            assert!(
                !native.is_null(),
                "native adapter lost a remote data channel"
            );
            Some(PeerConnectionEvent::DataChannel(DataChannel::from_native(
                native,
                peer.clone(),
            )))
        }
        10 => {
            let native = ffi::peer_take_transceiver(peer.native(), event.operation_id);
            assert!(
                !native.is_null(),
                "native adapter lost a remote transceiver"
            );
            Some(PeerConnectionEvent::Track(RtpTransceiver::from_native(
                native,
                peer.clone(),
            )))
        }
        11 => {
            let native = ffi::peer_take_receiver(peer.native(), event.operation_id);
            assert!(!native.is_null(), "native adapter lost a removed receiver");
            Some(PeerConnectionEvent::TrackRemoved(RtpReceiver::from_native(
                native,
                peer.clone(),
            )))
        }
        18 | 19 => {
            let native = ffi::peer_take_stream_event(peer.native(), event.operation_id);
            assert!(!native.is_null(), "native adapter lost a stream arrival");
            let stream = crate::MediaStream::from_native(native, peer._factory.clone());
            Some(if event.kind == 18 {
                PeerConnectionEvent::StreamAdded(stream)
            } else {
                PeerConnectionEvent::StreamRemoved(stream)
            })
        }
        20 => {
            let value = ffi::peer_take_track_event(peer.native(), event.operation_id);
            assert!(
                !value.receiver.is_null() && !value.streams.is_null(),
                "native adapter lost a track arrival"
            );
            let streams = (0..ffi::media_streams_len(&value.streams))
                .map(|i| {
                    crate::MediaStream::from_native(
                        ffi::media_streams_get(&value.streams, i),
                        peer._factory.clone(),
                    )
                })
                .collect();
            Some(PeerConnectionEvent::TrackAdded {
                receiver: RtpReceiver::from_native(value.receiver, peer.clone()),
                streams,
            })
        }
        21 => Some(PeerConnectionEvent::RenegotiationNeeded),
        22 => Some(PeerConnectionEvent::IceConnectionReceivingChanged(
            event.state != 0,
        )),
        23 => Some(PeerConnectionEvent::InterestingUsage {
            usage_pattern: event.ice_state,
        }),
        24 => {
            let value = event.channel_message;
            Some(PeerConnectionEvent::DataChannelMessage(
                PeerDataChannelMessage {
                    unix_timestamp_ms: value.unix_timestamp_ms,
                    datachannel_id: value.datachannel_id,
                    label: value.label,
                    direction: match value.direction {
                        0 => DataChannelMessageDirection::Send,
                        1 => DataChannelMessageDirection::Receive,
                        _ => unreachable!("pinned message direction"),
                    },
                    message: crate::DataChannelMessage {
                        kind: match value.data_type {
                            0 => crate::DataChannelMessageKind::Text,
                            1 => crate::DataChannelMessageKind::Binary,
                            _ => unreachable!("pinned message type"),
                        },
                        bytes: value.data,
                    },
                },
            ))
        }
        _ => unreachable!("native adapter returned an invalid peer event"),
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    #[test]
    fn candidate_arrival_and_removal_keep_native_bytes_and_index_normalization() {
        let factory = PeerConnectionFactory::builder().build().unwrap();
        let mut peer = factory
            .create_peer_connection(PeerConfiguration::default())
            .unwrap();
        let mid = [0xff, 0];
        let mut sdp = b"candidate:".to_vec();
        sdp.push(0xfe);
        sdp.extend_from_slice(b" 1 udp 123 127.0.0.1 10000 typ host");
        for removed in [false, true] {
            assert!(ffi::test_peer_candidate_observation(
                peer.native(),
                &mid,
                i32::MAX,
                &sdp,
                removed
            ));
            let bytes = match peer.try_next_event().unwrap() {
                PeerConnectionEvent::IceCandidateBytes(bytes) if !removed => bytes,
                PeerConnectionEvent::IceCandidateRemovedBytes(bytes) if removed => bytes,
                event => panic!("unexpected native candidate observation: {event:?}"),
            };
            assert_eq!(bytes.sdp_mid, mid);
            assert_eq!(bytes.sdp_mline_index, -1); // Actual SDK constructor normalization.
            assert!(bytes.candidate.contains(&0xfe));
        }
        assert_eq!(peer.event_observation().retained_control_events, 0);
        peer.close().unwrap();
    }
    #[test]
    fn ice_callbacks_are_distinct_callback_time_controls_with_existing_bounds() {
        let factory = PeerConnectionFactory::builder().build().unwrap();
        let mut peer = factory
            .create_peer_connection(PeerConfiguration::default())
            .unwrap();
        assert!(ffi::test_peer_ice_observations(peer.native(), 2, 3, 200));
        let observation = peer.event_observation();
        assert_eq!(observation.retained_control_events, 128);
        assert_eq!(observation.dropped_control_events, 272);
        assert_eq!(peer.ice_connection_state(), PeerIceConnectionState::New);
        for _ in 0..64 {
            assert!(matches!(
                peer.try_next_event(),
                Some(PeerConnectionEvent::IceConnectionStateChanged(
                    PeerIceConnectionState::Connected
                ))
            ));
            assert!(matches!(
                peer.try_next_event(),
                Some(PeerConnectionEvent::StandardizedIceConnectionStateChanged(
                    PeerIceConnectionState::Completed
                ))
            ));
        }
        assert!(peer.try_next_event().is_none());
        assert_eq!(peer.event_observation().retained_control_events, 0);
        assert_eq!(peer.event_observation().admitted_operations, 0);
        peer.close().unwrap();
    }
    #[test]
    fn arrival_id_collision_does_not_retire_a_pending_legacy_operation() {
        // Narrow bookkeeping regression, not simulated data-channel delivery.
        // Arrival IDs and operation IDs deliberately occupy different spaces.
        let world =
            crate::ControlledWorld::acquire(919, std::time::Duration::from_secs(3)).unwrap();
        let network = world.create_network().unwrap();
        let endpoint = network
            .register_endpoint(std::net::Ipv4Addr::new(10, 91, 9, 1).into())
            .unwrap();
        let factory = world
            .peer_factory_builder()
            .unwrap()
            .network_manager(endpoint.network_manager().unwrap())
            .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
            .build()
            .unwrap();
        let mut peer = factory.create_peer_connection(Default::default()).unwrap();
        // A real SDK all-track request, explicitly assigned ID 1 to collide
        // with the observer fixture's first data-channel arrival ID.
        assert_eq!(ffi::peer_request_legacy_stats(peer.native(), 0, 1), 0);
        let source = factory.create_video_source().unwrap();
        let video = factory
            .create_video_track("bookkeeping-lease", &source)
            .unwrap();
        // Attach an extra private lease solely to test Rust's cleanup filter;
        // no claim that the SDK accepted this unregistered track selector.
        *peer.inner.pending_legacy_selector.borrow_mut() =
            Some((OperationId(1), video.as_media_track()));
        assert!(ffi::test_peer_control_observations(peer.native(), 0, true));
        let mut arrival = false;
        while let Some(event) = peer.try_next_event() {
            if matches!(event, PeerConnectionEvent::DataChannel(_)) {
                arrival = true;
                break;
            }
        }
        assert!(arrival);
        assert!(peer.inner.pending_legacy_selector.borrow().is_some());
        assert_eq!(peer.event_observation().pending_operations, 1);
        assert_eq!(world.now(), std::time::Duration::from_secs(3));
        peer.close().unwrap();
        assert!(peer.inner.pending_legacy_selector.borrow().is_none());
    }
    #[test]
    fn observer_control_backlog_is_bounded_and_states_are_callback_snapshots() {
        // These are binding observer-unit injections, not wire/transport proof.
        for (bytes, arrivals) in [(32, false), (4096, false), (0, true)] {
            let factory = PeerConnectionFactory::builder().build().unwrap();
            let mut peer = factory
                .create_peer_connection(PeerConfiguration::default())
                .unwrap();
            assert!(ffi::test_peer_control_observations(
                peer.native(),
                bytes,
                arrivals
            ));
            let before = peer.event_observation();
            assert!(before.retained_control_events <= 132);
            assert!(before.coalesced_advisories >= 199);
            assert!(before.dropped_control_events > 0);
            if bytes == 4096 {
                assert!(before.retained_control_events < 128);
            }
            // Engine signaling is still Stable: queued notification must retain
            // the supplied callback-time HaveLocalOffer, not sample it later.
            assert!(matches!(
                peer.try_next_event(),
                Some(PeerConnectionEvent::SignalingStateChanged(
                    SignalingState::HaveLocalOffer
                ))
            ));
            peer.close().unwrap();
            assert_eq!(peer.event_observation().retained_control_events, 0);
            assert!(matches!(
                peer.try_next_event(),
                Some(PeerConnectionEvent::Closed)
            ));
            assert!(peer.try_next_event().is_none());
        }
    }
}

fn sdp_type(value: u8) -> SessionDescriptionType {
    match value {
        0 => SessionDescriptionType::Offer,
        1 => SessionDescriptionType::ProvisionalAnswer,
        2 => SessionDescriptionType::Answer,
        3 => SessionDescriptionType::Rollback,
        _ => unreachable!("native adapter returned an invalid SDP type"),
    }
}

fn connection_state(value: u8) -> ConnectionState {
    match value {
        0 => ConnectionState::New,
        1 => ConnectionState::Connecting,
        2 => ConnectionState::Connected,
        3 => ConnectionState::Disconnected,
        4 => ConnectionState::Failed,
        5 => ConnectionState::Closed,
        _ => unreachable!("native adapter returned an invalid connection state"),
    }
}

fn signaling_state(value: u8) -> SignalingState {
    match value {
        0 => SignalingState::Stable,
        1 => SignalingState::HaveLocalOffer,
        2 => SignalingState::HaveLocalProvisionalAnswer,
        3 => SignalingState::HaveRemoteOffer,
        4 => SignalingState::HaveRemoteProvisionalAnswer,
        5 => SignalingState::Closed,
        _ => unreachable!("native adapter returned an invalid signaling state"),
    }
}

fn gathering_state(value: u8) -> IceGatheringState {
    match value {
        0 => IceGatheringState::New,
        1 => IceGatheringState::Gathering,
        2 => IceGatheringState::Complete,
        _ => unreachable!("native adapter returned an invalid ICE gathering state"),
    }
}

pub(crate) fn error_kind(value: u8) -> PeerErrorKind {
    match value {
        1 => PeerErrorKind::UnsupportedOperation,
        2 => PeerErrorKind::UnsupportedParameter,
        3 => PeerErrorKind::InvalidParameter,
        4 => PeerErrorKind::InvalidRange,
        5 => PeerErrorKind::Syntax,
        6 => PeerErrorKind::InvalidState,
        7 => PeerErrorKind::InvalidModification,
        8 => PeerErrorKind::Network,
        9 => PeerErrorKind::ResourceExhausted,
        10 => PeerErrorKind::Internal,
        11 => PeerErrorKind::Operation,
        255 => PeerErrorKind::Closed,
        _ => PeerErrorKind::Internal,
    }
}
