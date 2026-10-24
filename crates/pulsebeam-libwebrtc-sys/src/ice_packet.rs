//! Packet operations on the revocable internal transport, never a socket owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IcePacketDecryption {
    NotDecrypted,
    DtlsDecrypted,
    SrtpEncrypted,
    Other(i32),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IceReceivedPacket {
    pub payload: Vec<u8>,
    pub source: crate::SocketAddress,
    pub arrival_time: Option<crate::Timestamp>,
    pub ecn: crate::EcnMarking,
    pub decryption: IcePacketDecryption,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IceSentPacket {
    pub packet_id: i64,
    pub send_time_ms: i64,
    pub info: IcePacketInfo,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IcePacketEvent {
    WritableState(bool),
    ReadyToSend,
    GatheringState(IceTransportGatheringState),
    Received(IceReceivedPacket),
    Sent(IceSentPacket),
    NetworkRoute(Option<IceNetworkRoute>),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IcePacketObserverError {
    ZeroCapacity,
    AlreadyRegistered,
    Unavailable,
}
impl fmt::Display for IcePacketObserverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => f.write_str("ICE observer capacity must be nonzero"),
            Self::AlreadyRegistered => {
                f.write_str("ICE packet observer already registered on this alias")
            }
            Self::Unavailable => IceInternalUnavailable.fmt(f),
        }
    }
}
impl std::error::Error for IcePacketObserverError {}
impl IceTransportInternal {
    /// Register only SDK callbacks with an actual native removal API. Shares
    /// one registration across clones, snapshots borrowed packet bytes and
    /// addresses in the callback, and drops oldest events on capacity overflow.
    /// Engine-owned candidate/error/pair hooks remain peer observations.
    pub fn register_packet_observer(&self, capacity: usize) -> Result<(), IcePacketObserverError> {
        match ffi::ice_transport_register_packet_observer(self.0.native(), capacity) {
            0 => Ok(()),
            1 => Err(IcePacketObserverError::ZeroCapacity),
            2 => Err(IcePacketObserverError::Unavailable),
            3 => Err(IcePacketObserverError::AlreadyRegistered),
            _ => unreachable!("unknown native observer status"),
        }
    }
    /// Retire subscriptions and discard the queue on the network role. If
    /// already revoked, the native callback lists have gone; the queue is still
    /// cleared, but no successful native unsubscription is fabricated.
    pub fn unregister_packet_observer(&self) -> Result<(), IceInternalUnavailable> {
        available(ffi::ice_transport_unregister_packet_observer(
            self.0.native(),
        ))
    }
    /// Queued copied observations remain consumable after ICE revocation.
    pub fn take_packet_event(&self) -> Option<IcePacketEvent> {
        let e = ffi::ice_transport_packet_event(self.0.native());
        Some(match e.kind {
            0 => return None,
            1 => IcePacketEvent::WritableState(e.flag),
            2 => IcePacketEvent::ReadyToSend,
            3 => IcePacketEvent::GatheringState(IceTransportGatheringState::from_native(e.state)),
            4 => IcePacketEvent::Received(IceReceivedPacket {
                payload: e.payload,
                source: crate::SocketAddress::from_native(e.source),
                arrival_time: e
                    .has_arrival_time
                    .then(|| crate::Timestamp::from_native_microsecond_bits(e.arrival_time)),
                ecn: crate::EcnMarking::from(e.ecn as u8),
                decryption: match e.decryption {
                    0 => IcePacketDecryption::NotDecrypted,
                    1 => IcePacketDecryption::DtlsDecrypted,
                    2 => IcePacketDecryption::SrtpEncrypted,
                    v => IcePacketDecryption::Other(v),
                },
            }),
            5 => IcePacketEvent::Sent(IceSentPacket {
                packet_id: e.packet_id,
                send_time_ms: e.send_time_ms,
                info: IcePacketInfo::from_native(e.info),
            }),
            6 => IcePacketEvent::NetworkRoute(IceNetworkRoute::from_native(e.route)),
            _ => unreachable!("unknown native packet event"),
        })
    }
    pub fn take_dropped_packet_events(&self) -> u64 {
        ffi::ice_transport_packet_dropped(self.0.native())
    }
}

use super::*;
/// Six-bit DSCP value or the SDK's -1 "no change" sentinel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Dscp(i32);
impl Dscp {
    pub const NO_CHANGE: Self = Self(-1);
    pub const DEFAULT: Self = Self(0);
    pub const fn new(value: u8) -> Option<Self> {
        if value <= 63 {
            Some(Self(value as i32))
        } else {
            None
        }
    }
    pub const fn native_value(self) -> i32 {
        self.0
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum PacketType {
    Unknown,
    Data,
    IceConnectivityCheck,
    IceConnectivityCheckResponse,
    StunMessage,
    TurnMessage,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum PacketInfoProtocol {
    Unknown,
    Udp,
    Dtls,
    Tcp,
    SslTcp,
    Tls,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IcePacketInfo {
    pub included_in_feedback: bool,
    pub included_in_allocation: bool,
    pub is_media: bool,
    pub packet_type: PacketType,
    pub protocol: PacketInfoProtocol,
    pub network_id: Option<u16>,
    pub packet_size_bytes: usize,
    pub turn_overhead_bytes: usize,
    pub ip_overhead_bytes: usize,
}
impl IcePacketInfo {
    fn from_native(v: ffi::FfiIcePacketInfo) -> Self {
        Self {
            included_in_feedback: v.included_in_feedback,
            included_in_allocation: v.included_in_allocation,
            is_media: v.is_media,
            packet_type: match v.packet_type {
                0 => PacketType::Unknown,
                1 => PacketType::Data,
                2 => PacketType::IceConnectivityCheck,
                3 => PacketType::IceConnectivityCheckResponse,
                4 => PacketType::StunMessage,
                5 => PacketType::TurnMessage,
                _ => unreachable!("unknown pinned packet type"),
            },
            protocol: match v.protocol {
                0 => PacketInfoProtocol::Unknown,
                1 => PacketInfoProtocol::Udp,
                2 => PacketInfoProtocol::Dtls,
                3 => PacketInfoProtocol::Tcp,
                4 => PacketInfoProtocol::SslTcp,
                5 => PacketInfoProtocol::Tls,
                _ => unreachable!("unknown pinned packet protocol"),
            },
            network_id: v.has_network_id.then_some(v.network_id),
            packet_size_bytes: v.packet_size_bytes,
            turn_overhead_bytes: v.turn_overhead_bytes,
            ip_overhead_bytes: v.ip_overhead_bytes,
        }
    }
    fn native(&self) -> ffi::FfiIcePacketInfo {
        ffi::FfiIcePacketInfo {
            included_in_feedback: self.included_in_feedback,
            included_in_allocation: self.included_in_allocation,
            is_media: self.is_media,
            packet_type: self.packet_type as i32,
            protocol: self.protocol as i32,
            has_network_id: self.network_id.is_some(),
            network_id: self.network_id.unwrap_or(0),
            packet_size_bytes: self.packet_size_bytes,
            turn_overhead_bytes: self.turn_overhead_bytes,
            ip_overhead_bytes: self.ip_overhead_bytes,
        }
    }
}
/// SDK rewriting/authentication metadata. These fields refer to the actual
/// RTP/SRTP packet being sent; they do not synthesize or packetize that packet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IcePacketTimeUpdate {
    pub rtp_sendtime_extension_id: i32,
    pub srtp_auth_key: Vec<u8>,
    pub srtp_auth_tag_len: i32,
    pub srtp_packet_index: i64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IcePacketOptions {
    pub dscp: Dscp,
    pub ect_1: bool,
    pub packet_id: i64,
    pub packet_time: IcePacketTimeUpdate,
    pub info_signaled_after_sent: IcePacketInfo,
    pub batchable: bool,
    pub last_packet_in_batch: bool,
}
impl Default for IcePacketOptions {
    fn default() -> Self {
        let v = ffi::ice_packet_options_default();
        Self {
            dscp: Dscp(v.dscp),
            ect_1: v.ect_1,
            packet_id: v.packet_id,
            packet_time: IcePacketTimeUpdate {
                rtp_sendtime_extension_id: v.rtp_sendtime_extension_id,
                srtp_auth_key: v.srtp_auth_key,
                srtp_auth_tag_len: v.srtp_auth_tag_len,
                srtp_packet_index: v.srtp_packet_index,
            },
            info_signaled_after_sent: IcePacketInfo::from_native(v.info),
            batchable: v.batchable,
            last_packet_in_batch: v.last_packet_in_batch,
        }
    }
}
impl IcePacketOptions {
    fn native(&self) -> ffi::FfiIcePacketOptions {
        ffi::FfiIcePacketOptions {
            dscp: self.dscp.native_value(),
            ect_1: self.ect_1,
            packet_id: self.packet_id,
            rtp_sendtime_extension_id: self.packet_time.rtp_sendtime_extension_id,
            srtp_auth_key: self.packet_time.srtp_auth_key.clone(),
            srtp_auth_tag_len: self.packet_time.srtp_auth_tag_len,
            srtp_packet_index: self.packet_time.srtp_packet_index,
            info: self.info_signaled_after_sent.native(),
            batchable: self.batchable,
            last_packet_in_batch: self.last_packet_in_batch,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum IceSocketOption {
    DontFragment,
    ReceiveBuffer,
    SendBuffer,
    NoDelay,
    Ipv6Only,
    Dscp,
    RtpSendTimeExtensionId,
    SendEcn,
    ReceiveEcn,
    KeepAlive,
    TcpKeepCount,
    TcpKeepIdle,
    TcpKeepInterval,
    TcpUserTimeout,
}
/// Exact native return and last channel error, sampled in the same network task.
/// Last error is not guaranteed to describe a failed option/send operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IceSocketOperationResult {
    pub return_value: i32,
    pub last_error: i32,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IcePacketState {
    pub transport_name: Vec<u8>,
    pub writable: bool,
    pub receiving: bool,
    pub last_error: i32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IceRouteEndpoint {
    pub adapter_type: crate::NetworkAdapterType,
    pub adapter_id: u16,
    pub network_id: u16,
    pub uses_turn: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IceNetworkRoute {
    pub connected: bool,
    pub local: IceRouteEndpoint,
    pub remote: IceRouteEndpoint,
    pub last_sent_packet_id: i32,
    pub packet_overhead: i32,
}
impl IceNetworkRoute {
    fn from_native(v: ffi::FfiIceRoute) -> Option<Self> {
        fn endpoint(v: ffi::FfiIceRouteEndpoint) -> IceRouteEndpoint {
            IceRouteEndpoint {
                adapter_type: adapter(v.adapter_type),
                adapter_id: v.adapter_id,
                network_id: v.network_id,
                uses_turn: v.uses_turn,
            }
        }
        v.present.then(|| Self {
            connected: v.connected,
            local: endpoint(v.local),
            remote: endpoint(v.remote),
            last_sent_packet_id: v.last_sent_packet_id,
            packet_overhead: v.packet_overhead,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IcePacketSendError {
    Unavailable(IceInternalUnavailable),
    EmptyPacket,
}
impl fmt::Display for IcePacketSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(e) => e.fmt(f),
            Self::EmptyPacket => {
                f.write_str("native P2P packet send does not support a zero-byte successful send")
            }
        }
    }
}
impl std::error::Error for IcePacketSendError {}
fn socket_result(
    v: ffi::FfiIceSocketResult,
) -> Result<IceSocketOperationResult, IceInternalUnavailable> {
    available(v.available)?;
    Ok(IceSocketOperationResult {
        return_value: v.result,
        last_error: v.error,
    })
}
impl IceTransportInternal {
    pub fn packet_state(&self) -> Result<IcePacketState, IceInternalUnavailable> {
        let v = ffi::ice_transport_packet_state(self.0.native());
        available(v.available)?;
        Ok(IcePacketState {
            transport_name: v.name,
            writable: v.writable,
            receiving: v.receiving,
            last_error: v.error,
        })
    }
    pub fn transport_name(&self) -> Result<Vec<u8>, IceInternalUnavailable> {
        Ok(self.packet_state()?.transport_name)
    }
    pub fn writable(&self) -> Result<bool, IceInternalUnavailable> {
        Ok(self.packet_state()?.writable)
    }
    pub fn receiving(&self) -> Result<bool, IceInternalUnavailable> {
        Ok(self.packet_state()?.receiving)
    }
    pub fn last_error(&self) -> Result<i32, IceInternalUnavailable> {
        Ok(self.packet_state()?.last_error)
    }
    pub fn send_packet(
        &self,
        data: &[u8],
        options: &IcePacketOptions,
        flags: i32,
    ) -> Result<IceSocketOperationResult, IcePacketSendError> {
        if data.is_empty() && flags == 0 {
            return Err(IcePacketSendError::EmptyPacket);
        }
        socket_result(ffi::ice_transport_send_packet(
            self.0.native(),
            data,
            &options.native(),
            flags,
        ))
        .map_err(IcePacketSendError::Unavailable)
    }
    pub fn set_option(
        &self,
        option: IceSocketOption,
        value: i32,
    ) -> Result<IceSocketOperationResult, IceInternalUnavailable> {
        socket_result(ffi::ice_transport_set_option(
            self.0.native(),
            option as u8,
            value,
        ))
    }
    pub fn get_option(
        &self,
        option: IceSocketOption,
    ) -> Result<Option<i32>, IceInternalUnavailable> {
        let v = ffi::ice_transport_get_option(self.0.native(), option as u8);
        available(v.available)?;
        Ok(v.success.then_some(v.value))
    }
    pub fn network_route(&self) -> Result<Option<IceNetworkRoute>, IceInternalUnavailable> {
        let v = ffi::ice_transport_route(self.0.native());
        available(v.available)?;
        Ok(IceNetworkRoute::from_native(v.route))
    }
}
