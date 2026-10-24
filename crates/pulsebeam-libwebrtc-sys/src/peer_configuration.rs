//! Owned native application RTCConfiguration values. Defaults are read from
//! the pinned native constructor, rather than copied into Rust.
use crate::{
    Certificate, CryptoOptions, IceServer, IceTransportPolicy, NetworkAdapterMask,
    PeerConfiguration, ffi,
};
use std::net::IpAddr;

/// Complete owned ICE server value. The legacy IceServer converts losslessly
/// into this type, while this type also exposes native TLS and URI options.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct RtcIceServer {
    /// Legacy single URI. Native prefers urls when nonempty.
    pub uri: String,
    pub urls: Vec<String>,
    pub username: String,
    pub password: String,
    pub tls_certificate_policy: TlsCertificatePolicy,
    /// TLS SNI hostname override, useful with IP-literal URLs.
    pub hostname: String,
    pub tls_alpn_protocols: Vec<String>,
    pub tls_elliptic_curves: Vec<String>,
}
impl std::fmt::Debug for RtcIceServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtcIceServer")
            .field("uri", &self.uri)
            .field("urls", &self.urls)
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .field("tls_certificate_policy", &self.tls_certificate_policy)
            .field("hostname", &self.hostname)
            .field("tls_alpn_protocols", &self.tls_alpn_protocols)
            .field("tls_elliptic_curves", &self.tls_elliptic_curves)
            .finish()
    }
}
impl From<IceServer> for RtcIceServer {
    fn from(v: IceServer) -> Self {
        Self {
            urls: v.urls,
            username: v.username,
            password: v.password,
            ..Default::default()
        }
    }
}

/// Owned application RTCConfiguration. Default uses the native safe preset.
/// Existing PeerConfiguration literals remain a convenient compatible input
/// to peer creation and configuration updates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcConfiguration {
    /// Native range is 0..=65535. Invalid values return InvalidRange.
    pub ice_candidate_pool_size: i32,
    pub always_negotiate_data_channels: bool,
    pub ice_servers: Vec<RtcIceServer>,
    pub ice_transport_policy: IceTransportPolicy,
    /// Additional trusted TURN/TLS CA. Upstream roots and hostname checks remain.
    pub turn_tls_ca_pem: Option<String>,
    pub options: PeerConnectionOptions,
}
impl Default for RtcConfiguration {
    fn default() -> Self {
        Self::native_default()
    }
}
impl From<PeerConfiguration> for RtcConfiguration {
    fn from(v: PeerConfiguration) -> Self {
        Self {
            ice_candidate_pool_size: i32::from(v.ice_candidate_pool_size),
            always_negotiate_data_channels: v.always_negotiate_data_channels,
            ice_servers: v.ice_servers.into_iter().map(RtcIceServer::from).collect(),
            ice_transport_policy: v.ice_transport_policy,
            turn_tls_ca_pem: v.turn_tls_ca_pem,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BundlePolicy {
    Balanced,
    MaxBundle,
    MaxCompat,
}
impl BundlePolicy {
    pub(crate) fn from_native(value: u8) -> Self {
        match value {
            0 => Self::Balanced,
            1 => Self::MaxBundle,
            2 => Self::MaxCompat,
            _ => unreachable!("unknown pinned native BundlePolicy"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RtcpMuxPolicy {
    Negotiate,
    Require,
}
impl RtcpMuxPolicy {
    pub(crate) fn from_native(value: u8) -> Self {
        match value {
            0 => Self::Negotiate,
            1 => Self::Require,
            _ => unreachable!("unknown pinned native RtcpMuxPolicy"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TcpCandidatePolicy {
    Enabled,
    Disabled,
}
impl TcpCandidatePolicy {
    pub(crate) fn from_native(value: u8) -> Self {
        match value {
            0 => Self::Enabled,
            1 => Self::Disabled,
            _ => unreachable!("unknown pinned native TcpCandidatePolicy"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CandidateNetworkPolicy {
    All,
    LowCost,
}
impl CandidateNetworkPolicy {
    pub(crate) fn from_native(value: u8) -> Self {
        match value {
            0 => Self::All,
            1 => Self::LowCost,
            _ => unreachable!("unknown pinned native CandidateNetworkPolicy"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ContinualGatheringPolicy {
    Once,
    Continually,
}
impl ContinualGatheringPolicy {
    pub(crate) fn from_native(value: u8) -> Self {
        match value {
            0 => Self::Once,
            1 => Self::Continually,
            _ => unreachable!("unknown pinned native ContinualGatheringPolicy"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TurnPortPrunePolicy {
    None,
    BasedOnPriority,
    KeepFirstReady,
}
impl TurnPortPrunePolicy {
    pub(crate) fn from_native(value: u8) -> Self {
        match value {
            0 => Self::None,
            1 => Self::BasedOnPriority,
            2 => Self::KeepFirstReady,
            _ => unreachable!("unknown pinned native TurnPortPrunePolicy"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SdpSemantics {
    PlanB,
    UnifiedPlan,
}
impl SdpSemantics {
    pub(crate) fn from_native(value: u8) -> Self {
        match value {
            0 => Self::PlanB,
            1 => Self::UnifiedPlan,
            _ => unreachable!("unknown pinned native SdpSemantics"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum VpnPreference {
    Default,
    OnlyUseVpn,
    NeverUseVpn,
    PreferVpn,
    AvoidVpn,
}
impl VpnPreference {
    pub(crate) fn from_native(value: u8) -> Self {
        match value {
            0 => Self::Default,
            1 => Self::OnlyUseVpn,
            2 => Self::NeverUseVpn,
            3 => Self::PreferVpn,
            4 => Self::AvoidVpn,
            _ => unreachable!("unknown pinned native VpnPreference"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
#[derive(Default)]
pub enum TlsCertificatePolicy {
    #[default]
    Secure,
    InsecureNoCheck,
}
impl TlsCertificatePolicy {
    pub(crate) fn from_native(value: u8) -> Self {
        match value {
            0 => Self::Secure,
            1 => Self::InsecureNoCheck,
            _ => unreachable!("unknown pinned native TlsCertificatePolicy"),
        }
    }
}

/// Native subnet hint. Native handles out-of-range prefix lengths according to
/// its IP truncation behavior; the binding does not invent a filtering policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkMask {
    pub address: IpAddr,
    pub prefix_length: i32,
}

/// Exact native TimeDelta representation in microseconds. The two extremes
/// represent infinity, not finite durations. Negative values are preserved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeTimeDelta(i64);
impl NativeTimeDelta {
    pub const PLUS_INFINITY: Self = Self(i64::MAX);
    pub const MINUS_INFINITY: Self = Self(i64::MIN);
    pub const fn from_microseconds(value: i64) -> Self {
        Self(value)
    }
    pub const fn as_microseconds(self) -> i64 {
        self.0
    }
}

/// Zero port bounds mean unspecified. Native applies these only to a factory
/// allocator, not an injected allocator. Flags preserve the full native mask.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PortAllocatorOptions {
    pub min_port: i32,
    pub max_port: i32,
    pub flags: u32,
}

/// Native application configuration, including deprecated and rollout knobs.
/// ICE times are native integer milliseconds, including native sentinel values.
/// Native validates configuration changes; not all fields are mutable after
/// creation. Accessors for media policy are mirrored here, not MediaConfig's
/// implementation-only storage. TURN customization is a separate provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerConnectionOptions {
    /// Retained native identities. Cloning never exports or reimports keys.
    pub certificates: Vec<Certificate>,
    pub bundle_policy: BundlePolicy,
    pub rtcp_mux_policy: RtcpMuxPolicy,
    pub disable_ipv6_on_wifi: bool,
    pub max_ipv6_networks: i32,
    pub disable_link_local_networks: bool,
    pub screencast_min_bitrate: Option<i32>,
    pub tcp_candidate_policy: TcpCandidatePolicy,
    pub candidate_network_policy: CandidateNetworkPolicy,
    pub audio_jitter_buffer_max_packets: i32,
    pub audio_jitter_buffer_fast_accelerate: bool,
    pub audio_jitter_buffer_min_delay_ms: i32,
    /// None selects the native undefined/default sentinel (-1).
    pub ice_connection_receiving_timeout: Option<i32>,
    /// None selects the native undefined/default sentinel (-1).
    pub ice_backup_candidate_pair_ping_interval: Option<i32>,
    pub continual_gathering_policy: ContinualGatheringPolicy,
    pub prioritize_most_likely_ice_candidate_pairs: bool,
    pub prune_turn_ports: bool,
    pub turn_port_prune_policy: TurnPortPrunePolicy,
    pub presume_writable_when_fully_relayed: bool,
    pub enable_ice_renomination: bool,
    pub redetermine_role_on_ice_restart: bool,
    pub surface_ice_candidates_on_ice_transport_type_changed: bool,
    pub ice_check_interval_strong_connectivity: Option<i32>,
    pub ice_check_interval_weak_connectivity: Option<i32>,
    pub ice_check_min_interval: Option<i32>,
    pub ice_unwritable_timeout: Option<i32>,
    pub ice_unwritable_min_checks: Option<i32>,
    pub ice_inactive_timeout: Option<i32>,
    pub stun_candidate_keepalive_interval: Option<i32>,
    pub stable_writable_connection_ping_interval_ms: Option<i32>,
    pub report_usage_pattern_delay_ms: Option<i32>,
    pub network_preference: Option<NetworkAdapterMask>,
    pub sdp_semantics: SdpSemantics,
    pub crypto_options: CryptoOptions,
    pub offer_extmap_allow_mixed: bool,
    pub turn_logging_id: String,
    pub enable_implicit_rollback: bool,
    pub vpn_preference: VpnPreference,
    pub vpn_list: Vec<NetworkMask>,
    pub port_allocator: PortAllocatorOptions,
    pub pacer_burst_interval: Option<NativeTimeDelta>,
    pub enable_any_address_ports: bool,
    pub max_sctp_streams: i32,
    pub enable_sctp_snap: bool,
    pub dscp: bool,
    pub stats_timestamp_with_environment_clock: bool,
    pub cpu_adaptation: bool,
    pub suspend_below_min_bitrate: bool,
    pub prerenderer_smoothing: bool,
    pub experiment_cpu_load_estimator: bool,
    pub audio_rtcp_report_interval_ms: i32,
    pub video_rtcp_report_interval_ms: i32,
}
impl Default for PeerConnectionOptions {
    fn default() -> Self {
        Self::from_native(ffi::peer_configuration_default(false).options)
    }
}
impl PeerConnectionOptions {
    /// Native pruning policy after applying the legacy prune_turn_ports override.
    pub fn effective_turn_port_prune_policy(&self) -> TurnPortPrunePolicy {
        if self.prune_turn_ports {
            TurnPortPrunePolicy::BasedOnPriority
        } else {
            self.turn_port_prune_policy
        }
    }

    pub(crate) fn to_native(&self) -> ffi::FfiPeerConnectionOptions {
        ffi::FfiPeerConnectionOptions {
            bundle_policy: self.bundle_policy as u8,
            rtcp_mux_policy: self.rtcp_mux_policy as u8,
            disable_ipv6_on_wifi: self.disable_ipv6_on_wifi,
            max_ipv6_networks: self.max_ipv6_networks,
            disable_link_local_networks: self.disable_link_local_networks,
            screencast_min_bitrate: optional(self.screencast_min_bitrate),
            tcp_candidate_policy: self.tcp_candidate_policy as u8,
            candidate_network_policy: self.candidate_network_policy as u8,
            audio_jitter_buffer_max_packets: self.audio_jitter_buffer_max_packets,
            audio_jitter_buffer_fast_accelerate: self.audio_jitter_buffer_fast_accelerate,
            audio_jitter_buffer_min_delay_ms: self.audio_jitter_buffer_min_delay_ms,
            ice_connection_receiving_timeout: self.ice_connection_receiving_timeout.unwrap_or(-1),
            ice_backup_candidate_pair_ping_interval: self
                .ice_backup_candidate_pair_ping_interval
                .unwrap_or(-1),
            continual_gathering_policy: self.continual_gathering_policy as u8,
            prioritize_most_likely_ice_candidate_pairs: self
                .prioritize_most_likely_ice_candidate_pairs,
            prune_turn_ports: self.prune_turn_ports,
            turn_port_prune_policy: self.turn_port_prune_policy as u8,
            presume_writable_when_fully_relayed: self.presume_writable_when_fully_relayed,
            enable_ice_renomination: self.enable_ice_renomination,
            redetermine_role_on_ice_restart: self.redetermine_role_on_ice_restart,
            surface_ice_candidates_on_ice_transport_type_changed: self
                .surface_ice_candidates_on_ice_transport_type_changed,
            ice_check_interval_strong_connectivity: optional(
                self.ice_check_interval_strong_connectivity,
            ),
            ice_check_interval_weak_connectivity: optional(
                self.ice_check_interval_weak_connectivity,
            ),
            ice_check_min_interval: optional(self.ice_check_min_interval),
            ice_unwritable_timeout: optional(self.ice_unwritable_timeout),
            ice_unwritable_min_checks: optional(self.ice_unwritable_min_checks),
            ice_inactive_timeout: optional(self.ice_inactive_timeout),
            stun_candidate_keepalive_interval: optional(self.stun_candidate_keepalive_interval),
            stable_writable_connection_ping_interval_ms: optional(
                self.stable_writable_connection_ping_interval_ms,
            ),
            report_usage_pattern_delay_ms: optional(self.report_usage_pattern_delay_ms),
            network_preference: optional(self.network_preference.map(|v| v.bits() as i32)),
            sdp_semantics: self.sdp_semantics as u8,
            crypto_options: self.crypto_options.to_native(),
            offer_extmap_allow_mixed: self.offer_extmap_allow_mixed,
            turn_logging_id: self.turn_logging_id.clone(),
            enable_implicit_rollback: self.enable_implicit_rollback,
            vpn_preference: self.vpn_preference as u8,
            vpn_list: self
                .vpn_list
                .iter()
                .map(|v| ffi::FfiNetworkMask {
                    address: v.address.to_string(),
                    prefix_length: v.prefix_length,
                })
                .collect(),
            port_allocator: ffi::FfiPortAllocatorOptions {
                min_port: self.port_allocator.min_port,
                max_port: self.port_allocator.max_port,
                flags: self.port_allocator.flags,
            },
            pacer_burst_interval: ffi::FfiOptionalI64 {
                present: self.pacer_burst_interval.is_some(),
                value: self.pacer_burst_interval.map_or(0, |v| v.0),
            },
            enable_any_address_ports: self.enable_any_address_ports,
            max_sctp_streams: self.max_sctp_streams,
            enable_sctp_snap: self.enable_sctp_snap,
            dscp: self.dscp,
            stats_timestamp_with_environment_clock: self.stats_timestamp_with_environment_clock,
            cpu_adaptation: self.cpu_adaptation,
            suspend_below_min_bitrate: self.suspend_below_min_bitrate,
            prerenderer_smoothing: self.prerenderer_smoothing,
            experiment_cpu_load_estimator: self.experiment_cpu_load_estimator,
            audio_rtcp_report_interval_ms: self.audio_rtcp_report_interval_ms,
            video_rtcp_report_interval_ms: self.video_rtcp_report_interval_ms,
        }
    }
    pub(crate) fn from_native(v: ffi::FfiPeerConnectionOptions) -> Self {
        Self {
            certificates: Vec::new(),
            bundle_policy: BundlePolicy::from_native(v.bundle_policy),
            rtcp_mux_policy: RtcpMuxPolicy::from_native(v.rtcp_mux_policy),
            disable_ipv6_on_wifi: v.disable_ipv6_on_wifi,
            max_ipv6_networks: v.max_ipv6_networks,
            disable_link_local_networks: v.disable_link_local_networks,
            screencast_min_bitrate: v
                .screencast_min_bitrate
                .present
                .then_some(v.screencast_min_bitrate.value),
            tcp_candidate_policy: TcpCandidatePolicy::from_native(v.tcp_candidate_policy),
            candidate_network_policy: CandidateNetworkPolicy::from_native(
                v.candidate_network_policy,
            ),
            audio_jitter_buffer_max_packets: v.audio_jitter_buffer_max_packets,
            audio_jitter_buffer_fast_accelerate: v.audio_jitter_buffer_fast_accelerate,
            audio_jitter_buffer_min_delay_ms: v.audio_jitter_buffer_min_delay_ms,
            ice_connection_receiving_timeout: (v.ice_connection_receiving_timeout != -1)
                .then_some(v.ice_connection_receiving_timeout),
            ice_backup_candidate_pair_ping_interval: (v.ice_backup_candidate_pair_ping_interval
                != -1)
                .then_some(v.ice_backup_candidate_pair_ping_interval),
            continual_gathering_policy: ContinualGatheringPolicy::from_native(
                v.continual_gathering_policy,
            ),
            prioritize_most_likely_ice_candidate_pairs: v
                .prioritize_most_likely_ice_candidate_pairs,
            prune_turn_ports: v.prune_turn_ports,
            turn_port_prune_policy: TurnPortPrunePolicy::from_native(v.turn_port_prune_policy),
            presume_writable_when_fully_relayed: v.presume_writable_when_fully_relayed,
            enable_ice_renomination: v.enable_ice_renomination,
            redetermine_role_on_ice_restart: v.redetermine_role_on_ice_restart,
            surface_ice_candidates_on_ice_transport_type_changed: v
                .surface_ice_candidates_on_ice_transport_type_changed,
            ice_check_interval_strong_connectivity: v
                .ice_check_interval_strong_connectivity
                .present
                .then_some(v.ice_check_interval_strong_connectivity.value),
            ice_check_interval_weak_connectivity: v
                .ice_check_interval_weak_connectivity
                .present
                .then_some(v.ice_check_interval_weak_connectivity.value),
            ice_check_min_interval: v
                .ice_check_min_interval
                .present
                .then_some(v.ice_check_min_interval.value),
            ice_unwritable_timeout: v
                .ice_unwritable_timeout
                .present
                .then_some(v.ice_unwritable_timeout.value),
            ice_unwritable_min_checks: v
                .ice_unwritable_min_checks
                .present
                .then_some(v.ice_unwritable_min_checks.value),
            ice_inactive_timeout: v
                .ice_inactive_timeout
                .present
                .then_some(v.ice_inactive_timeout.value),
            stun_candidate_keepalive_interval: v
                .stun_candidate_keepalive_interval
                .present
                .then_some(v.stun_candidate_keepalive_interval.value),
            stable_writable_connection_ping_interval_ms: v
                .stable_writable_connection_ping_interval_ms
                .present
                .then_some(v.stable_writable_connection_ping_interval_ms.value),
            report_usage_pattern_delay_ms: v
                .report_usage_pattern_delay_ms
                .present
                .then_some(v.report_usage_pattern_delay_ms.value),
            network_preference: v
                .network_preference
                .present
                .then(|| NetworkAdapterMask::from_bits(v.network_preference.value as u32)),
            sdp_semantics: SdpSemantics::from_native(v.sdp_semantics),
            crypto_options: CryptoOptions::from_native(v.crypto_options),
            offer_extmap_allow_mixed: v.offer_extmap_allow_mixed,
            turn_logging_id: v.turn_logging_id,
            enable_implicit_rollback: v.enable_implicit_rollback,
            vpn_preference: VpnPreference::from_native(v.vpn_preference),
            vpn_list: v
                .vpn_list
                .into_iter()
                .map(|v| NetworkMask {
                    address: v.address.parse().expect("native IP address"),
                    prefix_length: v.prefix_length,
                })
                .collect(),
            port_allocator: PortAllocatorOptions {
                min_port: v.port_allocator.min_port,
                max_port: v.port_allocator.max_port,
                flags: v.port_allocator.flags,
            },
            pacer_burst_interval: v
                .pacer_burst_interval
                .present
                .then_some(NativeTimeDelta(v.pacer_burst_interval.value)),
            enable_any_address_ports: v.enable_any_address_ports,
            max_sctp_streams: v.max_sctp_streams,
            enable_sctp_snap: v.enable_sctp_snap,
            dscp: v.dscp,
            stats_timestamp_with_environment_clock: v.stats_timestamp_with_environment_clock,
            cpu_adaptation: v.cpu_adaptation,
            suspend_below_min_bitrate: v.suspend_below_min_bitrate,
            prerenderer_smoothing: v.prerenderer_smoothing,
            experiment_cpu_load_estimator: v.experiment_cpu_load_estimator,
            audio_rtcp_report_interval_ms: v.audio_rtcp_report_interval_ms,
            video_rtcp_report_interval_ms: v.video_rtcp_report_interval_ms,
        }
    }
}
fn optional(v: Option<i32>) -> ffi::FfiOptionalI32 {
    ffi::FfiOptionalI32 {
        present: v.is_some(),
        value: v.unwrap_or(0),
    }
}
impl RtcIceServer {
    pub(crate) fn to_native(&self) -> ffi::FfiIceServer {
        ffi::FfiIceServer {
            uri: self.uri.clone(),
            urls: self.urls.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            tls_certificate_policy: self.tls_certificate_policy as u8,
            hostname: self.hostname.clone(),
            tls_alpn_protocols: self.tls_alpn_protocols.clone(),
            tls_elliptic_curves: self.tls_elliptic_curves.clone(),
        }
    }
    fn from_native(v: ffi::FfiIceServer) -> Self {
        Self {
            uri: v.uri,
            urls: v.urls,
            username: v.username,
            password: v.password,
            tls_certificate_policy: TlsCertificatePolicy::from_native(v.tls_certificate_policy),
            hostname: v.hostname,
            tls_alpn_protocols: v.tls_alpn_protocols,
            tls_elliptic_curves: v.tls_elliptic_curves,
        }
    }
}
impl RtcConfiguration {
    /// Pinned native safe preset, which does not force a data-channel m-line.
    pub fn native_default() -> Self {
        Self::from_values(ffi::peer_configuration_default(false))
    }
    /// Pinned native aggressive preset, without synthesized Rust defaults.
    pub fn aggressive() -> Self {
        Self::from_values(ffi::peer_configuration_default(true))
    }
    pub(crate) fn to_native(&self) -> cxx::UniquePtr<ffi::NativePeerConfiguration> {
        let mut native = ffi::peer_configuration_new(&ffi::FfiPeerConfiguration {
            ice_candidate_pool_size: self.ice_candidate_pool_size,
            always_negotiate_data_channels: self.always_negotiate_data_channels,
            ice_servers: self
                .ice_servers
                .iter()
                .map(RtcIceServer::to_native)
                .collect(),
            ice_transport_policy: self.ice_transport_policy as u8,
            turn_tls_ca_pem: self.turn_tls_ca_pem.clone().unwrap_or_default(),
            options: self.options.to_native(),
        });
        for certificate in &self.options.certificates {
            ffi::peer_configuration_add_certificate(native.pin_mut(), certificate.native());
        }
        native
    }
    fn from_values(v: ffi::FfiPeerConfiguration) -> Self {
        Self {
            ice_candidate_pool_size: v.ice_candidate_pool_size,
            always_negotiate_data_channels: v.always_negotiate_data_channels,
            ice_servers: v
                .ice_servers
                .into_iter()
                .map(RtcIceServer::from_native)
                .collect(),
            ice_transport_policy: IceTransportPolicy::from_native(v.ice_transport_policy),
            turn_tls_ca_pem: (!v.turn_tls_ca_pem.is_empty()).then_some(v.turn_tls_ca_pem),
            options: PeerConnectionOptions::from_native(v.options),
        }
    }
    pub(crate) fn from_native(native: &ffi::NativePeerConfiguration) -> Self {
        let mut values = Self::from_values(ffi::peer_configuration_values(native));
        values.options.certificates = (0..ffi::peer_configuration_certificate_count(native))
            .map(|i| {
                Certificate::from_native(ffi::peer_configuration_certificate(native, i))
                    .expect("retained native certificate")
            })
            .collect();
        values
    }
}
