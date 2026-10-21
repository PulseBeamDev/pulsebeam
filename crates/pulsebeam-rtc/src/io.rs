#![allow(
    clippy::disallowed_types,
    reason = "owned network and RTP values use immutable Bytes and negotiated URI strings"
)]

use crate::{
    DataChannelConfig, DataChannelId, DataChannelPriority, DataMessage, DataReliability, MediaKind,
    MediaPayloadBitrate, RtpPacket, RtpWrite, SenderReport,
};
use bytes::Bytes;
use std::{net::SocketAddr, sync::Arc, time::Instant};
use thiserror::Error;

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
    /// One complete RFC4571 frame. The caller filters retired socket incarnations.
    IceTcp {
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
    /// Bytes belong to the currently selected socket at this tuple and must not
    /// be redirected to a replacement stream after partial writes.
    IceTcp {
        local: SocketAddr,
        remote: SocketAddr,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkProtocol {
    Udp,
    IceTcp,
}

/// A tuple authenticated by ICE, attributed to the input that proved it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthenticatedPath {
    pub protocol: NetworkProtocol,
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub arrival: Instant,
}

/// Authenticated SR/RR reception progress for a negotiated primary sender.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiverReport {
    pub mid: Arc<str>,
    pub ssrc: u32,
    pub arrival: Instant,
    /// RFC 3550 extended highest sequence, modulo 2^32. Not a local send counter.
    pub extended_highest_sequence: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransmitKind {
    Original { ssrc: u32 },
    Repair { ssrc: u32 },
    Padding { ssrc: u32 },
    Control,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transmit {
    pub kind: TransmitKind,
    pub target: TransmitTarget,
    pub payload: Bytes,
}

#[derive(Debug, Error)]
#[error("{reason}")]
pub struct RejectedRtp {
    pub reason: WriteError,
    pub packet: RtpWrite,
}

#[derive(Debug, Error)]
#[error("{reason}")]
pub struct RejectedData {
    pub reason: WriteError,
    pub message: DataMessage,
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
    AuthenticatedPath(AuthenticatedPath),
    ReceiverReport(ReceiverReport),
    Rtp {
        mid: Arc<str>,
        rid: Option<Arc<str>>,
        packet: RtpPacket,
    },
    SenderReport(SenderReport),
    EncodingDiscovered(EncodingInfo),
    EncodingRetired {
        ssrc: u32,
        reason: EncodingRetireReason,
    },
    KeyframeRequested {
        ssrc: u32,
    },
    BandwidthEstimate(MediaPayloadBitrate),
    DataChannel(DataChannelEvent),
    Warning(ConnectionWarning),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodingInfo {
    pub mid: Arc<str>,
    pub ssrc: u32,
    pub kind: MediaKind,
    pub rid: Option<Arc<str>>,
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
    #[error("packet arrival is later than execution")]
    InvalidObservation,
    #[error("connection is closed")]
    Closed,
    #[error("invalid network envelope")]
    InvalidNetworkEnvelope,
    #[error("network input exceeds a configured limit")]
    InputLimitExceeded,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum WriteError {
    #[error("connection is closed")]
    Closed,
    #[error("unknown negotiated RTP stream")]
    UnknownStream,
    #[error("unknown RTP encoding")]
    UnknownEncoding,
    #[error("unknown data channel {0:?}")]
    UnknownDataChannel(DataChannelId),
    #[error("invalid RTP write or extension value")]
    InvalidRtp,
    #[error("RTP index exceeds the SRTP key lifetime")]
    IndexExhausted,
    #[error("operation is invalid in the current state")]
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
    pub bandwidth_estimate: MediaPayloadBitrate,
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
    pub mid: Arc<str>,
    pub ssrc: u32,
    pub queued_packets: usize,
    pub queued_payload_bytes: usize,
    pub transmitted_packets: u64,
    pub transmitted_payload_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodingStats {
    pub mid: Arc<str>,
    pub ssrc: Option<u32>,
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
