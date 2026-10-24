use crate::{RtcEventKind, ffi};

macro_rules! event_enum {
    ($name:ident { $($value:literal => $variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum $name { $($variant,)+ Unknown(i32) }
        impl $name {
            pub(crate) fn native(self) -> Result<i32, crate::EventLogError> {
                match self {
                    $(Self::$variant => Ok($value),)+
                    Self::Unknown(value) => match value {
                        $($value => Ok(value),)+
                        _ => Err(crate::EventLogError::InvalidEventEnum),
                    }
                }
            }
        }
        impl From<i32> for $name {
            fn from(value: i32) -> Self {
                match value { $($value => Self::$variant,)+ other => Self::Unknown(other) }
            }
        }
    };
}
/// Compatibility name for the shared native bandwidth-usage value.
pub type RtcEventBandwidthUsage = crate::BandwidthUsage;
event_enum!(RtcEventDtlsState { 0 => New, 1 => Connecting, 2 => Connected, 3 => Closed, 4 => Failed });
event_enum!(RtcEventIceConfigType { 0 => Added, 1 => Updated, 2 => Destroyed, 3 => Selected });
event_enum!(RtcEventIcePairType { 0 => CheckSent, 1 => CheckReceived, 2 => CheckResponseSent, 3 => CheckResponseReceived });
event_enum!(RtcEventIceProtocol { 0 => UnknownProtocol, 1 => Udp, 2 => Tcp, 3 => SslTcp, 4 => Tls });
event_enum!(RtcEventIceAddressFamily { 0 => UnknownFamily, 1 => Ipv4, 2 => Ipv6 });
event_enum!(RtcEventIceNetworkType { 0 => UnknownNetwork, 1 => Ethernet, 2 => Loopback, 3 => Wifi, 4 => Vpn, 5 => Cellular });
event_enum!(RtcEventIceCandidateType { 0 => Host, 1 => ServerReflexive, 2 => PeerReflexive, 3 => Relay });
event_enum!(RtcEventProbeFailure { 0 => InvalidSendReceiveInterval, 1 => InvalidSendReceiveRatio, 2 => Timeout });
event_enum!(RtcEventVideoCodec { 0 => Generic, 1 => Vp8, 2 => Vp9, 3 => Av1, 4 => H264, 5 => H265 });
event_enum!(RtcEventRtcpMode { 0 => Off, 1 => Compound, 2 => ReducedSize });

/// Compatibility name for the shared native data-rate value.
pub type RtcEventDataRate = crate::DataRate;

#[derive(Clone, Debug, PartialEq)]
pub struct RtcEventAudioAdaptation {
    pub bitrate_bps: Option<i32>,
    pub frame_length_ms: Option<i32>,
    pub uplink_packet_loss_fraction: Option<f32>,
    pub enable_fec: Option<bool>,
    pub enable_dtx: Option<bool>,
    pub channels: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventStreamCodec {
    pub payload_name: String,
    pub payload_type: i32,
    pub rtx_payload_type: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventRtpExtension {
    pub uri: String,
    /// Native ID, including its unset sentinel.
    pub id: i32,
    pub encrypted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventStreamConfig {
    pub local_ssrc: u32,
    pub remote_ssrc: u32,
    pub rtx_ssrc: u32,
    pub rsid: String,
    pub remb: bool,
    pub rtp_extensions: Vec<RtcEventRtpExtension>,
    pub rtcp_mode: RtcEventRtcpMode,
    pub codecs: Vec<RtcEventStreamCodec>,
}
impl From<ffi::FfiRtcEventStreamConfig> for RtcEventStreamConfig {
    fn from(value: ffi::FfiRtcEventStreamConfig) -> Self {
        Self {
            local_ssrc: value.local_ssrc,
            remote_ssrc: value.remote_ssrc,
            rtx_ssrc: value.rtx_ssrc,
            rsid: value.rsid,
            remb: value.remb,
            rtp_extensions: value
                .extensions
                .into_iter()
                .map(|extension| RtcEventRtpExtension {
                    uri: extension.uri,
                    id: extension.preferred_id,
                    encrypted: extension.preferred_encrypt,
                })
                .collect(),
            rtcp_mode: value.rtcp_mode.into(),
            codecs: value
                .codecs
                .into_iter()
                .map(|codec| RtcEventStreamCodec {
                    payload_name: codec.payload_name,
                    payload_type: codec.payload_type,
                    rtx_payload_type: codec.rtx_payload_type,
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventIcePairDescription {
    pub local_candidate_type: RtcEventIceCandidateType,
    pub local_relay_protocol: RtcEventIceProtocol,
    pub local_network_type: RtcEventIceNetworkType,
    pub local_address_family: RtcEventIceAddressFamily,
    pub remote_candidate_type: RtcEventIceCandidateType,
    pub remote_address_family: RtcEventIceAddressFamily,
    pub candidate_pair_protocol: RtcEventIceProtocol,
}

/// Scalar and raw-header accessors of the native RTP logging event. The event
/// retains its original packet separately; this value does not pretend to carry
/// packet payload bytes, which the native logging interface does not expose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventRtpPacket {
    pub raw_header: Vec<u8>,
    pub packet_length: usize,
    pub header_length: usize,
    pub payload_length: usize,
    pub padding_length: usize,
    pub ssrc: u32,
    pub timestamp: u32,
    pub sequence_number: u16,
    pub payload_type: u8,
    pub marker: bool,
    pub rtx_original_sequence_number: Option<u16>,
}

/// Owned, typed snapshots of public native event payload accessors. Begin/end
/// log events expose only their common timestamp through `RtcEvent`; the begin
/// event's UTC constructor value has no public accessor in the pinned header.
#[derive(Clone, Debug, PartialEq)]
pub enum RtcEventData {
    AlrState {
        in_alr: bool,
    },
    RouteChange {
        connected: bool,
        overhead: u32,
    },
    RemoteEstimate {
        link_capacity_lower: RtcEventDataRate,
        link_capacity_upper: RtcEventDataRate,
    },
    AudioNetworkAdaptation(RtcEventAudioAdaptation),
    AudioPlayout {
        ssrc: u32,
    },
    AudioReceiveStreamConfig(RtcEventStreamConfig),
    AudioSendStreamConfig(RtcEventStreamConfig),
    BweUpdateDelayBased {
        bitrate_bps: i32,
        detector_state: RtcEventBandwidthUsage,
    },
    BweUpdateLossBased {
        bitrate_bps: i32,
        fraction_loss: u8,
        total_packets: i32,
    },
    BweUpdateScream {
        ref_window_bytes: u32,
        data_in_flight_bytes: u32,
        target_rate_kbps: u32,
        smoothed_rtt_ms: u32,
        avg_queue_delay_ms: u32,
        l4s_marked_permille: u32,
    },
    DtlsTransportState(RtcEventDtlsState),
    DtlsWritableState {
        writable: bool,
    },
    IceCandidatePairConfig {
        kind: RtcEventIceConfigType,
        candidate_pair_id: u32,
        description: RtcEventIcePairDescription,
    },
    IceCandidatePair {
        kind: RtcEventIcePairType,
        candidate_pair_id: u32,
        transaction_id: u32,
    },
    ProbeClusterCreated {
        id: i32,
        bitrate_bps: i32,
        min_probes: u32,
        min_bytes: u32,
    },
    ProbeResultFailure {
        id: i32,
        reason: RtcEventProbeFailure,
    },
    ProbeResultSuccess {
        id: i32,
        bitrate_bps: i32,
    },
    RtcpPacketIncoming(Vec<u8>),
    RtcpPacketOutgoing(Vec<u8>),
    RtpPacketIncoming(RtcEventRtpPacket),
    RtpPacketOutgoing {
        packet: RtcEventRtpPacket,
        probe_cluster_id: i32,
    },
    VideoReceiveStreamConfig(RtcEventStreamConfig),
    VideoSendStreamConfig(RtcEventStreamConfig),
    FrameDecoded {
        render_time_ms: i64,
        ssrc: u32,
        width: i32,
        height: i32,
        codec: RtcEventVideoCodec,
        qp: u8,
    },
    NetEqSetMinimumDelay {
        remote_ssrc: u32,
        minimum_delay_ms: i32,
    },
    BeginLog,
    EndLog,
    Unknown(u32),
}

impl RtcEventData {
    pub(crate) fn from_native(kind: RtcEventKind, value: ffi::FfiRtcEventPayload) -> Self {
        match kind {
            RtcEventKind::AlrState => Self::AlrState { in_alr: value.flag },
            RtcEventKind::RouteChange => Self::RouteChange {
                connected: value.flag,
                overhead: value.overhead,
            },
            RtcEventKind::RemoteEstimate => Self::RemoteEstimate {
                link_capacity_lower: RtcEventDataRate::from_native(
                    value.lower_rate_bps,
                    value.lower_rate_kind,
                ),
                link_capacity_upper: RtcEventDataRate::from_native(
                    value.upper_rate_bps,
                    value.upper_rate_kind,
                ),
            },
            RtcEventKind::AudioNetworkAdaptation => {
                Self::AudioNetworkAdaptation(RtcEventAudioAdaptation {
                    bitrate_bps: (value.adaptation_present & 1 != 0).then_some(value.bitrate_bps),
                    frame_length_ms: (value.adaptation_present & 2 != 0)
                        .then_some(value.frame_length_ms),
                    uplink_packet_loss_fraction: (value.adaptation_present & 4 != 0)
                        .then_some(value.uplink_packet_loss),
                    enable_fec: (value.adaptation_present & 8 != 0).then_some(value.enable_fec),
                    enable_dtx: (value.adaptation_present & 16 != 0).then_some(value.enable_dtx),
                    channels: (value.adaptation_present & 32 != 0).then_some(value.channels),
                })
            }
            RtcEventKind::AudioPlayout => Self::AudioPlayout { ssrc: value.ssrc },
            RtcEventKind::AudioReceiveStreamConfig => {
                Self::AudioReceiveStreamConfig(value.stream.into())
            }
            RtcEventKind::AudioSendStreamConfig => Self::AudioSendStreamConfig(value.stream.into()),
            RtcEventKind::VideoReceiveStreamConfig => {
                Self::VideoReceiveStreamConfig(value.stream.into())
            }
            RtcEventKind::VideoSendStreamConfig => Self::VideoSendStreamConfig(value.stream.into()),
            RtcEventKind::BweUpdateDelayBased => Self::BweUpdateDelayBased {
                bitrate_bps: value.bitrate_bps,
                detector_state: value.detector_state.into(),
            },
            RtcEventKind::BweUpdateLossBased => Self::BweUpdateLossBased {
                bitrate_bps: value.bitrate_bps,
                fraction_loss: value.fraction_loss,
                total_packets: value.total_packets,
            },
            RtcEventKind::BweUpdateScream => Self::BweUpdateScream {
                ref_window_bytes: value.ref_window_bytes,
                data_in_flight_bytes: value.data_in_flight_bytes,
                target_rate_kbps: value.target_rate_kbps,
                smoothed_rtt_ms: value.smoothed_rtt_ms,
                avg_queue_delay_ms: value.avg_queue_delay_ms,
                l4s_marked_permille: value.l4s_marked_permille,
            },
            RtcEventKind::DtlsTransportState => Self::DtlsTransportState(value.dtls_state.into()),
            RtcEventKind::DtlsWritableState => Self::DtlsWritableState {
                writable: value.flag,
            },
            RtcEventKind::IceCandidatePairConfig => Self::IceCandidatePairConfig {
                kind: value.ice_type.into(),
                candidate_pair_id: value.candidate_pair_id,
                description: RtcEventIcePairDescription {
                    local_candidate_type: value.local_candidate_type.into(),
                    local_relay_protocol: value.local_relay_protocol.into(),
                    local_network_type: value.local_network_type.into(),
                    local_address_family: value.local_address_family.into(),
                    remote_candidate_type: value.remote_candidate_type.into(),
                    remote_address_family: value.remote_address_family.into(),
                    candidate_pair_protocol: value.candidate_pair_protocol.into(),
                },
            },
            RtcEventKind::IceCandidatePair => Self::IceCandidatePair {
                kind: value.ice_type.into(),
                candidate_pair_id: value.candidate_pair_id,
                transaction_id: value.transaction_id,
            },
            RtcEventKind::ProbeClusterCreated => Self::ProbeClusterCreated {
                id: value.probe_id,
                bitrate_bps: value.bitrate_bps,
                min_probes: value.min_probes,
                min_bytes: value.min_bytes,
            },
            RtcEventKind::ProbeResultFailure => Self::ProbeResultFailure {
                id: value.probe_id,
                reason: value.failure_reason.into(),
            },
            RtcEventKind::ProbeResultSuccess => Self::ProbeResultSuccess {
                id: value.probe_id,
                bitrate_bps: value.bitrate_bps,
            },
            RtcEventKind::RtcpPacketIncoming => Self::RtcpPacketIncoming(value.bytes),
            RtcEventKind::RtcpPacketOutgoing => Self::RtcpPacketOutgoing(value.bytes),
            RtcEventKind::RtpPacketIncoming | RtcEventKind::RtpPacketOutgoing => {
                let packet = RtcEventRtpPacket {
                    raw_header: value.bytes,
                    packet_length: value.packet_length,
                    header_length: value.header_length,
                    payload_length: value.payload_length,
                    padding_length: value.padding_length,
                    ssrc: value.ssrc,
                    timestamp: value.rtp_timestamp,
                    sequence_number: value.sequence_number,
                    payload_type: value.payload_type,
                    marker: value.marker,
                    rtx_original_sequence_number: value
                        .has_rtx_sequence_number
                        .then_some(value.rtx_sequence_number),
                };
                if kind == RtcEventKind::RtpPacketIncoming {
                    Self::RtpPacketIncoming(packet)
                } else {
                    Self::RtpPacketOutgoing {
                        packet,
                        probe_cluster_id: value.probe_cluster_id,
                    }
                }
            }
            RtcEventKind::FrameDecoded => Self::FrameDecoded {
                render_time_ms: value.render_time_ms,
                ssrc: value.ssrc,
                width: value.width,
                height: value.height,
                codec: value.video_codec.into(),
                qp: value.qp,
            },
            RtcEventKind::NetEqSetMinimumDelay => Self::NetEqSetMinimumDelay {
                remote_ssrc: value.ssrc,
                minimum_delay_ms: value.minimum_delay_ms,
            },
            RtcEventKind::BeginLog => Self::BeginLog,
            RtcEventKind::EndLog => Self::EndLog,
            RtcEventKind::Unknown(value) => Self::Unknown(value),
        }
    }
}
