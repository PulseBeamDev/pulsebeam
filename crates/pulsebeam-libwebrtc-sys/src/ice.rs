//! Typed configuration and revocable, network-role ICE operations.
#[path = "ice_packet.rs"]
mod packet;
pub use packet::{
    Dscp, IceNetworkRoute, IcePacketDecryption, IcePacketEvent, IcePacketInfo,
    IcePacketObserverError, IcePacketOptions, IcePacketSendError, IcePacketState,
    IcePacketTimeUpdate, IceReceivedPacket, IceRouteEndpoint, IceSentPacket,
    IceSocketOperationResult, IceSocketOption, PacketInfoProtocol, PacketType,
};
#[path = "ice_candidate.rs"]
mod candidate;
use super::*;
use crate::{RtcError, TimeDelta};
pub use candidate::{
    CandidateNetworkCostOutOfRange, IceCandidatePair, IceCandidatePairChange,
    IceCandidatePairState, IceCandidateStats, IceConnectionInfo, IceStunStats, IceTransportStats,
    MAX_ICE_CANDIDATE_NETWORK_COST, TransportCandidate, TransportCandidateProperties,
    TransportCandidateType,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum IceNominationMode {
    Regular,
    Aggressive,
    SemiAggressive,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum IceMode {
    Full,
    Lite,
}
/// Exact owned native credential bytes. Construction does not imply SDP
/// credential validation; the internal SDK setter accepts these values as-is.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IceParameters {
    pub username_fragment: Vec<u8>,
    pub password: Vec<u8>,
    pub renomination: bool,
}
#[derive(Debug)]
pub enum IceOperationError {
    Unavailable(IceInternalUnavailable),
    Native(RtcError),
    UnknownRole(i32),
}
impl fmt::Display for IceOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(e) => e.fmt(f),
            Self::Native(e) => e.fmt(f),
            Self::UnknownRole(value) => write!(f, "unknown pinned ICE role {value}"),
        }
    }
}
impl std::error::Error for IceOperationError {}
fn available(value: bool) -> Result<(), IceInternalUnavailable> {
    if value {
        Ok(())
    } else {
        Err(IceInternalUnavailable)
    }
}
fn native_result(value: cxx::UniquePtr<ffi::NativeRtcError>) -> Result<(), IceOperationError> {
    if value.is_null() {
        return Err(IceOperationError::Unavailable(IceInternalUnavailable));
    }
    let error = RtcError::from_native(value);
    if error.is_ok() {
        Ok(())
    } else {
        Err(IceOperationError::Native(error))
    }
}
/// Optional timers retain the distinction between explicit settings and SDK
/// implementation defaults. Setters may ignore changes after gathering starts;
/// read `IceTransportInternal::config` for the actual native configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IceConfig {
    pub receiving_timeout: Option<TimeDelta>,
    pub backup_connection_ping_interval: Option<TimeDelta>,
    pub stable_writable_connection_ping_interval: Option<TimeDelta>,
    pub regather_on_failed_networks_interval: Option<TimeDelta>,
    pub receiving_switching_delay: Option<TimeDelta>,
    pub ice_check_interval_strong_connectivity: Option<TimeDelta>,
    pub ice_check_interval_weak_connectivity: Option<TimeDelta>,
    pub ice_check_min_interval: Option<TimeDelta>,
    pub ice_unwritable_timeout: Option<TimeDelta>,
    pub ice_inactive_timeout: Option<TimeDelta>,
    pub stun_keepalive_interval: Option<TimeDelta>,
    pub prioritize_most_likely_candidate_pairs: bool,
    pub presume_writable_when_fully_relayed: bool,
    pub surface_ice_candidates_on_ice_transport_type_changed: bool,
    pub dtls_handshake_in_stun: bool,
    pub continual_gathering_policy: crate::ContinualGatheringPolicy,
    pub default_nomination_mode: IceNominationMode,
    pub vpn_preference: crate::VpnPreference,
    pub network_preference: Option<crate::NetworkAdapterType>,
    pub ice_unwritable_min_checks: Option<i32>,
}
fn adapter(value: i32) -> crate::NetworkAdapterType {
    use crate::NetworkAdapterType::*;
    match value {
        0 => Unknown,
        1 => Ethernet,
        2 => Wifi,
        4 => Cellular,
        8 => Vpn,
        16 => Loopback,
        32 => Any,
        64 => Cellular2G,
        128 => Cellular3G,
        256 => Cellular4G,
        512 => Cellular5G,
        _ => unreachable!("unknown pinned native adapter"),
    }
}
impl Default for IceConfig {
    fn default() -> Self {
        Self::from_native(ffi::ice_config_default())
    }
}
impl IceConfig {
    fn from_native(value: ffi::FfiIceConfig) -> Self {
        Self {
            receiving_timeout: value
                .has_receiving_timeout
                .then(|| TimeDelta::from_native_microsecond_bits(value.receiving_timeout)),
            backup_connection_ping_interval: value.has_backup_connection_ping_interval.then(|| {
                TimeDelta::from_native_microsecond_bits(value.backup_connection_ping_interval)
            }),
            stable_writable_connection_ping_interval: value
                .has_stable_writable_connection_ping_interval
                .then(|| {
                    TimeDelta::from_native_microsecond_bits(
                        value.stable_writable_connection_ping_interval,
                    )
                }),
            regather_on_failed_networks_interval: value
                .has_regather_on_failed_networks_interval
                .then(|| {
                    TimeDelta::from_native_microsecond_bits(
                        value.regather_on_failed_networks_interval,
                    )
                }),
            receiving_switching_delay: value
                .has_receiving_switching_delay
                .then(|| TimeDelta::from_native_microsecond_bits(value.receiving_switching_delay)),
            ice_check_interval_strong_connectivity: value
                .has_ice_check_interval_strong_connectivity
                .then(|| {
                    TimeDelta::from_native_microsecond_bits(
                        value.ice_check_interval_strong_connectivity,
                    )
                }),
            ice_check_interval_weak_connectivity: value
                .has_ice_check_interval_weak_connectivity
                .then(|| {
                    TimeDelta::from_native_microsecond_bits(
                        value.ice_check_interval_weak_connectivity,
                    )
                }),
            ice_check_min_interval: value
                .has_ice_check_min_interval
                .then(|| TimeDelta::from_native_microsecond_bits(value.ice_check_min_interval)),
            ice_unwritable_timeout: value
                .has_ice_unwritable_timeout
                .then(|| TimeDelta::from_native_microsecond_bits(value.ice_unwritable_timeout)),
            ice_inactive_timeout: value
                .has_ice_inactive_timeout
                .then(|| TimeDelta::from_native_microsecond_bits(value.ice_inactive_timeout)),
            stun_keepalive_interval: value
                .has_stun_keepalive_interval
                .then(|| TimeDelta::from_native_microsecond_bits(value.stun_keepalive_interval)),
            prioritize_most_likely_candidate_pairs: value.prioritize_most_likely_candidate_pairs,
            presume_writable_when_fully_relayed: value.presume_writable_when_fully_relayed,
            surface_ice_candidates_on_ice_transport_type_changed: value
                .surface_ice_candidates_on_ice_transport_type_changed,
            dtls_handshake_in_stun: value.dtls_handshake_in_stun,
            continual_gathering_policy: crate::ContinualGatheringPolicy::from_native(
                value.continual_gathering_policy,
            ),
            default_nomination_mode: match value.default_nomination_mode {
                0 => IceNominationMode::Regular,
                1 => IceNominationMode::Aggressive,
                2 => IceNominationMode::SemiAggressive,
                _ => unreachable!("unknown pinned nomination mode"),
            },
            vpn_preference: crate::VpnPreference::from_native(value.vpn_preference),
            network_preference: value
                .has_network_preference
                .then(|| adapter(value.network_preference)),
            ice_unwritable_min_checks: value
                .has_ice_unwritable_min_checks
                .then_some(value.ice_unwritable_min_checks),
        }
    }
    fn native(&self) -> ffi::FfiIceConfig {
        ffi::FfiIceConfig {
            has_receiving_timeout: self.receiving_timeout.is_some(),
            receiving_timeout: self
                .receiving_timeout
                .map_or(0, TimeDelta::native_microsecond_bits),
            has_backup_connection_ping_interval: self.backup_connection_ping_interval.is_some(),
            backup_connection_ping_interval: self
                .backup_connection_ping_interval
                .map_or(0, TimeDelta::native_microsecond_bits),
            has_stable_writable_connection_ping_interval: self
                .stable_writable_connection_ping_interval
                .is_some(),
            stable_writable_connection_ping_interval: self
                .stable_writable_connection_ping_interval
                .map_or(0, TimeDelta::native_microsecond_bits),
            has_regather_on_failed_networks_interval: self
                .regather_on_failed_networks_interval
                .is_some(),
            regather_on_failed_networks_interval: self
                .regather_on_failed_networks_interval
                .map_or(0, TimeDelta::native_microsecond_bits),
            has_receiving_switching_delay: self.receiving_switching_delay.is_some(),
            receiving_switching_delay: self
                .receiving_switching_delay
                .map_or(0, TimeDelta::native_microsecond_bits),
            has_ice_check_interval_strong_connectivity: self
                .ice_check_interval_strong_connectivity
                .is_some(),
            ice_check_interval_strong_connectivity: self
                .ice_check_interval_strong_connectivity
                .map_or(0, TimeDelta::native_microsecond_bits),
            has_ice_check_interval_weak_connectivity: self
                .ice_check_interval_weak_connectivity
                .is_some(),
            ice_check_interval_weak_connectivity: self
                .ice_check_interval_weak_connectivity
                .map_or(0, TimeDelta::native_microsecond_bits),
            has_ice_check_min_interval: self.ice_check_min_interval.is_some(),
            ice_check_min_interval: self
                .ice_check_min_interval
                .map_or(0, TimeDelta::native_microsecond_bits),
            has_ice_unwritable_timeout: self.ice_unwritable_timeout.is_some(),
            ice_unwritable_timeout: self
                .ice_unwritable_timeout
                .map_or(0, TimeDelta::native_microsecond_bits),
            has_ice_inactive_timeout: self.ice_inactive_timeout.is_some(),
            ice_inactive_timeout: self
                .ice_inactive_timeout
                .map_or(0, TimeDelta::native_microsecond_bits),
            has_stun_keepalive_interval: self.stun_keepalive_interval.is_some(),
            stun_keepalive_interval: self
                .stun_keepalive_interval
                .map_or(0, TimeDelta::native_microsecond_bits),
            prioritize_most_likely_candidate_pairs: self.prioritize_most_likely_candidate_pairs,
            presume_writable_when_fully_relayed: self.presume_writable_when_fully_relayed,
            surface_ice_candidates_on_ice_transport_type_changed: self
                .surface_ice_candidates_on_ice_transport_type_changed,
            dtls_handshake_in_stun: self.dtls_handshake_in_stun,
            continual_gathering_policy: self.continual_gathering_policy as u8,
            default_nomination_mode: self.default_nomination_mode as u8,
            vpn_preference: self.vpn_preference as u8,
            has_network_preference: self.network_preference.is_some(),
            network_preference: self.network_preference.map_or(0, |v| v as i32),
            has_ice_unwritable_min_checks: self.ice_unwritable_min_checks.is_some(),
            ice_unwritable_min_checks: self.ice_unwritable_min_checks.unwrap_or(0),
        }
    }
    pub fn validate(&self) -> Result<(), RtcError> {
        let error = RtcError::from_native(ffi::ice_config_validate(&self.native()));
        if error.is_ok() { Ok(()) } else { Err(error) }
    }
    /// Resolve only the SDK's optional timer/check-count implementation
    /// defaults. The original configuration is not changed.
    pub fn resolved_defaults(&self) -> Self {
        Self::from_native(ffi::ice_config_resolve(&self.native()))
    }
    pub fn gather_continually(&self) -> bool {
        self.continual_gathering_policy == crate::ContinualGatheringPolicy::Continually
    }
}
impl IceTransportInternal {
    pub fn config(&self) -> Result<IceConfig, IceInternalUnavailable> {
        let mut value = ffi::ice_config_default();
        available(ffi::ice_transport_config(self.0.native(), &mut value))?;
        Ok(IceConfig::from_native(value))
    }
    /// Validate using the native IceConfig contract, then invoke the native
    /// setter. Success means the void setter ran, not that every field changed.
    pub fn set_config(&self, config: &IceConfig) -> Result<(), IceOperationError> {
        native_result(ffi::ice_transport_set_config(
            self.0.native(),
            &config.native(),
        ))
    }
    pub fn set_role(&self, role: IceTransportRole) -> Result<(), IceOperationError> {
        let role = match role {
            IceTransportRole::Controlling => 0,
            IceTransportRole::Controlled => 1,
            IceTransportRole::Unknown => 2,
            IceTransportRole::Other(v) if (0..=2).contains(&v) => v,
            IceTransportRole::Other(v) => return Err(IceOperationError::UnknownRole(v)),
        };
        available(ffi::ice_transport_set_role(self.0.native(), role))
            .map_err(IceOperationError::Unavailable)
    }
    pub fn set_parameters(&self, parameters: &IceParameters) -> Result<(), IceInternalUnavailable> {
        self.parameters(parameters, false)
    }
    pub fn set_remote_parameters(
        &self,
        parameters: &IceParameters,
    ) -> Result<(), IceInternalUnavailable> {
        self.parameters(parameters, true)
    }
    fn parameters(&self, p: &IceParameters, remote: bool) -> Result<(), IceInternalUnavailable> {
        available(ffi::ice_transport_set_parameters(
            self.0.native(),
            remote,
            &p.username_fragment,
            &p.password,
            p.renomination,
        ))
    }
    pub fn set_remote_mode(&self, mode: IceMode) -> Result<(), IceInternalUnavailable> {
        available(ffi::ice_transport_set_remote_mode(
            self.0.native(),
            mode as u8,
        ))
    }
    /// Invoke the native gathering trigger. Empty credentials or an unchanged
    /// generation may make this a native no-op, not an accepted async operation.
    pub fn maybe_start_gathering(&self) -> Result<(), IceInternalUnavailable> {
        available(ffi::ice_transport_maybe_start_gathering(self.0.native()))
    }
}
