//! Owned pinned network-control values. Optional presence, signed times,
//! infinity states, packet vectors and every declared field remain distinct.
use crate::{DataRate, TimeDelta, Timestamp, ffi};

/// Native byte count. Finite native construction requires nonnegative values
/// strictly between its sentinel endpoints; both infinities remain expressible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataSize {
    Bytes(i64),
    PositiveInfinity,
    NegativeInfinity,
}
impl Default for DataSize {
    fn default() -> Self {
        Self::Bytes(0)
    }
}
impl DataSize {
    pub(crate) fn from_native(value: i64, kind: i8) -> Self {
        match kind {
            1 => Self::PositiveInfinity,
            -1 => Self::NegativeInfinity,
            _ => Self::Bytes(value),
        }
    }
}
/// Full scoped native ECN marking domain, including unknown uint8 values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EcnMarking {
    #[default]
    NotEct,
    Ect1,
    Ect0,
    CongestionExperienced,
    Unknown(u8),
}
impl EcnMarking {
    pub fn native_bits(self) -> u8 {
        match self {
            Self::NotEct => 0,
            Self::Ect1 => 1,
            Self::Ect0 => 2,
            Self::CongestionExperienced => 3,
            Self::Unknown(value) => value,
        }
    }
}
impl From<u8> for EcnMarking {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::NotEct,
            1 => Self::Ect1,
            2 => Self::Ect0,
            3 => Self::CongestionExperienced,
            value => Self::Unknown(value),
        }
    }
}
fn rate_native(value: DataRate) -> ffi::FfiNcMagnitude {
    match value {
        DataRate::BitsPerSecond(value) => ffi::FfiNcMagnitude { value, kind: 0 },
        DataRate::PositiveInfinity => ffi::FfiNcMagnitude { value: 0, kind: 1 },
        DataRate::NegativeInfinity => ffi::FfiNcMagnitude { value: 0, kind: -1 },
    }
}
fn size_native(value: DataSize) -> ffi::FfiNcMagnitude {
    match value {
        DataSize::Bytes(value) => ffi::FfiNcMagnitude { value, kind: 0 },
        DataSize::PositiveInfinity => ffi::FfiNcMagnitude { value: 0, kind: 1 },
        DataSize::NegativeInfinity => ffi::FfiNcMagnitude { value: 0, kind: -1 },
    }
}

/// Owned native BitrateAllocationLimits payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct BitrateAllocationLimits {
    pub min_allocatable_rate: DataRate,
    pub max_allocatable_rate: DataRate,
    pub max_padding_rate: DataRate,
}
impl Default for BitrateAllocationLimits {
    fn default() -> Self {
        Self {
            min_allocatable_rate: DataRate::BitsPerSecond(0),
            max_allocatable_rate: DataRate::BitsPerSecond(0),
            max_padding_rate: DataRate::BitsPerSecond(0),
        }
    }
}
impl BitrateAllocationLimits {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcBitrateAllocationLimits {
        ffi::FfiNcBitrateAllocationLimits {
            min_allocatable_rate: rate_native(self.min_allocatable_rate),
            max_allocatable_rate: rate_native(self.max_allocatable_rate),
            max_padding_rate: rate_native(self.max_padding_rate),
        }
    }
    #[cfg(test)]
    pub(crate) fn from_native(value: ffi::FfiNcBitrateAllocationLimits) -> Self {
        Self {
            min_allocatable_rate: DataRate::from_native(
                value.min_allocatable_rate.value,
                value.min_allocatable_rate.kind,
            ),
            max_allocatable_rate: DataRate::from_native(
                value.max_allocatable_rate.value,
                value.max_allocatable_rate.kind,
            ),
            max_padding_rate: DataRate::from_native(
                value.max_padding_rate.value,
                value.max_padding_rate.kind,
            ),
        }
    }
}

/// Owned native StreamsConfig payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamsConfig {
    pub at_time: Timestamp,
    pub requests_alr_probing: Option<bool>,
    pub enable_repeated_initial_probing: bool,
    pub pacing_factor: Option<f64>,
    pub min_total_allocated_bitrate: Option<DataRate>,
    pub max_padding_rate: Option<DataRate>,
    pub max_total_allocated_bitrate: Option<DataRate>,
}
impl Default for StreamsConfig {
    fn default() -> Self {
        Self {
            at_time: Timestamp::plus_infinity(),
            requests_alr_probing: None,
            enable_repeated_initial_probing: false,
            pacing_factor: None,
            min_total_allocated_bitrate: None,
            max_padding_rate: None,
            max_total_allocated_bitrate: None,
        }
    }
}
impl StreamsConfig {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcStreamsConfig {
        ffi::FfiNcStreamsConfig {
            at_time: self.at_time.native_microsecond_bits(),
            has_requests_alr_probing: self.requests_alr_probing.is_some(),
            requests_alr_probing: self
                .requests_alr_probing
                .as_ref()
                .map(|value| *value)
                .unwrap_or_default(),
            enable_repeated_initial_probing: self.enable_repeated_initial_probing,
            has_pacing_factor: self.pacing_factor.is_some(),
            pacing_factor: self
                .pacing_factor
                .as_ref()
                .map(|value| *value)
                .unwrap_or_default(),
            has_min_total_allocated_bitrate: self.min_total_allocated_bitrate.is_some(),
            min_total_allocated_bitrate: self
                .min_total_allocated_bitrate
                .as_ref()
                .map(|value| rate_native(*value))
                .unwrap_or_default(),
            has_max_padding_rate: self.max_padding_rate.is_some(),
            max_padding_rate: self
                .max_padding_rate
                .as_ref()
                .map(|value| rate_native(*value))
                .unwrap_or_default(),
            has_max_total_allocated_bitrate: self.max_total_allocated_bitrate.is_some(),
            max_total_allocated_bitrate: self
                .max_total_allocated_bitrate
                .as_ref()
                .map(|value| rate_native(*value))
                .unwrap_or_default(),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcStreamsConfig) -> Self {
        Self {
            at_time: Timestamp::from_native_microsecond_bits(value.at_time),
            requests_alr_probing: if value.has_requests_alr_probing {
                Some(value.requests_alr_probing)
            } else {
                None
            },
            enable_repeated_initial_probing: value.enable_repeated_initial_probing,
            pacing_factor: if value.has_pacing_factor {
                Some(value.pacing_factor)
            } else {
                None
            },
            min_total_allocated_bitrate: if value.has_min_total_allocated_bitrate {
                Some(DataRate::from_native(
                    value.min_total_allocated_bitrate.value,
                    value.min_total_allocated_bitrate.kind,
                ))
            } else {
                None
            },
            max_padding_rate: if value.has_max_padding_rate {
                Some(DataRate::from_native(
                    value.max_padding_rate.value,
                    value.max_padding_rate.kind,
                ))
            } else {
                None
            },
            max_total_allocated_bitrate: if value.has_max_total_allocated_bitrate {
                Some(DataRate::from_native(
                    value.max_total_allocated_bitrate.value,
                    value.max_total_allocated_bitrate.kind,
                ))
            } else {
                None
            },
        }
    }
}

/// Owned native TargetRateConstraints payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct TargetRateConstraints {
    pub at_time: Timestamp,
    pub min_data_rate: Option<DataRate>,
    pub max_data_rate: Option<DataRate>,
    pub starting_rate: Option<DataRate>,
}
impl Default for TargetRateConstraints {
    fn default() -> Self {
        Self {
            at_time: Timestamp::plus_infinity(),
            min_data_rate: None,
            max_data_rate: None,
            starting_rate: None,
        }
    }
}
impl TargetRateConstraints {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcTargetRateConstraints {
        ffi::FfiNcTargetRateConstraints {
            at_time: self.at_time.native_microsecond_bits(),
            has_min_data_rate: self.min_data_rate.is_some(),
            min_data_rate: self
                .min_data_rate
                .as_ref()
                .map(|value| rate_native(*value))
                .unwrap_or_default(),
            has_max_data_rate: self.max_data_rate.is_some(),
            max_data_rate: self
                .max_data_rate
                .as_ref()
                .map(|value| rate_native(*value))
                .unwrap_or_default(),
            has_starting_rate: self.starting_rate.is_some(),
            starting_rate: self
                .starting_rate
                .as_ref()
                .map(|value| rate_native(*value))
                .unwrap_or_default(),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcTargetRateConstraints) -> Self {
        Self {
            at_time: Timestamp::from_native_microsecond_bits(value.at_time),
            min_data_rate: if value.has_min_data_rate {
                Some(DataRate::from_native(
                    value.min_data_rate.value,
                    value.min_data_rate.kind,
                ))
            } else {
                None
            },
            max_data_rate: if value.has_max_data_rate {
                Some(DataRate::from_native(
                    value.max_data_rate.value,
                    value.max_data_rate.kind,
                ))
            } else {
                None
            },
            starting_rate: if value.has_starting_rate {
                Some(DataRate::from_native(
                    value.starting_rate.value,
                    value.starting_rate.kind,
                ))
            } else {
                None
            },
        }
    }
}

/// Owned native NetworkAvailability payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkAvailability {
    pub at_time: Timestamp,
    pub network_available: bool,
}
impl Default for NetworkAvailability {
    fn default() -> Self {
        Self {
            at_time: Timestamp::plus_infinity(),
            network_available: false,
        }
    }
}
impl NetworkAvailability {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcNetworkAvailability {
        ffi::FfiNcNetworkAvailability {
            at_time: self.at_time.native_microsecond_bits(),
            network_available: self.network_available,
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcNetworkAvailability) -> Self {
        Self {
            at_time: Timestamp::from_native_microsecond_bits(value.at_time),
            network_available: value.network_available,
        }
    }
}

/// Owned native NetworkRouteChange payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkRouteChange {
    pub at_time: Timestamp,
    pub constraints: TargetRateConstraints,
    pub restart_bwe: bool,
}
impl Default for NetworkRouteChange {
    fn default() -> Self {
        Self {
            at_time: Timestamp::plus_infinity(),
            constraints: TargetRateConstraints::default(),
            restart_bwe: true,
        }
    }
}
impl NetworkRouteChange {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcNetworkRouteChange {
        ffi::FfiNcNetworkRouteChange {
            at_time: self.at_time.native_microsecond_bits(),
            constraints: self.constraints.native(),
            restart_bwe: self.restart_bwe,
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcNetworkRouteChange) -> Self {
        Self {
            at_time: Timestamp::from_native_microsecond_bits(value.at_time),
            constraints: TargetRateConstraints::from_native(value.constraints),
            restart_bwe: value.restart_bwe,
        }
    }
}

/// Owned native PacedPacketInfo payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct PacedPacketInfo {
    pub send_bitrate: DataRate,
    pub probe_cluster_id: i32,
    pub probe_cluster_min_probes: i32,
    pub probe_cluster_min_bytes: i32,
    pub probe_cluster_bytes_sent: i32,
}
impl Default for PacedPacketInfo {
    fn default() -> Self {
        Self {
            send_bitrate: DataRate::BitsPerSecond(0),
            probe_cluster_id: -1,
            probe_cluster_min_probes: -1,
            probe_cluster_min_bytes: -1,
            probe_cluster_bytes_sent: 0,
        }
    }
}
impl PacedPacketInfo {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcPacedPacketInfo {
        ffi::FfiNcPacedPacketInfo {
            send_bitrate: rate_native(self.send_bitrate),
            probe_cluster_id: self.probe_cluster_id,
            probe_cluster_min_probes: self.probe_cluster_min_probes,
            probe_cluster_min_bytes: self.probe_cluster_min_bytes,
            probe_cluster_bytes_sent: self.probe_cluster_bytes_sent,
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcPacedPacketInfo) -> Self {
        Self {
            send_bitrate: DataRate::from_native(value.send_bitrate.value, value.send_bitrate.kind),
            probe_cluster_id: value.probe_cluster_id,
            probe_cluster_min_probes: value.probe_cluster_min_probes,
            probe_cluster_min_bytes: value.probe_cluster_min_bytes,
            probe_cluster_bytes_sent: value.probe_cluster_bytes_sent,
        }
    }
}

/// Owned native SentPacket payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct SentPacket {
    pub send_time: Timestamp,
    pub size: DataSize,
    pub prior_unacked_data: DataSize,
    pub pacing_info: PacedPacketInfo,
    pub audio: bool,
    pub sequence_number: i64,
    pub data_in_flight: DataSize,
}
impl Default for SentPacket {
    fn default() -> Self {
        Self {
            send_time: Timestamp::plus_infinity(),
            size: DataSize::Bytes(0),
            prior_unacked_data: DataSize::Bytes(0),
            pacing_info: PacedPacketInfo::default(),
            audio: false,
            sequence_number: 0,
            data_in_flight: DataSize::Bytes(0),
        }
    }
}
impl SentPacket {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcSentPacket {
        ffi::FfiNcSentPacket {
            send_time: self.send_time.native_microsecond_bits(),
            size: size_native(self.size),
            prior_unacked_data: size_native(self.prior_unacked_data),
            pacing_info: self.pacing_info.native(),
            audio: self.audio,
            sequence_number: self.sequence_number,
            data_in_flight: size_native(self.data_in_flight),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcSentPacket) -> Self {
        Self {
            send_time: Timestamp::from_native_microsecond_bits(value.send_time),
            size: DataSize::from_native(value.size.value, value.size.kind),
            prior_unacked_data: DataSize::from_native(
                value.prior_unacked_data.value,
                value.prior_unacked_data.kind,
            ),
            pacing_info: PacedPacketInfo::from_native(value.pacing_info),
            audio: value.audio,
            sequence_number: value.sequence_number,
            data_in_flight: DataSize::from_native(
                value.data_in_flight.value,
                value.data_in_flight.kind,
            ),
        }
    }
}

/// Owned native ReceivedPacket payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct ReceivedPacket {
    pub send_time: Timestamp,
    pub receive_time: Timestamp,
    pub size: DataSize,
}
impl Default for ReceivedPacket {
    fn default() -> Self {
        Self {
            send_time: Timestamp::minus_infinity(),
            receive_time: Timestamp::plus_infinity(),
            size: DataSize::Bytes(0),
        }
    }
}
impl ReceivedPacket {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcReceivedPacket {
        ffi::FfiNcReceivedPacket {
            send_time: self.send_time.native_microsecond_bits(),
            receive_time: self.receive_time.native_microsecond_bits(),
            size: size_native(self.size),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcReceivedPacket) -> Self {
        Self {
            send_time: Timestamp::from_native_microsecond_bits(value.send_time),
            receive_time: Timestamp::from_native_microsecond_bits(value.receive_time),
            size: DataSize::from_native(value.size.value, value.size.kind),
        }
    }
}

/// Owned native RemoteBitrateReport payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteBitrateReport {
    pub receive_time: Timestamp,
    pub bandwidth: DataRate,
}
impl Default for RemoteBitrateReport {
    fn default() -> Self {
        Self {
            receive_time: Timestamp::plus_infinity(),
            bandwidth: DataRate::PositiveInfinity,
        }
    }
}
impl RemoteBitrateReport {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcRemoteBitrateReport {
        ffi::FfiNcRemoteBitrateReport {
            receive_time: self.receive_time.native_microsecond_bits(),
            bandwidth: rate_native(self.bandwidth),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcRemoteBitrateReport) -> Self {
        Self {
            receive_time: Timestamp::from_native_microsecond_bits(value.receive_time),
            bandwidth: DataRate::from_native(value.bandwidth.value, value.bandwidth.kind),
        }
    }
}

/// Owned native RoundTripTimeUpdate payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct RoundTripTimeUpdate {
    pub receive_time: Timestamp,
    pub round_trip_time: TimeDelta,
    pub smoothed: bool,
}
impl Default for RoundTripTimeUpdate {
    fn default() -> Self {
        Self {
            receive_time: Timestamp::plus_infinity(),
            round_trip_time: TimeDelta::plus_infinity(),
            smoothed: false,
        }
    }
}
impl RoundTripTimeUpdate {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcRoundTripTimeUpdate {
        ffi::FfiNcRoundTripTimeUpdate {
            receive_time: self.receive_time.native_microsecond_bits(),
            round_trip_time: self.round_trip_time.native_microsecond_bits(),
            smoothed: self.smoothed,
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcRoundTripTimeUpdate) -> Self {
        Self {
            receive_time: Timestamp::from_native_microsecond_bits(value.receive_time),
            round_trip_time: TimeDelta::from_native_microsecond_bits(value.round_trip_time),
            smoothed: value.smoothed,
        }
    }
}

/// Owned native TransportLossReport payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct TransportLossReport {
    pub receive_time: Timestamp,
    pub start_time: Timestamp,
    pub end_time: Timestamp,
    pub packets_lost_delta: u64,
    pub packets_received_delta: u64,
}
impl Default for TransportLossReport {
    fn default() -> Self {
        Self {
            receive_time: Timestamp::plus_infinity(),
            start_time: Timestamp::plus_infinity(),
            end_time: Timestamp::plus_infinity(),
            packets_lost_delta: 0,
            packets_received_delta: 0,
        }
    }
}
impl TransportLossReport {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcTransportLossReport {
        ffi::FfiNcTransportLossReport {
            receive_time: self.receive_time.native_microsecond_bits(),
            start_time: self.start_time.native_microsecond_bits(),
            end_time: self.end_time.native_microsecond_bits(),
            packets_lost_delta: self.packets_lost_delta,
            packets_received_delta: self.packets_received_delta,
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcTransportLossReport) -> Self {
        Self {
            receive_time: Timestamp::from_native_microsecond_bits(value.receive_time),
            start_time: Timestamp::from_native_microsecond_bits(value.start_time),
            end_time: Timestamp::from_native_microsecond_bits(value.end_time),
            packets_lost_delta: value.packets_lost_delta,
            packets_received_delta: value.packets_received_delta,
        }
    }
}

/// Owned native RtpPacketInfo payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RtpPacketInfo {
    pub ssrc: u32,
    pub rtp_sequence_number: u16,
    pub is_retransmission: bool,
}
impl RtpPacketInfo {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcRtpPacketInfo {
        ffi::FfiNcRtpPacketInfo {
            ssrc: self.ssrc,
            rtp_sequence_number: self.rtp_sequence_number,
            is_retransmission: self.is_retransmission,
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcRtpPacketInfo) -> Self {
        Self {
            ssrc: value.ssrc,
            rtp_sequence_number: value.rtp_sequence_number,
            is_retransmission: value.is_retransmission,
        }
    }
}

/// Owned native PacketResult payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct PacketResult {
    pub sent_packet: SentPacket,
    pub receive_time: Timestamp,
    pub arrival_time_offset: Option<TimeDelta>,
    pub ecn: EcnMarking,
    pub sent_with_ect1: bool,
    pub reported_lost_for_the_first_time: bool,
    pub reported_recovered_for_the_first_time: bool,
    pub rtp_packet_info: Option<RtpPacketInfo>,
}
impl Default for PacketResult {
    fn default() -> Self {
        Self {
            sent_packet: SentPacket::default(),
            receive_time: Timestamp::plus_infinity(),
            arrival_time_offset: None,
            ecn: EcnMarking::NotEct,
            sent_with_ect1: false,
            reported_lost_for_the_first_time: false,
            reported_recovered_for_the_first_time: false,
            rtp_packet_info: None,
        }
    }
}
impl PacketResult {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcPacketResult {
        ffi::FfiNcPacketResult {
            sent_packet: self.sent_packet.native(),
            receive_time: self.receive_time.native_microsecond_bits(),
            has_arrival_time_offset: self.arrival_time_offset.is_some(),
            arrival_time_offset: self
                .arrival_time_offset
                .as_ref()
                .map(|value| value.native_microsecond_bits())
                .unwrap_or_default(),
            ecn: self.ecn.native_bits(),
            sent_with_ect1: self.sent_with_ect1,
            reported_lost_for_the_first_time: self.reported_lost_for_the_first_time,
            reported_recovered_for_the_first_time: self.reported_recovered_for_the_first_time,
            has_rtp_packet_info: self.rtp_packet_info.is_some(),
            rtp_packet_info: self
                .rtp_packet_info
                .as_ref()
                .map(|value| value.native())
                .unwrap_or_default(),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcPacketResult) -> Self {
        Self {
            sent_packet: SentPacket::from_native(value.sent_packet),
            receive_time: Timestamp::from_native_microsecond_bits(value.receive_time),
            arrival_time_offset: if value.has_arrival_time_offset {
                Some(TimeDelta::from_native_microsecond_bits(
                    value.arrival_time_offset,
                ))
            } else {
                None
            },
            ecn: value.ecn.into(),
            sent_with_ect1: value.sent_with_ect1,
            reported_lost_for_the_first_time: value.reported_lost_for_the_first_time,
            reported_recovered_for_the_first_time: value.reported_recovered_for_the_first_time,
            rtp_packet_info: if value.has_rtp_packet_info {
                Some(RtpPacketInfo::from_native(value.rtp_packet_info))
            } else {
                None
            },
        }
    }
}

/// Owned native TransportPacketsFeedback payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct TransportPacketsFeedback {
    pub feedback_time: Timestamp,
    pub data_in_flight: DataSize,
    pub transport_supports_ecn: bool,
    pub packet_feedbacks: Vec<PacketResult>,
    pub sendless_arrival_times: Vec<Timestamp>,
}
impl Default for TransportPacketsFeedback {
    fn default() -> Self {
        Self {
            feedback_time: Timestamp::plus_infinity(),
            data_in_flight: DataSize::Bytes(0),
            transport_supports_ecn: false,
            packet_feedbacks: Vec::new(),
            sendless_arrival_times: Vec::new(),
        }
    }
}
impl TransportPacketsFeedback {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcTransportPacketsFeedback {
        ffi::FfiNcTransportPacketsFeedback {
            feedback_time: self.feedback_time.native_microsecond_bits(),
            data_in_flight: size_native(self.data_in_flight),
            transport_supports_ecn: self.transport_supports_ecn,
            packet_feedbacks: self
                .packet_feedbacks
                .iter()
                .map(|value| value.native())
                .collect(),
            sendless_arrival_times: self
                .sendless_arrival_times
                .iter()
                .map(|value| value.native_microsecond_bits())
                .collect(),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcTransportPacketsFeedback) -> Self {
        Self {
            feedback_time: Timestamp::from_native_microsecond_bits(value.feedback_time),
            data_in_flight: DataSize::from_native(
                value.data_in_flight.value,
                value.data_in_flight.kind,
            ),
            transport_supports_ecn: value.transport_supports_ecn,
            packet_feedbacks: value
                .packet_feedbacks
                .into_iter()
                .map(PacketResult::from_native)
                .collect(),
            sendless_arrival_times: value
                .sendless_arrival_times
                .into_iter()
                .map(Timestamp::from_native_microsecond_bits)
                .collect(),
        }
    }
}

/// Owned native NetworkEstimate payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkEstimate {
    pub at_time: Timestamp,
    pub bandwidth: DataRate,
    pub round_trip_time: TimeDelta,
    pub bwe_period: TimeDelta,
    pub loss_rate_ratio: f32,
}
impl Default for NetworkEstimate {
    fn default() -> Self {
        Self {
            at_time: Timestamp::plus_infinity(),
            bandwidth: DataRate::PositiveInfinity,
            round_trip_time: TimeDelta::plus_infinity(),
            bwe_period: TimeDelta::plus_infinity(),
            loss_rate_ratio: 0.0,
        }
    }
}
impl NetworkEstimate {
    pub(crate) fn native(&self) -> ffi::FfiNcNetworkEstimate {
        ffi::FfiNcNetworkEstimate {
            at_time: self.at_time.native_microsecond_bits(),
            bandwidth: rate_native(self.bandwidth),
            round_trip_time: self.round_trip_time.native_microsecond_bits(),
            bwe_period: self.bwe_period.native_microsecond_bits(),
            loss_rate_ratio: self.loss_rate_ratio,
        }
    }
    #[cfg(test)]
    pub(crate) fn from_native(value: ffi::FfiNcNetworkEstimate) -> Self {
        Self {
            at_time: Timestamp::from_native_microsecond_bits(value.at_time),
            bandwidth: DataRate::from_native(value.bandwidth.value, value.bandwidth.kind),
            round_trip_time: TimeDelta::from_native_microsecond_bits(value.round_trip_time),
            bwe_period: TimeDelta::from_native_microsecond_bits(value.bwe_period),
            loss_rate_ratio: value.loss_rate_ratio,
        }
    }
}

/// Owned native PacerConfig payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct PacerConfig {
    pub at_time: Timestamp,
    pub data_window: DataSize,
    pub time_window: TimeDelta,
    pub pad_window: DataSize,
}
impl Default for PacerConfig {
    fn default() -> Self {
        Self {
            at_time: Timestamp::plus_infinity(),
            data_window: DataSize::PositiveInfinity,
            time_window: TimeDelta::plus_infinity(),
            pad_window: DataSize::Bytes(0),
        }
    }
}
impl PacerConfig {
    pub(crate) fn native(&self) -> ffi::FfiNcPacerConfig {
        ffi::FfiNcPacerConfig {
            at_time: self.at_time.native_microsecond_bits(),
            data_window: size_native(self.data_window),
            time_window: self.time_window.native_microsecond_bits(),
            pad_window: size_native(self.pad_window),
        }
    }
    #[cfg(test)]
    pub(crate) fn from_native(value: ffi::FfiNcPacerConfig) -> Self {
        Self {
            at_time: Timestamp::from_native_microsecond_bits(value.at_time),
            data_window: DataSize::from_native(value.data_window.value, value.data_window.kind),
            time_window: TimeDelta::from_native_microsecond_bits(value.time_window),
            pad_window: DataSize::from_native(value.pad_window.value, value.pad_window.kind),
        }
    }
}

/// Owned native ProbeClusterConfig payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeClusterConfig {
    pub at_time: Timestamp,
    pub target_data_rate: DataRate,
    pub target_duration: TimeDelta,
    pub min_probe_delta: TimeDelta,
    pub target_probe_count: i32,
    pub id: i32,
}
impl Default for ProbeClusterConfig {
    fn default() -> Self {
        Self {
            at_time: Timestamp::plus_infinity(),
            target_data_rate: DataRate::BitsPerSecond(0),
            target_duration: TimeDelta::zero(),
            min_probe_delta: TimeDelta::from_native_microsecond_bits(2000),
            target_probe_count: 0,
            id: 0,
        }
    }
}
impl ProbeClusterConfig {
    pub(crate) fn native(&self) -> ffi::FfiNcProbeClusterConfig {
        ffi::FfiNcProbeClusterConfig {
            at_time: self.at_time.native_microsecond_bits(),
            target_data_rate: rate_native(self.target_data_rate),
            target_duration: self.target_duration.native_microsecond_bits(),
            min_probe_delta: self.min_probe_delta.native_microsecond_bits(),
            target_probe_count: self.target_probe_count,
            id: self.id,
        }
    }
    #[cfg(test)]
    pub(crate) fn from_native(value: ffi::FfiNcProbeClusterConfig) -> Self {
        Self {
            at_time: Timestamp::from_native_microsecond_bits(value.at_time),
            target_data_rate: DataRate::from_native(
                value.target_data_rate.value,
                value.target_data_rate.kind,
            ),
            target_duration: TimeDelta::from_native_microsecond_bits(value.target_duration),
            min_probe_delta: TimeDelta::from_native_microsecond_bits(value.min_probe_delta),
            target_probe_count: value.target_probe_count,
            id: value.id,
        }
    }
}

/// Owned native TargetTransferRate payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct TargetTransferRate {
    pub at_time: Timestamp,
    pub network_estimate: NetworkEstimate,
    pub target_rate: DataRate,
    pub cwnd_reduce_ratio: f64,
}
impl Default for TargetTransferRate {
    fn default() -> Self {
        Self {
            at_time: Timestamp::plus_infinity(),
            network_estimate: NetworkEstimate::default(),
            target_rate: DataRate::BitsPerSecond(0),
            cwnd_reduce_ratio: 0.0,
        }
    }
}
impl TargetTransferRate {
    pub(crate) fn native(&self) -> ffi::FfiNcTargetTransferRate {
        ffi::FfiNcTargetTransferRate {
            at_time: self.at_time.native_microsecond_bits(),
            network_estimate: self.network_estimate.native(),
            target_rate: rate_native(self.target_rate),
            cwnd_reduce_ratio: self.cwnd_reduce_ratio,
        }
    }
    #[cfg(test)]
    pub(crate) fn from_native(value: ffi::FfiNcTargetTransferRate) -> Self {
        Self {
            at_time: Timestamp::from_native_microsecond_bits(value.at_time),
            network_estimate: NetworkEstimate::from_native(value.network_estimate),
            target_rate: DataRate::from_native(value.target_rate.value, value.target_rate.kind),
            cwnd_reduce_ratio: value.cwnd_reduce_ratio,
        }
    }
}

/// Owned native NetworkControlUpdate payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct NetworkControlUpdate {
    pub congestion_window: Option<DataSize>,
    pub pacer_config: Option<PacerConfig>,
    pub probe_cluster_configs: Vec<ProbeClusterConfig>,
    pub target_rate: Option<TargetTransferRate>,
}
impl NetworkControlUpdate {
    pub(crate) fn native(&self) -> ffi::FfiNcNetworkControlUpdate {
        ffi::FfiNcNetworkControlUpdate {
            has_congestion_window: self.congestion_window.is_some(),
            congestion_window: self
                .congestion_window
                .as_ref()
                .map(|value| size_native(*value))
                .unwrap_or_default(),
            has_pacer_config: self.pacer_config.is_some(),
            pacer_config: self
                .pacer_config
                .as_ref()
                .map(|value| value.native())
                .unwrap_or_default(),
            probe_cluster_configs: self
                .probe_cluster_configs
                .iter()
                .map(|value| value.native())
                .collect(),
            has_target_rate: self.target_rate.is_some(),
            target_rate: self
                .target_rate
                .as_ref()
                .map(|value| value.native())
                .unwrap_or_default(),
            failed: false,
        }
    }
    #[cfg(test)]
    pub(crate) fn from_native(value: ffi::FfiNcNetworkControlUpdate) -> Self {
        Self {
            congestion_window: if value.has_congestion_window {
                Some(DataSize::from_native(
                    value.congestion_window.value,
                    value.congestion_window.kind,
                ))
            } else {
                None
            },
            pacer_config: if value.has_pacer_config {
                Some(PacerConfig::from_native(value.pacer_config))
            } else {
                None
            },
            probe_cluster_configs: value
                .probe_cluster_configs
                .into_iter()
                .map(|value| ProbeClusterConfig::from_native(value))
                .collect(),
            target_rate: if value.has_target_rate {
                Some(TargetTransferRate::from_native(value.target_rate))
            } else {
                None
            },
        }
    }
}

/// Owned native ProcessInterval payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessInterval {
    pub at_time: Timestamp,
    pub pacer_queue: Option<DataSize>,
}
impl Default for ProcessInterval {
    fn default() -> Self {
        Self {
            at_time: Timestamp::plus_infinity(),
            pacer_queue: None,
        }
    }
}
impl ProcessInterval {
    #[cfg(test)]
    pub(crate) fn native(&self) -> ffi::FfiNcProcessInterval {
        ffi::FfiNcProcessInterval {
            at_time: self.at_time.native_microsecond_bits(),
            has_pacer_queue: self.pacer_queue.is_some(),
            pacer_queue: self
                .pacer_queue
                .as_ref()
                .map(|value| size_native(*value))
                .unwrap_or_default(),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcProcessInterval) -> Self {
        Self {
            at_time: Timestamp::from_native_microsecond_bits(value.at_time),
            pacer_queue: if value.has_pacer_queue {
                Some(DataSize::from_native(
                    value.pacer_queue.value,
                    value.pacer_queue.kind,
                ))
            } else {
                None
            },
        }
    }
}

/// Owned native NetworkStateEstimate payload. Defaults match the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkStateEstimate {
    pub confidence: f64,
    pub update_time: Timestamp,
    pub last_receive_time: Timestamp,
    pub last_send_time: Timestamp,
    pub link_capacity: DataRate,
    pub link_capacity_lower: DataRate,
    pub link_capacity_upper: DataRate,
    pub pre_link_buffer_delay: TimeDelta,
    pub post_link_buffer_delay: TimeDelta,
    pub propagation_delay: TimeDelta,
    pub time_delta: TimeDelta,
    pub last_feed_time: Timestamp,
    pub cross_delay_rate: f64,
    pub spike_delay_rate: f64,
    pub link_capacity_std_dev: DataRate,
    pub link_capacity_min: DataRate,
    pub cross_traffic_ratio: f64,
}
impl Default for NetworkStateEstimate {
    fn default() -> Self {
        Self {
            confidence: f64::NAN,
            update_time: Timestamp::minus_infinity(),
            last_receive_time: Timestamp::minus_infinity(),
            last_send_time: Timestamp::minus_infinity(),
            link_capacity: DataRate::NegativeInfinity,
            link_capacity_lower: DataRate::NegativeInfinity,
            link_capacity_upper: DataRate::NegativeInfinity,
            pre_link_buffer_delay: TimeDelta::minus_infinity(),
            post_link_buffer_delay: TimeDelta::minus_infinity(),
            propagation_delay: TimeDelta::minus_infinity(),
            time_delta: TimeDelta::minus_infinity(),
            last_feed_time: Timestamp::minus_infinity(),
            cross_delay_rate: f64::NAN,
            spike_delay_rate: f64::NAN,
            link_capacity_std_dev: DataRate::NegativeInfinity,
            link_capacity_min: DataRate::NegativeInfinity,
            cross_traffic_ratio: f64::NAN,
        }
    }
}
impl NetworkStateEstimate {
    pub(crate) fn native(&self) -> ffi::FfiNcNetworkStateEstimate {
        ffi::FfiNcNetworkStateEstimate {
            confidence: self.confidence,
            update_time: self.update_time.native_microsecond_bits(),
            last_receive_time: self.last_receive_time.native_microsecond_bits(),
            last_send_time: self.last_send_time.native_microsecond_bits(),
            link_capacity: rate_native(self.link_capacity),
            link_capacity_lower: rate_native(self.link_capacity_lower),
            link_capacity_upper: rate_native(self.link_capacity_upper),
            pre_link_buffer_delay: self.pre_link_buffer_delay.native_microsecond_bits(),
            post_link_buffer_delay: self.post_link_buffer_delay.native_microsecond_bits(),
            propagation_delay: self.propagation_delay.native_microsecond_bits(),
            time_delta: self.time_delta.native_microsecond_bits(),
            last_feed_time: self.last_feed_time.native_microsecond_bits(),
            cross_delay_rate: self.cross_delay_rate,
            spike_delay_rate: self.spike_delay_rate,
            link_capacity_std_dev: rate_native(self.link_capacity_std_dev),
            link_capacity_min: rate_native(self.link_capacity_min),
            cross_traffic_ratio: self.cross_traffic_ratio,
        }
    }
    pub(crate) fn from_native(value: ffi::FfiNcNetworkStateEstimate) -> Self {
        Self {
            confidence: value.confidence,
            update_time: Timestamp::from_native_microsecond_bits(value.update_time),
            last_receive_time: Timestamp::from_native_microsecond_bits(value.last_receive_time),
            last_send_time: Timestamp::from_native_microsecond_bits(value.last_send_time),
            link_capacity: DataRate::from_native(
                value.link_capacity.value,
                value.link_capacity.kind,
            ),
            link_capacity_lower: DataRate::from_native(
                value.link_capacity_lower.value,
                value.link_capacity_lower.kind,
            ),
            link_capacity_upper: DataRate::from_native(
                value.link_capacity_upper.value,
                value.link_capacity_upper.kind,
            ),
            pre_link_buffer_delay: TimeDelta::from_native_microsecond_bits(
                value.pre_link_buffer_delay,
            ),
            post_link_buffer_delay: TimeDelta::from_native_microsecond_bits(
                value.post_link_buffer_delay,
            ),
            propagation_delay: TimeDelta::from_native_microsecond_bits(value.propagation_delay),
            time_delta: TimeDelta::from_native_microsecond_bits(value.time_delta),
            last_feed_time: Timestamp::from_native_microsecond_bits(value.last_feed_time),
            cross_delay_rate: value.cross_delay_rate,
            spike_delay_rate: value.spike_delay_rate,
            link_capacity_std_dev: DataRate::from_native(
                value.link_capacity_std_dev.value,
                value.link_capacity_std_dev.kind,
            ),
            link_capacity_min: DataRate::from_native(
                value.link_capacity_min.value,
                value.link_capacity_min.kind,
            ),
            cross_traffic_ratio: value.cross_traffic_ratio,
        }
    }
}

impl PacketResult {
    pub fn is_received(&self) -> bool {
        !self.receive_time.is_plus_infinity()
    }
    pub fn receive_time_order(&self, other: &Self) -> std::cmp::Ordering {
        self.receive_time
            .cmp(&other.receive_time)
            .then_with(|| self.sent_packet.send_time.cmp(&other.sent_packet.send_time))
            .then_with(|| {
                self.sent_packet
                    .sequence_number
                    .cmp(&other.sent_packet.sequence_number)
            })
    }
}
impl TransportPacketsFeedback {
    pub fn received_with_send_info(&self) -> Vec<PacketResult> {
        self.packet_feedbacks
            .iter()
            .filter(|value| value.is_received())
            .cloned()
            .collect()
    }
    pub fn lost_with_send_info(&self) -> Vec<PacketResult> {
        self.packet_feedbacks
            .iter()
            .filter(|value| !value.is_received())
            .cloned()
            .collect()
    }
    pub fn packets_with_feedback(&self) -> Vec<PacketResult> {
        self.packet_feedbacks.clone()
    }
    pub fn sorted_by_receive_time(&self) -> Vec<PacketResult> {
        let mut values = self.received_with_send_info();
        values.sort_by(|left, right| left.receive_time_order(right));
        values
    }
    pub fn has_packet_with_ecn_ce(&self) -> bool {
        self.packet_feedbacks
            .iter()
            .any(|value| value.ecn.native_bits() == 3)
    }
}
impl NetworkControlUpdate {
    pub fn has_updates(&self) -> bool {
        self.congestion_window.is_some()
            || self.pacer_config.is_some()
            || !self.probe_cluster_configs.is_empty()
            || self.target_rate.is_some()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkControlValueError {
    InvalidUnit,
    InvalidTimeWindow,
    OutOfRange,
}
impl std::fmt::Display for NetworkControlValueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "network-control unit operation failed: {self:?}")
    }
}
impl std::error::Error for NetworkControlValueError {}
impl PacerConfig {
    fn finite_rate_window(&self) -> Result<i64, NetworkControlValueError> {
        let time = self.rate_window();
        if !time.is_finite() || time.microseconds_or(0) <= 0 {
            return Err(NetworkControlValueError::InvalidTimeWindow);
        }
        Ok(time.microseconds_or(0))
    }
    fn rate(&self, value: DataSize) -> Result<DataRate, NetworkControlValueError> {
        let window = self.finite_rate_window()?;
        let DataSize::Bytes(bytes) = value else {
            return Err(NetworkControlValueError::InvalidUnit);
        };
        if bytes < 0 {
            return Err(NetworkControlValueError::InvalidUnit);
        }
        // Match native Microbits rather than overflowing its i64 multiply.
        let microbits = bytes
            .checked_mul(8_000_000)
            .ok_or(NetworkControlValueError::OutOfRange)?;
        Ok(DataRate::BitsPerSecond(microbits / window))
    }
    pub fn data_rate(&self) -> Result<DataRate, NetworkControlValueError> {
        self.rate(self.data_window)
    }
    pub fn pad_rate(&self) -> Result<DataRate, NetworkControlValueError> {
        self.rate(self.pad_window)
    }
    /// Native Create uses integer microbits and half-byte rounding. Invalid
    /// or overflowing operations return errors instead of invoking SDK checks.
    pub fn create(
        at_time: Timestamp,
        send_rate: DataRate,
        pad_rate: DataRate,
        time_window: TimeDelta,
    ) -> Result<Self, NetworkControlValueError> {
        let mut config = Self {
            at_time,
            time_window,
            ..Self::default()
        };
        let window = config.finite_rate_window()?;
        let convert = |rate: DataRate| -> Result<DataSize, NetworkControlValueError> {
            let DataRate::BitsPerSecond(bps) = rate else {
                return Err(NetworkControlValueError::InvalidUnit);
            };
            if bps < 0 {
                return Err(NetworkControlValueError::InvalidUnit);
            }
            let microbits = bps
                .checked_mul(window)
                .and_then(|value| value.checked_add(4_000_000))
                .ok_or(NetworkControlValueError::OutOfRange)?;
            Ok(DataSize::Bytes(microbits / 8_000_000))
        };
        config.data_window = convert(send_rate)?;
        config.pad_window = convert(pad_rate)?;
        Ok(config)
    }
}

impl PacerConfig {
    pub const DEFAULT_TIME_WINDOW: TimeDelta = TimeDelta::from_native_microsecond_bits(40_000);
    pub fn rate_window(&self) -> TimeDelta {
        if self.time_window.is_zero() {
            TimeDelta::from_native_microsecond_bits(1_000_000)
        } else {
            self.time_window
        }
    }
}
impl PacedPacketInfo {
    pub const NOT_A_PROBE: i32 = -1;
    /// Match native operator==, which intentionally ignores bytes_sent.
    pub fn same_native_probe(&self, other: &Self) -> bool {
        self.send_bitrate == other.send_bitrate
            && self.probe_cluster_id == other.probe_cluster_id
            && self.probe_cluster_min_probes == other.probe_cluster_min_probes
            && self.probe_cluster_min_bytes == other.probe_cluster_min_bytes
    }
}
