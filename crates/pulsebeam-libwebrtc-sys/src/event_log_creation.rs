use crate::{
    EventLogError, RtcEvent, RtcEventData, RtcEventDataRate, RtcEventRtpExtension,
    RtcEventStreamConfig, ffi,
};

impl RtcEvent {
    /// Construct an ordinary native event from typed payload values. RTP
    /// snapshots omit packet bytes; use the packet constructors instead. Begin
    /// and end log events use their explicit timestamp constructors.
    /// Unknown native enum domains are rejected before native entry. Other
    /// scalar values retain the pinned event constructor's storage semantics.
    pub fn from_data(data: RtcEventData) -> Result<Self, EventLogError> {
        let (kind, payload) = event_payload(data)?;
        let native = ffi::new_rtc_event(kind, &payload);
        if native.is_null() {
            Err(EventLogError::IncompleteEventData)
        } else {
            Ok(Self::from_native(native))
        }
    }

    pub fn begin_log(timestamp_us: i64, utc_start_us: i64) -> Result<Self, EventLogError> {
        finite_timestamp(timestamp_us)?;
        finite_timestamp(utc_start_us)?;
        Ok(Self::from_native(ffi::new_rtc_event_boundary(
            true,
            timestamp_us,
            utc_start_us,
        )))
    }

    pub fn end_log(timestamp_us: i64) -> Result<Self, EventLogError> {
        finite_timestamp(timestamp_us)?;
        Ok(Self::from_native(ffi::new_rtc_event_boundary(
            false,
            timestamp_us,
            0,
        )))
    }

    /// Parse and own the complete RTP packet using the pinned native parser.
    /// Unknown extension URIs retain native behavior: their bytes remain in
    /// the packet, but they do not register a decoded native extension type.
    pub fn rtp_incoming(
        packet: &[u8],
        extensions: &[RtcEventRtpExtension],
        rtx_original_sequence_number: Option<u16>,
    ) -> Result<Self, EventLogError> {
        rtp_event(true, packet, extensions, 0, rtx_original_sequence_number)
    }

    pub fn rtp_outgoing(
        packet: &[u8],
        extensions: &[RtcEventRtpExtension],
        probe_cluster_id: i32,
        rtx_original_sequence_number: Option<u16>,
    ) -> Result<Self, EventLogError> {
        rtp_event(
            false,
            packet,
            extensions,
            probe_cluster_id,
            rtx_original_sequence_number,
        )
    }
}

fn finite_timestamp(value: i64) -> Result<(), EventLogError> {
    if value == i64::MIN || value == i64::MAX {
        Err(EventLogError::InvalidTimestamp)
    } else {
        Ok(())
    }
}
fn rtp_event(
    incoming: bool,
    packet: &[u8],
    extensions: &[RtcEventRtpExtension],
    probe_cluster_id: i32,
    rtx: Option<u16>,
) -> Result<RtcEvent, EventLogError> {
    if extensions
        .iter()
        .any(|extension| !(1..=255).contains(&extension.id))
    {
        return Err(EventLogError::InvalidRtpExtensionId);
    }
    let extensions: Vec<_> = extensions
        .iter()
        .map(|extension| ffi::FfiRtpHeaderExtension {
            uri: extension.uri.clone(),
            preferred_id: extension.id,
            preferred_encrypt: extension.encrypted,
            direction: 0,
        })
        .collect();
    let native = ffi::new_rtc_event_rtp(
        incoming,
        packet,
        &extensions,
        probe_cluster_id,
        rtx.is_some(),
        rtx.unwrap_or(0),
    );
    if native.is_null() {
        Err(EventLogError::InvalidRtpPacket)
    } else {
        Ok(RtcEvent::from_native(native))
    }
}

fn stream(config: RtcEventStreamConfig) -> Result<ffi::FfiRtcEventStreamConfig, EventLogError> {
    Ok(ffi::FfiRtcEventStreamConfig {
        local_ssrc: config.local_ssrc,
        remote_ssrc: config.remote_ssrc,
        rtx_ssrc: config.rtx_ssrc,
        rsid: config.rsid,
        remb: config.remb,
        rtcp_mode: config.rtcp_mode.native()?,
        extensions: config
            .rtp_extensions
            .into_iter()
            .map(|extension| ffi::FfiRtpHeaderExtension {
                uri: extension.uri,
                preferred_id: extension.id,
                preferred_encrypt: extension.encrypted,
                direction: 0,
            })
            .collect(),
        codecs: config
            .codecs
            .into_iter()
            .map(|codec| ffi::FfiRtcEventStreamCodec {
                payload_name: codec.payload_name,
                payload_type: codec.payload_type,
                rtx_payload_type: codec.rtx_payload_type,
            })
            .collect(),
    })
}
fn rate(rate: RtcEventDataRate) -> Result<(i64, i8), EventLogError> {
    match rate {
        RtcEventDataRate::BitsPerSecond(value) if (0..i64::MAX).contains(&value) => Ok((value, 0)),
        RtcEventDataRate::BitsPerSecond(_) => Err(EventLogError::InvalidRate),
        RtcEventDataRate::PositiveInfinity => Ok((0, 1)),
        RtcEventDataRate::NegativeInfinity => Ok((0, -1)),
    }
}

fn event_payload(data: RtcEventData) -> Result<(u32, ffi::FfiRtcEventPayload), EventLogError> {
    let mut value = ffi::FfiRtcEventPayload::default();
    let kind = match data {
        RtcEventData::AlrState { in_alr } => {
            value.flag = in_alr;
            0
        }
        RtcEventData::RouteChange {
            connected,
            overhead,
        } => {
            value.flag = connected;
            value.overhead = overhead;
            1
        }
        RtcEventData::RemoteEstimate {
            link_capacity_lower,
            link_capacity_upper,
        } => {
            (value.lower_rate_bps, value.lower_rate_kind) = rate(link_capacity_lower)?;
            (value.upper_rate_bps, value.upper_rate_kind) = rate(link_capacity_upper)?;
            2
        }
        RtcEventData::AudioNetworkAdaptation(config) => {
            if let Some(field) = config.bitrate_bps {
                value.adaptation_present |= 1;
                value.bitrate_bps = field;
            }
            if let Some(field) = config.frame_length_ms {
                value.adaptation_present |= 2;
                value.frame_length_ms = field;
            }
            if let Some(field) = config.uplink_packet_loss_fraction {
                value.adaptation_present |= 4;
                value.uplink_packet_loss = field;
            }
            if let Some(field) = config.enable_fec {
                value.adaptation_present |= 8;
                value.enable_fec = field;
            }
            if let Some(field) = config.enable_dtx {
                value.adaptation_present |= 16;
                value.enable_dtx = field;
            }
            if let Some(field) = config.channels {
                value.adaptation_present |= 32;
                value.channels = field;
            }
            3
        }
        RtcEventData::AudioPlayout { ssrc } => {
            value.ssrc = ssrc;
            4
        }
        RtcEventData::AudioReceiveStreamConfig(config) => {
            value.stream = stream(config)?;
            5
        }
        RtcEventData::AudioSendStreamConfig(config) => {
            value.stream = stream(config)?;
            6
        }
        RtcEventData::BweUpdateDelayBased {
            bitrate_bps,
            detector_state,
        } => {
            value.bitrate_bps = bitrate_bps;
            value.detector_state = detector_state.event_bits()?;
            7
        }
        RtcEventData::BweUpdateLossBased {
            bitrate_bps,
            fraction_loss,
            total_packets,
        } => {
            value.bitrate_bps = bitrate_bps;
            value.fraction_loss = fraction_loss;
            value.total_packets = total_packets;
            8
        }
        RtcEventData::BweUpdateScream {
            ref_window_bytes,
            data_in_flight_bytes,
            target_rate_kbps,
            smoothed_rtt_ms,
            avg_queue_delay_ms,
            l4s_marked_permille,
        } => {
            value.ref_window_bytes = ref_window_bytes;
            value.data_in_flight_bytes = data_in_flight_bytes;
            value.target_rate_kbps = target_rate_kbps;
            value.smoothed_rtt_ms = smoothed_rtt_ms;
            value.avg_queue_delay_ms = avg_queue_delay_ms;
            value.l4s_marked_permille = l4s_marked_permille;
            9
        }
        RtcEventData::DtlsTransportState(state) => {
            value.dtls_state = state.native()?;
            10
        }
        RtcEventData::DtlsWritableState { writable } => {
            value.flag = writable;
            11
        }
        RtcEventData::IceCandidatePairConfig {
            kind,
            candidate_pair_id,
            description,
        } => {
            value.ice_type = kind.native()?;
            value.candidate_pair_id = candidate_pair_id;
            value.local_candidate_type = description.local_candidate_type.native()?;
            value.local_relay_protocol = description.local_relay_protocol.native()?;
            value.local_network_type = description.local_network_type.native()?;
            value.local_address_family = description.local_address_family.native()?;
            value.remote_candidate_type = description.remote_candidate_type.native()?;
            value.remote_address_family = description.remote_address_family.native()?;
            value.candidate_pair_protocol = description.candidate_pair_protocol.native()?;
            12
        }
        RtcEventData::IceCandidatePair {
            kind,
            candidate_pair_id,
            transaction_id,
        } => {
            value.ice_type = kind.native()?;
            value.candidate_pair_id = candidate_pair_id;
            value.transaction_id = transaction_id;
            13
        }
        RtcEventData::ProbeClusterCreated {
            id,
            bitrate_bps,
            min_probes,
            min_bytes,
        } => {
            value.probe_id = id;
            value.bitrate_bps = bitrate_bps;
            value.min_probes = min_probes;
            value.min_bytes = min_bytes;
            14
        }
        RtcEventData::ProbeResultFailure { id, reason } => {
            value.probe_id = id;
            value.failure_reason = reason.native()?;
            15
        }
        RtcEventData::ProbeResultSuccess { id, bitrate_bps } => {
            value.probe_id = id;
            value.bitrate_bps = bitrate_bps;
            16
        }
        RtcEventData::RtcpPacketIncoming(bytes) => {
            value.bytes = bytes;
            17
        }
        RtcEventData::RtcpPacketOutgoing(bytes) => {
            value.bytes = bytes;
            18
        }
        RtcEventData::VideoReceiveStreamConfig(config) => {
            value.stream = stream(config)?;
            21
        }
        RtcEventData::VideoSendStreamConfig(config) => {
            value.stream = stream(config)?;
            22
        }
        RtcEventData::FrameDecoded {
            render_time_ms,
            ssrc,
            width,
            height,
            codec,
            qp,
        } => {
            value.render_time_ms = render_time_ms;
            value.ssrc = ssrc;
            value.width = width;
            value.height = height;
            value.video_codec = codec.native()?;
            value.qp = qp;
            23
        }
        RtcEventData::NetEqSetMinimumDelay {
            remote_ssrc,
            minimum_delay_ms,
        } => {
            value.ssrc = remote_ssrc;
            value.minimum_delay_ms = minimum_delay_ms;
            24
        }
        RtcEventData::RtpPacketIncoming(_)
        | RtcEventData::RtpPacketOutgoing { .. }
        | RtcEventData::BeginLog
        | RtcEventData::EndLog => return Err(EventLogError::IncompleteEventData),
        RtcEventData::Unknown(_) => return Err(EventLogError::UnsupportedEventKind),
    };
    Ok((kind, value))
}
