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
    EncodingId, ForwardedMedia, MediaKind, MediaPacket, MediaPayloadBitrate, PolicyError, SenderId,
    SenderPolicy,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EcnCodepoint {
    NotEct,
    Ect0,
    Ect1,
    Ce,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transmit {
    pub target: TransmitTarget,
    pub payload: Bytes,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum Command {
    SetSenderPolicy {
        sender: SenderId,
        policy: SenderPolicy,
    },
    SendMedia {
        sender: SenderId,
        media: ForwardedMedia,
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
    Media {
        encoding: EncodingId,
        packet: MediaPacket,
    },
    EncodingDiscovered(EncodingInfo),
    EncodingRetired {
        encoding: EncodingId,
        reason: EncodingRetireReason,
    },
    KeyframeRequested {
        sender: SenderId,
    },
    AllocationChanged(AllocationSnapshot),
    DataChannel(DataChannelEvent),
    Warning(ConnectionWarning),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodingInfo {
    pub id: EncodingId,
    pub kind: MediaKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EncodingRetireReason {
    RemoteBye,
    Retired,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AllocationSnapshot {
    pub total: MediaPayloadBitrate,
    pub senders: Vec<SenderAllocation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SenderAllocation {
    pub sender: SenderId,
    pub bitrate: MediaPayloadBitrate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DataChannelEvent {
    Opened {
        channel: DataChannelId,
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
    #[error("invalid sender policy: {0:?}")]
    InvalidPolicy(PolicyError),
    #[error("invalid frame metadata")]
    InvalidFrameMetadata,
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
    pub target_media_bitrate: MediaPayloadBitrate,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SenderStats {
    pub sender: SenderId,
    pub policy: SenderPolicy,
    pub allocation: MediaPayloadBitrate,
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
