//! Owning metadata for the SDK's packet socket interface. No packet I/O is
//! performed by these values, and constructing them does not select a provider.
use crate::{EcnMarking, SocketAddress, Timestamp, ffi};
use std::fmt;

/// Rejection of an actual pinned SDK enum or legacy constructor precondition.
/// Unrecognized representable enum values remain usable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketSocketValueError {
    EnumRepresentation,
    LegacyArrivalTime,
}
impl fmt::Display for PacketSocketValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EnumRepresentation => {
                "value is outside the pinned native packet enum representation"
            }
            Self::LegacyArrivalTime => {
                "legacy arrival time must be -1 or a nonnegative finite microsecond value"
            }
        })
    }
}
impl std::error::Error for PacketSocketValueError {}

/// Native DiffServCodePoint, including unrecognized representable enum values.
/// The SDK enum includes -1 and values through 56, with representation -64..63.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct DscpCodePoint(i32);
impl DscpCodePoint {
    pub const NO_CHANGE: Self = Self(-1);
    pub const DEFAULT: Self = Self(0);
    pub const CS0: Self = Self(0);
    pub const CS1: Self = Self(8);
    pub const AF11: Self = Self(10);
    pub const AF12: Self = Self(12);
    pub const AF13: Self = Self(14);
    pub const CS2: Self = Self(16);
    pub const AF21: Self = Self(18);
    pub const AF22: Self = Self(20);
    pub const AF23: Self = Self(22);
    pub const CS3: Self = Self(24);
    pub const AF31: Self = Self(26);
    pub const AF32: Self = Self(28);
    pub const AF33: Self = Self(30);
    pub const CS4: Self = Self(32);
    pub const AF41: Self = Self(34);
    pub const AF42: Self = Self(36);
    pub const AF43: Self = Self(38);
    pub const CS5: Self = Self(40);
    pub const EF: Self = Self(46);
    pub const CS6: Self = Self(48);
    pub const CS7: Self = Self(56);
    pub fn from_native_value(value: i32) -> Result<Self, PacketSocketValueError> {
        (-64..=63)
            .contains(&value)
            .then_some(Self(value))
            .ok_or(PacketSocketValueError::EnumRepresentation)
    }
    pub const fn native_value(self) -> i32 {
        self.0
    }
}
impl Default for DscpCodePoint {
    fn default() -> Self {
        Self::NO_CHANGE
    }
}

/// Native Socket::Option. Values 14 and 15 are unrecognized but representable
/// by the pinned unscoped enum; providers retain their native error policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PacketSocketOption(i32);
impl PacketSocketOption {
    pub const DONT_FRAGMENT: Self = Self(0);
    pub const RECEIVE_BUFFER: Self = Self(1);
    pub const SEND_BUFFER: Self = Self(2);
    pub const NO_DELAY: Self = Self(3);
    pub const IPV6_ONLY: Self = Self(4);
    pub const DSCP: Self = Self(5);
    pub const RTP_SENDTIME_EXTENSION_ID: Self = Self(6);
    pub const SEND_ECN: Self = Self(7);
    pub const RECEIVE_ECN: Self = Self(8);
    pub const KEEP_ALIVE: Self = Self(9);
    pub const TCP_KEEP_COUNT: Self = Self(10);
    pub const TCP_KEEP_IDLE: Self = Self(11);
    pub const TCP_KEEP_INTERVAL: Self = Self(12);
    pub const TCP_USER_TIMEOUT: Self = Self(13);
    pub fn from_native_value(value: i32) -> Result<Self, PacketSocketValueError> {
        (0..=15)
            .contains(&value)
            .then_some(Self(value))
            .ok_or(PacketSocketValueError::EnumRepresentation)
    }
    pub const fn native_value(self) -> i32 {
        self.0
    }
}

/// Native AsyncPacketSocket::State, including representable unrecognized values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct PacketSocketState(i32);
impl PacketSocketState {
    pub const CLOSED: Self = Self(0);
    pub const BINDING: Self = Self(1);
    pub const BOUND: Self = Self(2);
    pub const CONNECTING: Self = Self(3);
    pub const CONNECTED: Self = Self(4);
    pub fn from_native_value(value: i32) -> Result<Self, PacketSocketValueError> {
        (0..=7)
            .contains(&value)
            .then_some(Self(value))
            .ok_or(PacketSocketValueError::EnumRepresentation)
    }
    pub const fn native_value(self) -> i32 {
        self.0
    }
}

/// Open native int flags. No TLS/DTLS combination is silently normalized.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct PacketSocketFlags(pub i32);
impl PacketSocketFlags {
    pub const TLS_FAKE: Self = Self(0x01);
    pub const TLS: Self = Self(0x02);
    pub const STUN: Self = Self(0x04);
    pub const TLS_INSECURE: Self = Self(0x08);
    pub const DTLS_INSECURE: Self = Self(0x10);
    pub const DTLS: Self = Self(0x20);
    pub const SSL_TCP: Self = Self::TLS_FAKE;
}
impl std::ops::BitOr for PacketSocketFlags {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct PacketType(pub i32);
impl PacketType {
    pub const UNKNOWN: Self = Self(0);
    pub const DATA: Self = Self(1);
    pub const ICE_CONNECTIVITY_CHECK: Self = Self(2);
    pub const ICE_CONNECTIVITY_CHECK_RESPONSE: Self = Self(3);
    pub const STUN_MESSAGE: Self = Self(4);
    pub const TURN_MESSAGE: Self = Self(5);
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct PacketInfoProtocol(pub i32);
impl PacketInfoProtocol {
    pub const UNKNOWN: Self = Self(0);
    pub const UDP: Self = Self(1);
    pub const DTLS: Self = Self(2);
    pub const TCP: Self = Self(3);
    pub const SSL_TCP: Self = Self(4);
    pub const TLS: Self = Self(5);
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PacketInfo {
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
impl PacketInfo {
    pub(crate) fn native(&self) -> ffi::FfiSocketPacketInfo {
        ffi::FfiSocketPacketInfo {
            included_in_feedback: self.included_in_feedback,
            included_in_allocation: self.included_in_allocation,
            is_media: self.is_media,
            packet_type: self.packet_type.0,
            protocol: self.protocol.0,
            has_network_id: self.network_id.is_some(),
            network_id: self.network_id.unwrap_or(0),
            packet_size_bytes: self.packet_size_bytes,
            turn_overhead_bytes: self.turn_overhead_bytes,
            ip_overhead_bytes: self.ip_overhead_bytes,
        }
    }
    pub(crate) fn from_native(value: ffi::FfiSocketPacketInfo) -> Self {
        Self {
            included_in_feedback: value.included_in_feedback,
            included_in_allocation: value.included_in_allocation,
            is_media: value.is_media,
            packet_type: PacketType(value.packet_type),
            protocol: PacketInfoProtocol(value.protocol),
            network_id: value.has_network_id.then_some(value.network_id),
            packet_size_bytes: value.packet_size_bytes,
            turn_overhead_bytes: value.turn_overhead_bytes,
            ip_overhead_bytes: value.ip_overhead_bytes,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PacketTimeUpdateParams {
    pub rtp_sendtime_extension_id: i32,
    pub srtp_auth_key: Vec<u8>,
    pub srtp_auth_tag_len: i32,
    pub srtp_packet_index: i64,
}
impl Default for PacketTimeUpdateParams {
    fn default() -> Self {
        Self {
            rtp_sendtime_extension_id: -1,
            srtp_auth_key: Vec::new(),
            srtp_auth_tag_len: -1,
            srtp_packet_index: -1,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AsyncSocketPacketOptions {
    pub dscp: DscpCodePoint,
    pub ect_1: bool,
    pub packet_id: i64,
    pub packet_time_params: PacketTimeUpdateParams,
    pub info_signaled_after_sent: PacketInfo,
    pub batchable: bool,
    pub last_packet_in_batch: bool,
}
impl Default for AsyncSocketPacketOptions {
    fn default() -> Self {
        Self {
            dscp: DscpCodePoint::NO_CHANGE,
            ect_1: false,
            packet_id: -1,
            packet_time_params: PacketTimeUpdateParams::default(),
            info_signaled_after_sent: PacketInfo::default(),
            batchable: false,
            last_packet_in_batch: false,
        }
    }
}
impl AsyncSocketPacketOptions {
    pub fn new(dscp: DscpCodePoint) -> Self {
        Self {
            dscp,
            ..Self::default()
        }
    }
    pub(crate) fn native(&self) -> ffi::FfiSocketPacketOptions {
        ffi::FfiSocketPacketOptions {
            dscp: self.dscp.0,
            ect_1: self.ect_1,
            packet_id: self.packet_id,
            extension_id: self.packet_time_params.rtp_sendtime_extension_id,
            auth_key: self.packet_time_params.srtp_auth_key.clone(),
            auth_tag_len: self.packet_time_params.srtp_auth_tag_len,
            packet_index: self.packet_time_params.srtp_packet_index,
            info: self.info_signaled_after_sent.native(),
            batchable: self.batchable,
            last_packet_in_batch: self.last_packet_in_batch,
        }
    }
    pub(crate) fn from_native(value: ffi::FfiSocketPacketOptions) -> Self {
        Self {
            dscp: DscpCodePoint::from_native_value(value.dscp).expect("native DSCP representation"),
            ect_1: value.ect_1,
            packet_id: value.packet_id,
            packet_time_params: PacketTimeUpdateParams {
                rtp_sendtime_extension_id: value.extension_id,
                srtp_auth_key: value.auth_key,
                srtp_auth_tag_len: value.auth_tag_len,
                srtp_packet_index: value.packet_index,
            },
            info_signaled_after_sent: PacketInfo::from_native(value.info),
            batchable: value.batchable,
            last_packet_in_batch: value.last_packet_in_batch,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SentPacketInfo {
    pub packet_id: i64,
    pub send_time_ms: i64,
    pub info: PacketInfo,
}
impl Default for SentPacketInfo {
    fn default() -> Self {
        Self {
            packet_id: -1,
            send_time_ms: -1,
            info: PacketInfo::default(),
        }
    }
}
impl SentPacketInfo {
    pub(crate) fn native(&self) -> ffi::FfiSocketSentPacket {
        ffi::FfiSocketSentPacket {
            packet_id: self.packet_id,
            send_time_ms: self.send_time_ms,
            info: self.info.native(),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiSocketSentPacket) -> Self {
        Self {
            packet_id: value.packet_id,
            send_time_ms: value.send_time_ms,
            info: PacketInfo::from_native(value.info),
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct PacketDecryptionInfo(u8);
impl PacketDecryptionInfo {
    pub const NOT_DECRYPTED: Self = Self(0);
    pub const DTLS_DECRYPTED: Self = Self(1);
    pub const SRTP_ENCRYPTED: Self = Self(2);
    pub fn from_native_value(value: u8) -> Result<Self, PacketSocketValueError> {
        (value <= 3)
            .then_some(Self(value))
            .ok_or(PacketSocketValueError::EnumRepresentation)
    }
    pub const fn native_value(self) -> u8 {
        self.0
    }
}
/// Deep-owned counterpart of the SDK's borrowed span/address packet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedIpPacket {
    pub payload: Vec<u8>,
    pub source_address: SocketAddress,
    pub arrival_time: Option<Timestamp>,
    pub ecn: EcnMarking,
    pub decryption: PacketDecryptionInfo,
}
impl ReceivedIpPacket {
    pub fn new(payload: Vec<u8>, source_address: SocketAddress) -> Self {
        Self {
            payload,
            source_address,
            arrival_time: None,
            ecn: EcnMarking::NotEct,
            decryption: PacketDecryptionInfo::NOT_DECRYPTED,
        }
    }
    pub fn copy_and_set(&self, decryption: PacketDecryptionInfo) -> Self {
        Self::from_native(ffi::socket_received_packet_copy(
            &self.native(),
            decryption.0,
        ))
    }
    /// Invoke the SDK legacy constructor. `-1` means no arrival time;
    /// nonnegative finite values are microseconds. Other values would reach
    /// the SDK constructor's DCHECKs and are rejected before the native call.
    pub fn from_legacy(
        payload: Vec<u8>,
        source_address: SocketAddress,
        packet_time_us: i64,
    ) -> Result<Self, PacketSocketValueError> {
        if packet_time_us < -1 || packet_time_us == i64::MAX {
            return Err(PacketSocketValueError::LegacyArrivalTime);
        }
        Ok(Self::from_native(ffi::socket_received_packet_legacy(
            &payload,
            source_address.native(),
            packet_time_us,
        )))
    }
    /// The SDK's deprecated overload with its default, unspecified address.
    pub fn from_legacy_without_source(
        payload: Vec<u8>,
        packet_time_us: i64,
    ) -> Result<Self, PacketSocketValueError> {
        Self::from_legacy(payload, SocketAddress::default(), packet_time_us)
    }
    pub(crate) fn native(&self) -> ffi::FfiSocketReceivedPacket {
        ffi::FfiSocketReceivedPacket {
            payload: self.payload.clone(),
            address: ffi::socket_address_clone(self.source_address.native()),
            has_arrival: self.arrival_time.is_some(),
            arrival_us: self
                .arrival_time
                .map_or(0, Timestamp::native_microsecond_bits),
            ecn: self.ecn.native_bits(),
            decryption: self.decryption.0,
        }
    }
    pub(crate) fn from_native(value: ffi::FfiSocketReceivedPacket) -> Self {
        Self {
            payload: value.payload,
            source_address: SocketAddress::from_native(value.address),
            arrival_time: value
                .has_arrival
                .then(|| Timestamp::from_native_microsecond_bits(value.arrival_us)),
            ecn: EcnMarking::from(value.ecn),
            decryption: PacketDecryptionInfo(value.decryption),
        }
    }
}
