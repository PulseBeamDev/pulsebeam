//! Owning SDK Candidate values, distinct from SDP/JSEP candidate messages.
use super::*;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum TransportCandidateType {
    Host,
    ServerReflexive,
    PeerReflexive,
    Relay,
}
impl TransportCandidateType {
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::Host,
            1 => Self::ServerReflexive,
            2 => Self::PeerReflexive,
            3 => Self::Relay,
            _ => unreachable!("unknown pinned candidate type"),
        }
    }
}

pub const MAX_ICE_CANDIDATE_NETWORK_COST: u16 = 999;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateNetworkCostOutOfRange(pub u16);
impl fmt::Display for CandidateNetworkCostOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "network cost {} exceeds native maximum {}",
            self.0, MAX_ICE_CANDIDATE_NETWORK_COST
        )
    }
}
impl std::error::Error for CandidateNetworkCostOutOfRange {}
#[derive(Clone, Debug)]
pub struct TransportCandidateProperties {
    pub id: Vec<u8>,
    pub protocol: Vec<u8>,
    pub relay_protocol: Vec<u8>,
    pub username: Vec<u8>,
    pub password: Vec<u8>,
    pub network_name: Vec<u8>,
    pub foundation: Vec<u8>,
    pub tcptype: Vec<u8>,
    pub url: Vec<u8>,
    pub component: i32,
    pub priority: u32,
    pub candidate_type: TransportCandidateType,
    pub network_type: crate::NetworkAdapterType,
    pub underlying_type_for_vpn: crate::NetworkAdapterType,
    pub generation: u32,
    pub network_id: u16,
    pub network_cost: u16,
    pub network_slice: crate::NetworkSlice,
    pub address: crate::SocketAddress,
    pub related_address: crate::SocketAddress,
}
/// Owns a complete SDK Candidate, including its native-generated logging ID.
/// Cloning preserves that ID; serializing to SDP does not preserve all fields.
pub struct TransportCandidate {
    native: cxx::UniquePtr<ffi::NativeTransportCandidate>,
}
// SAFETY: Candidate owns strings, socket-address values, enums and scalars,
// never an ICE internal/connection/thread pointer. Reads are const and every
// mutation requires exclusive Rust access, just like SocketAddress.
unsafe impl Send for TransportCandidate {}
unsafe impl Sync for TransportCandidate {}
impl Default for TransportCandidate {
    fn default() -> Self {
        Self::from_native(ffi::transport_candidate_default())
    }
}
impl Clone for TransportCandidate {
    fn clone(&self) -> Self {
        Self::from_native(ffi::transport_candidate_clone(self.native()))
    }
}
impl fmt::Debug for TransportCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TransportCandidate")
            .field(&self.to_native_string())
            .finish()
    }
}
impl TransportCandidate {
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeTransportCandidate>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    fn native(&self) -> &ffi::NativeTransportCandidate {
        self.native.as_ref().unwrap()
    }
    pub fn parse(bytes: impl AsRef<[u8]>) -> Result<Self, RtcError> {
        let result = ffi::transport_candidate_parse(bytes.as_ref());
        if !result.error.is_null() {
            Err(RtcError::from_native(result.error))
        } else {
            Ok(Self::from_native(result.candidate))
        }
    }
    /// Native VerifyCandidate is separate from value construction and parsing.
    pub fn verify(&self) -> Result<(), RtcError> {
        let error = RtcError::from_native(ffi::transport_candidate_verify(self.native()));
        if error.is_ok() { Ok(()) } else { Err(error) }
    }
    pub fn properties(&self) -> TransportCandidateProperties {
        let value = ffi::transport_candidate_fields(self.native());
        TransportCandidateProperties {
            id: value.id,
            protocol: value.protocol,
            relay_protocol: value.relay_protocol,
            username: value.username,
            password: value.password,
            network_name: value.network_name,
            foundation: value.foundation,
            tcptype: value.tcptype,
            url: value.url,
            component: value.component,
            priority: value.priority,
            candidate_type: TransportCandidateType::from_native(value.candidate_type),
            network_type: adapter(value.network_type),
            underlying_type_for_vpn: adapter(value.underlying_type_for_vpn),
            generation: value.generation,
            network_id: value.network_id,
            network_cost: value.network_cost,
            network_slice: match value.network_slice {
                0 => crate::NetworkSlice::None,
                1 => crate::NetworkSlice::UnifiedCommunications,
                _ => unreachable!("unknown pinned network slice"),
            },
            address: crate::SocketAddress::from_native(value.address),
            related_address: crate::SocketAddress::from_native(value.related_address),
        }
    }
    pub fn set_protocol(&mut self, value: impl AsRef<[u8]>) {
        ffi::transport_candidate_set_bytes(self.native.pin_mut(), 0, value.as_ref());
    }
    pub fn set_relay_protocol(&mut self, value: impl AsRef<[u8]>) {
        ffi::transport_candidate_set_bytes(self.native.pin_mut(), 1, value.as_ref());
    }
    pub fn set_username(&mut self, value: impl AsRef<[u8]>) {
        ffi::transport_candidate_set_bytes(self.native.pin_mut(), 2, value.as_ref());
    }
    pub fn set_password(&mut self, value: impl AsRef<[u8]>) {
        ffi::transport_candidate_set_bytes(self.native.pin_mut(), 3, value.as_ref());
    }
    pub fn set_network_name(&mut self, value: impl AsRef<[u8]>) {
        ffi::transport_candidate_set_bytes(self.native.pin_mut(), 4, value.as_ref());
    }
    pub fn set_foundation(&mut self, value: impl AsRef<[u8]>) {
        ffi::transport_candidate_set_bytes(self.native.pin_mut(), 5, value.as_ref());
    }
    pub fn set_tcptype(&mut self, value: impl AsRef<[u8]>) {
        ffi::transport_candidate_set_bytes(self.native.pin_mut(), 6, value.as_ref());
    }
    pub fn set_url(&mut self, value: impl AsRef<[u8]>) {
        ffi::transport_candidate_set_bytes(self.native.pin_mut(), 7, value.as_ref());
    }
    pub fn set_component(&mut self, value: i32) {
        ffi::transport_candidate_set_integer(self.native.pin_mut(), 0, value as i64);
    }
    pub fn set_priority(&mut self, value: u32) {
        ffi::transport_candidate_set_integer(self.native.pin_mut(), 1, value as i64);
    }
    pub fn set_candidate_type(&mut self, value: TransportCandidateType) {
        ffi::transport_candidate_set_integer(self.native.pin_mut(), 2, value as i64);
    }
    pub fn set_network_type(&mut self, value: crate::NetworkAdapterType) {
        ffi::transport_candidate_set_integer(self.native.pin_mut(), 3, value as i64);
    }
    pub fn set_underlying_type_for_vpn(&mut self, value: crate::NetworkAdapterType) {
        ffi::transport_candidate_set_integer(self.native.pin_mut(), 4, value as i64);
    }
    pub fn set_generation(&mut self, value: u32) {
        ffi::transport_candidate_set_integer(self.native.pin_mut(), 5, value as i64);
    }
    pub fn set_network_id(&mut self, value: u16) {
        ffi::transport_candidate_set_integer(self.native.pin_mut(), 6, value as i64);
    }
    pub fn set_network_slice(&mut self, value: crate::NetworkSlice) {
        ffi::transport_candidate_set_integer(self.native.pin_mut(), 8, value as i64);
    }
    pub fn set_network_cost(&mut self, value: u16) -> Result<(), CandidateNetworkCostOutOfRange> {
        if value > MAX_ICE_CANDIDATE_NETWORK_COST {
            return Err(CandidateNetworkCostOutOfRange(value));
        }
        ffi::transport_candidate_set_integer(self.native.pin_mut(), 7, value as i64);
        Ok(())
    }
    pub fn set_address(&mut self, value: &crate::SocketAddress) {
        ffi::transport_candidate_set_address(self.native.pin_mut(), false, value.native());
    }
    pub fn set_related_address(&mut self, value: &crate::SocketAddress) {
        ffi::transport_candidate_set_address(self.native.pin_mut(), true, value.native());
    }
    pub fn to_native_string(&self) -> Vec<u8> {
        ffi::transport_candidate_serialize(self.native(), 0, false)
    }
    pub fn to_sensitive_string(&self) -> Vec<u8> {
        ffi::transport_candidate_serialize(self.native(), 1, false)
    }
    pub fn to_candidate_attribute(&self, include_ufrag: bool) -> Vec<u8> {
        ffi::transport_candidate_serialize(self.native(), 2, include_ufrag)
    }
    pub fn is_equivalent(&self, other: &Self) -> bool {
        ffi::transport_candidate_equivalent(self.native(), other.native(), false)
    }
    pub fn matches_for_removal(&self, other: &Self) -> bool {
        ffi::transport_candidate_equivalent(self.native(), other.native(), true)
    }
}
#[derive(Clone, Debug)]
pub struct IceStunStats {
    pub binding_requests_sent: i32,
    pub binding_responses_received: i32,
    pub binding_rtt_ms_total: f64,
    pub binding_rtt_ms_squared_total: f64,
}
#[derive(Clone, Debug)]
pub struct IceCandidateStats {
    pub candidate: TransportCandidate,
    pub stun_stats: Option<IceStunStats>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IceCandidatePairState {
    Waiting,
    InProgress,
    Succeeded,
    Failed,
    Other(i32),
}
#[derive(Clone, Debug)]
pub struct IceCandidatePairChange {
    pub transport_name: Vec<u8>,
    pub selected_candidate_pair: IceCandidatePair,
    pub last_data_received_ms: i64,
    pub reason: Vec<u8>,
    pub estimated_disconnected_time_ms: i64,
}
impl IceCandidatePairChange {
    pub(crate) fn from_native(value: ffi::FfiIcePairChange) -> Self {
        Self {
            transport_name: value.transport_name,
            selected_candidate_pair: IceCandidatePair {
                local: TransportCandidate::from_native(value.local),
                remote: TransportCandidate::from_native(value.remote),
            },
            last_data_received_ms: value.last_data_received_ms,
            reason: value.reason,
            estimated_disconnected_time_ms: value.estimated_disconnected_time_ms,
        }
    }
}
#[derive(Clone, Debug)]
pub struct IceCandidatePair {
    pub local: TransportCandidate,
    pub remote: TransportCandidate,
}
#[derive(Clone, Debug)]
pub struct IceConnectionInfo {
    pub best_connection: bool,
    pub writable: bool,
    pub receiving: bool,
    pub timeout: bool,
    pub rtt: usize,
    pub sent_discarded_bytes: usize,
    pub sent_total_bytes: usize,
    pub sent_bytes_second: usize,
    pub sent_discarded_packets: usize,
    pub sent_total_packets: usize,
    pub sent_ping_requests_total: usize,
    pub sent_ping_requests_before_first_response: usize,
    pub sent_ping_responses: usize,
    pub recv_total_bytes: usize,
    pub recv_bytes_second: usize,
    pub packets_received: usize,
    pub recv_ping_requests: usize,
    pub recv_ping_responses: usize,
    pub local_candidate: TransportCandidate,
    pub remote_candidate: TransportCandidate,
    pub state: IceCandidatePairState,
    pub priority: u64,
    pub nominated: bool,
    pub total_round_trip_time_ms: u64,
    pub current_round_trip_time_ms: Option<u32>,
    pub last_data_received: Option<crate::Timestamp>,
    pub last_data_sent: Option<crate::Timestamp>,
}
#[derive(Clone, Debug)]
pub struct IceTransportStats {
    pub candidate_stats: Vec<IceCandidateStats>,
    pub connection_infos: Vec<IceConnectionInfo>,
    pub selected_candidate_pair_changes: u32,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub ice_role: IceTransportRole,
    pub ice_local_username_fragment: Vec<u8>,
    pub ice_state: IceTransportState,
}
impl IceTransportInternal {
    /// Invoke the native void candidate mutation. This is not a fabricated
    /// asynchronous peer operation or an SDP-level validation result.
    pub fn add_remote_candidate(
        &self,
        candidate: &TransportCandidate,
    ) -> Result<(), IceInternalUnavailable> {
        available(ffi::ice_transport_candidate(
            self.0.native(),
            candidate.native(),
            false,
        ))
    }
    pub fn remove_remote_candidate(
        &self,
        candidate: &TransportCandidate,
    ) -> Result<(), IceInternalUnavailable> {
        available(ffi::ice_transport_candidate(
            self.0.native(),
            candidate.native(),
            true,
        ))
    }
    pub fn remove_all_remote_candidates(&self) -> Result<(), IceInternalUnavailable> {
        available(ffi::ice_transport_remove_all_candidates(self.0.native()))
    }
    pub fn selected_candidate_pair(
        &self,
    ) -> Result<Option<IceCandidatePair>, IceInternalUnavailable> {
        let value = ffi::ice_transport_selected_pair(self.0.native());
        available(value.available)?;
        Ok((!value.local.is_null()).then(|| IceCandidatePair {
            local: TransportCandidate::from_native(value.local),
            remote: TransportCandidate::from_native(value.remote),
        }))
    }
    pub fn stats(&self) -> Result<Option<IceTransportStats>, IceInternalUnavailable> {
        let value = ffi::ice_transport_stats(self.0.native());
        available(value.available)?;
        if !value.success {
            return Ok(None);
        }
        Ok(Some(IceTransportStats {
            selected_candidate_pair_changes: value.selected_candidate_pair_changes,
            bytes_sent: value.bytes_sent,
            bytes_received: value.bytes_received,
            packets_sent: value.packets_sent,
            packets_received: value.packets_received,
            ice_role: IceTransportRole::from_native(value.ice_role),
            ice_state: IceTransportState::from_native(value.ice_state),
            ice_local_username_fragment: value.ice_local_username_fragment,
            candidate_stats: value
                .candidate_stats
                .into_iter()
                .map(|v| IceCandidateStats {
                    candidate: TransportCandidate::from_native(v.candidate),
                    stun_stats: v.has_stun_stats.then_some(IceStunStats {
                        binding_requests_sent: v.stun_binding_requests_sent,
                        binding_responses_received: v.stun_binding_responses_received,
                        binding_rtt_ms_total: v.stun_binding_rtt_ms_total,
                        binding_rtt_ms_squared_total: v.stun_binding_rtt_ms_squared_total,
                    }),
                })
                .collect(),
            connection_infos: value
                .connection_infos
                .into_iter()
                .map(|v| IceConnectionInfo {
                    best_connection: v.best_connection,
                    writable: v.writable,
                    receiving: v.receiving,
                    timeout: v.timeout,
                    rtt: v.rtt,
                    sent_discarded_bytes: v.sent_discarded_bytes,
                    sent_total_bytes: v.sent_total_bytes,
                    sent_bytes_second: v.sent_bytes_second,
                    sent_discarded_packets: v.sent_discarded_packets,
                    sent_total_packets: v.sent_total_packets,
                    sent_ping_requests_total: v.sent_ping_requests_total,
                    sent_ping_requests_before_first_response: v
                        .sent_ping_requests_before_first_response,
                    sent_ping_responses: v.sent_ping_responses,
                    recv_total_bytes: v.recv_total_bytes,
                    recv_bytes_second: v.recv_bytes_second,
                    packets_received: v.packets_received,
                    recv_ping_requests: v.recv_ping_requests,
                    recv_ping_responses: v.recv_ping_responses,
                    priority: v.priority,
                    nominated: v.nominated,
                    total_round_trip_time_ms: v.total_round_trip_time_ms,
                    local_candidate: TransportCandidate::from_native(v.local_candidate),
                    remote_candidate: TransportCandidate::from_native(v.remote_candidate),
                    state: match v.state {
                        0 => IceCandidatePairState::Waiting,
                        1 => IceCandidatePairState::InProgress,
                        2 => IceCandidatePairState::Succeeded,
                        3 => IceCandidatePairState::Failed,
                        value => IceCandidatePairState::Other(value),
                    },
                    current_round_trip_time_ms: v
                        .has_current_round_trip_time_ms
                        .then_some(v.current_round_trip_time_ms),
                    last_data_received: v.has_last_data_received.then(|| {
                        crate::Timestamp::from_native_microsecond_bits(v.last_data_received)
                    }),
                    last_data_sent: v
                        .has_last_data_sent
                        .then(|| crate::Timestamp::from_native_microsecond_bits(v.last_data_sent)),
                })
                .collect(),
        }))
    }
}
