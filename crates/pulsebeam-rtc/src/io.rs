#![allow(
    clippy::disallowed_types,
    reason = "the v3 public wire-value contract uses immutable Bytes payloads"
)]

use std::{net::SocketAddr, time::Instant};

use bytes::Bytes;
use thiserror::Error;

use crate::IceTcpFlowId;
use crate::{
    DataChannelConfig, DataChannelId, DataChannelPriority, DataMessage, DataReliability,
    EncodingId, ForwardedMedia, MediaKind, MediaPacket, MediaPayloadBitrate, SenderAssignment,
    SenderId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EcnCodepoint {
    NotEct,
    Ect0,
    Ect1,
    Ce,
}

/// Observation of a complete datagram or RFC4571 frame, independent of execution time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketObservation {
    pub at: crate::TimePoint,
    /// Caller-owned opaque value, carried with normalized RTP without interpretation.
    pub provenance: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NetworkInput {
    Udp {
        local: SocketAddr,
        remote: SocketAddr,
        ecn: Option<EcnCodepoint>,
        payload: Bytes,
    },
    IceTcp {
        flow: IceTcpFlowId,
        local: SocketAddr,
        remote: SocketAddr,
        frame: Bytes,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthenticatedFlow {
    Udp {
        local: SocketAddr,
        remote: SocketAddr,
    },
    IceTcp {
        flow: IceTcpFlowId,
        local: SocketAddr,
        remote: SocketAddr,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransmitTarget {
    Udp {
        local: SocketAddr,
        remote: SocketAddr,
        ecn: Option<EcnCodepoint>,
    },
    IceTcp {
        flow: IceTcpFlowId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaTransmitInfo {
    pub sender: SenderId,
    pub global_media_at: crate::GlobalMediaTime,
    pub origin: crate::PacketOrigin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransmitKind {
    Original(MediaTransmitInfo),
    Repair(MediaTransmitInfo),
    Padding { sender: SenderId },
    Control,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transmit {
    pub kind: TransmitKind,
    pub target: TransmitTarget,
    pub payload: Bytes,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum Command {
    /// Register an accepted stream before delivering its first frame. IDs must never be reused.
    AdmitIceTcpFlow {
        flow: IceTcpFlowId,
        local: SocketAddr,
        remote: SocketAddr,
    },
    /// Remove a closed stream before admitting another incarnation of its tuple.
    RetireIceTcpFlow {
        flow: IceTcpFlowId,
    },
    SetSenderAssignment {
        sender: SenderId,
        assignment: Option<SenderAssignment>,
    },
    SendMedia {
        sender: SenderId,
        media: ForwardedMedia,
    },
    /// Recover within the same publication while retaining committed repair history.
    RecoverSenderMedia {
        sender: SenderId,
        assignment: SenderAssignment,
    },
    RequestKeyframe {
        encoding: EncodingId,
    },
    RetireEncoding {
        encoding: EncodingId,
    },
    OpenDataChannel(DataChannelConfig),
    SendData {
        channel: DataChannelId,
        message: DataMessage,
    },
    CloseDataChannel {
        channel: DataChannelId,
    },
    CloseGracefully {
        deadline: Instant,
    },
    Abort,
}

/// Inspect `reason` before retrying or discarding the original owned `command`.
#[derive(Debug, Error)]
#[error("{reason}")]
pub struct RejectedCommand {
    #[source]
    pub reason: CommandError,
    pub command: Command,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum Output {
    Transmit(Transmit),
    Event(Event),
    Idle { next_wakeup: Option<Instant> },
    Closed(CloseReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CloseReason {
    Graceful,
    Aborted,
    TransportFailure,
    Timeout,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum Event {
    Connected,
    IceAuthenticated(AuthenticatedFlow),
    Media {
        encoding: EncodingId,
        packet: MediaPacket,
    },
    EncodingDiscovered(EncodingInfo),
    EncodingRetired {
        encoding: EncodingId,
        reason: EncodingRetireReason,
    },
    /// The peer or local video delivery requires fresh independent media.
    KeyframeRequested {
        sender: SenderId,
        reason: KeyframeRequestReason,
    },
    MediaBudgetChanged(MediaPayloadBitrate),
    DataChannel(DataChannelEvent),
    Warning(ConnectionWarning),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KeyframeRequestReason {
    /// Authenticated peer feedback requests fresh media without abandoning current delivery.
    Peer,
    /// Local expiry or descriptor failure invalidated the sender's dependency chain.
    LocalRecovery,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodingInfo {
    pub id: EncodingId,
    pub sender_index: u32,
    pub kind: MediaKind,
    pub rid: Option<std::sync::Arc<str>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EncodingRetireReason {
    RemoteBye,
    Retired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DataChannelEvent {
    Opened {
        channel: DataChannelId,
        config: DataChannelConfig,
    },
    Message {
        channel: DataChannelId,
        message: DataMessage,
    },
    Closed {
        channel: DataChannelId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ConnectionWarning {
    ClockRegression,
    MediaClockSynchronized,
    MediaClockStale,
    MediaClockDiscontinuous,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum AcceptError {
    #[error("invalid SDP offer")]
    InvalidOffer,
    #[error("unsupported session profile")]
    UnsupportedSessionProfile,
    #[error("session does not negotiate packet feedback")]
    MissingPacketFeedback,
    #[error("session capabilities conflict")]
    CapabilityConflict,
    #[error("session exceeds a configured limit")]
    SessionLimitExceeded,
    #[error("invalid connection configuration")]
    InvalidConfiguration,
    #[error("connection cryptography initialization failed")]
    CryptographicFailure,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ReceiveError {
    #[error("packet observation is later than execution")]
    InvalidObservation,
    #[error("connection is closed")]
    Closed,
    #[error("unknown ICE-TCP flow")]
    UnknownIceTcpFlow,
    #[error("invalid network envelope")]
    InvalidNetworkEnvelope,
    #[error("network input exceeds a configured limit")]
    InputLimitExceeded,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum CommandError {
    #[error("connection is closed")]
    Closed,
    #[error("unknown sender {0:?}")]
    UnknownSender(SenderId),
    #[error("unknown encoding {0:?}")]
    UnknownEncoding(EncodingId),
    #[error("unknown data channel {0:?}")]
    UnknownDataChannel(DataChannelId),
    #[error("sender {0:?} has no source assignment")]
    SenderUnbound(SenderId),
    #[error("invalid sender assignment")]
    InvalidSenderAssignment,
    #[error("media belongs to a different source incarnation")]
    SourceIncarnationMismatch,
    #[error("invalid or conflicting ICE-TCP flow binding")]
    InvalidIceTcpFlow,
    #[error("invalid frame metadata")]
    InvalidFrameMetadata,
    #[error("media has passed its receiver usefulness deadline")]
    MediaExpired,
    #[error("source packets interrupt an incomplete admitted frame")]
    FrameInterrupted,
    #[error("sender dependency chain requires fresh independent media")]
    DependencyChainInterrupted,
    #[error("sender {0:?} needs a fresh receiver to restore browser-default playout")]
    PlayoutResetRequiresNewSender(SenderId),
    #[error("command is invalid in the current state")]
    InvalidState,
    #[error("data channel message is too large")]
    MessageTooLarge,
    #[error("connection has reached a configured buffer limit")]
    WouldBlock,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum ConnectionState {
    #[default]
    Open,
    Closing,
    Closed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatsSnapshot {
    pub connection: ConnectionStats,
    pub senders: Vec<SenderStats>,
    pub encodings: Vec<EncodingStats>,
    pub data_channels: Vec<DataChannelStats>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConnectionStats {
    pub state: ConnectionState,
    pub feedback: Option<crate::PacketFeedbackKind>,
    pub close_reason: Option<CloseReason>,
    pub clock_regressions: u64,
    pub rtp_bytes_in_flight: u64,
    pub media_budget: MediaPayloadBitrate,
    pub pacing_bitrate: u64,
    pub queued_media_bytes: usize,
    pub buffered_data_bytes: usize,
    pub transmitted_rtp_bytes: u64,
    pub transmitted_rtcp_bytes: u64,
    pub transmitted_sctp_bytes: u64,
    pub transmitted_protocol_bytes: u64,
    pub transmitted_padding_bytes: u64,
    pub dropped_network_inputs: u64,
    pub unknown_feedback: u64,
    pub duplicate_feedback: u64,
    pub stale_feedback: u64,
    pub wrong_path_feedback: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayoutDelayStatus {
    Unspecified,
    NotSignaled,
    Pending,
    Confirmed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SenderStats {
    pub sender: SenderId,
    pub assignment: Option<SenderAssignment>,
    pub playout_delay: PlayoutDelayStatus,
    pub queued_packets: usize,
    pub queued_payload_bytes: usize,
    pub transmitted_packets: u64,
    pub transmitted_payload_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodingStats {
    pub encoding: EncodingId,
    pub kind: MediaKind,
    pub received_packets: u64,
    pub retired: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataChannelStats {
    pub channel: DataChannelId,
    pub priority: DataChannelPriority,
    pub reliability: DataReliability,
    pub buffered_amount: usize,
    pub sent_messages: u64,
    pub received_messages: u64,
}
