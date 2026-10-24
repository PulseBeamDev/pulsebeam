use pulsebeam_webrtc_sys::*;

fn stream() -> RtcEventStreamConfig {
    RtcEventStreamConfig {
        local_ssrc: 11,
        remote_ssrc: 22,
        rtx_ssrc: 33,
        rsid: "open-stream-token".into(),
        remb: true,
        rtp_extensions: vec![RtcEventRtpExtension {
            uri: "urn:application:unknown-extension".into(),
            id: 15,
            encrypted: true,
        }],
        rtcp_mode: RtcEventRtcpMode::Compound,
        codecs: vec![RtcEventStreamCodec {
            payload_name: "application-codec".into(),
            payload_type: 113,
            rtx_payload_type: 114,
        }],
    }
}

#[test]
fn ordinary_event_constructor_copy_and_payload_fidelity() {
    let events = vec![
        RtcEventData::AlrState { in_alr: true },
        RtcEventData::RouteChange {
            connected: false,
            overhead: 123,
        },
        RtcEventData::RemoteEstimate {
            link_capacity_lower: RtcEventDataRate::BitsPerSecond(128_123),
            link_capacity_upper: RtcEventDataRate::PositiveInfinity,
        },
        RtcEventData::RemoteEstimate {
            link_capacity_lower: RtcEventDataRate::NegativeInfinity,
            link_capacity_upper: RtcEventDataRate::BitsPerSecond(0),
        },
        RtcEventData::AudioNetworkAdaptation(RtcEventAudioAdaptation {
            bitrate_bps: Some(0),
            frame_length_ms: None,
            uplink_packet_loss_fraction: Some(0.125),
            enable_fec: Some(false),
            enable_dtx: None,
            channels: Some(0),
        }),
        RtcEventData::AudioPlayout { ssrc: u32::MAX },
        RtcEventData::AudioReceiveStreamConfig(stream()),
        RtcEventData::AudioSendStreamConfig(stream()),
        RtcEventData::BweUpdateDelayBased {
            bitrate_bps: 93_123,
            detector_state: RtcEventBandwidthUsage::Overusing,
        },
        RtcEventData::BweUpdateLossBased {
            bitrate_bps: -1,
            fraction_loss: u8::MAX,
            total_packets: -2,
        },
        RtcEventData::BweUpdateScream {
            ref_window_bytes: 3,
            data_in_flight_bytes: 9,
            target_rate_kbps: 17,
            smoothed_rtt_ms: 23,
            avg_queue_delay_ms: 31,
            l4s_marked_permille: 1_001,
        },
        RtcEventData::DtlsTransportState(RtcEventDtlsState::Failed),
        RtcEventData::DtlsWritableState { writable: true },
        RtcEventData::IceCandidatePairConfig {
            kind: RtcEventIceConfigType::Selected,
            candidate_pair_id: 19,
            description: RtcEventIcePairDescription {
                local_candidate_type: RtcEventIceCandidateType::Relay,
                local_relay_protocol: RtcEventIceProtocol::Tls,
                local_network_type: RtcEventIceNetworkType::Vpn,
                local_address_family: RtcEventIceAddressFamily::Ipv6,
                remote_candidate_type: RtcEventIceCandidateType::PeerReflexive,
                remote_address_family: RtcEventIceAddressFamily::Ipv4,
                candidate_pair_protocol: RtcEventIceProtocol::Udp,
            },
        },
        RtcEventData::IceCandidatePair {
            kind: RtcEventIcePairType::CheckResponseReceived,
            candidate_pair_id: 37,
            transaction_id: u32::MAX,
        },
        RtcEventData::ProbeClusterCreated {
            id: -2,
            bitrate_bps: 7,
            min_probes: u32::MAX,
            min_bytes: 13,
        },
        RtcEventData::ProbeResultFailure {
            id: 17,
            reason: RtcEventProbeFailure::Timeout,
        },
        RtcEventData::ProbeResultSuccess {
            id: 19,
            bitrate_bps: 23,
        },
        RtcEventData::RtcpPacketIncoming(vec![0, 1, 0, 255]),
        RtcEventData::RtcpPacketOutgoing(vec![128, 200, 0, 0]),
        RtcEventData::VideoReceiveStreamConfig(stream()),
        RtcEventData::VideoSendStreamConfig(stream()),
        RtcEventData::FrameDecoded {
            render_time_ms: -1,
            ssrc: 29,
            width: -2,
            height: 33,
            codec: RtcEventVideoCodec::H265,
            qp: u8::MAX,
        },
        RtcEventData::NetEqSetMinimumDelay {
            remote_ssrc: 39,
            minimum_delay_ms: -4,
        },
    ];
    for data in events {
        let mut event = RtcEvent::from_data(data.clone()).unwrap();
        event.set_timestamp_us(123_456).unwrap();
        assert_eq!(event.data(), data);
        let is_config = matches!(
            data,
            RtcEventData::AudioReceiveStreamConfig(_)
                | RtcEventData::AudioSendStreamConfig(_)
                | RtcEventData::VideoReceiveStreamConfig(_)
                | RtcEventData::VideoSendStreamConfig(_)
        );
        assert_eq!(event.is_config_event(), is_config);
        let copy = event.try_clone().unwrap();
        assert_eq!(copy.kind(), event.kind());
        assert_eq!(copy.group_key(), event.group_key());
        assert_eq!(copy.timestamp_us(), 123_456);
        drop(event);
        assert_eq!(copy.data(), data);
        assert_eq!(copy.timestamp_ms(), 123);
        std::thread::scope(|scope| {
            scope
                .spawn(|| assert_eq!(copy.data(), data))
                .join()
                .unwrap();
        });
    }
}

#[test]
fn rtp_packet_parsing_owns_header_and_reports_real_packet_sizes() {
    // Version 2, padding, marker, payload 96, sequence 0x1234, timestamp
    // 0x01020304, SSRC 0x05060708, 3 payload bytes, then 2 padding bytes.
    let bytes = vec![
        0xa0, 0xe0, 0x12, 0x34, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 2,
    ];
    let incoming = RtcEvent::rtp_incoming(&bytes, &[], Some(0xabcd)).unwrap();
    let outgoing = RtcEvent::rtp_outgoing(&bytes, &[], 113, None).unwrap();
    let RtcEventData::RtpPacketIncoming(packet) = incoming.data() else {
        panic!("incoming RTP kind")
    };
    assert_eq!(packet.raw_header, bytes[..12]);
    assert_eq!(packet.packet_length, 17);
    assert_eq!(packet.header_length, 12);
    assert_eq!(packet.payload_length, 3);
    assert_eq!(packet.padding_length, 2);
    assert_eq!(packet.sequence_number, 0x1234);
    assert_eq!(packet.timestamp, 0x01020304);
    assert_eq!(packet.ssrc, 0x05060708);
    assert_eq!(packet.payload_type, 96);
    assert!(packet.marker);
    assert_eq!(packet.rtx_original_sequence_number, Some(0xabcd));
    let RtcEventData::RtpPacketOutgoing {
        packet,
        probe_cluster_id,
    } = outgoing.data()
    else {
        panic!("outgoing RTP kind")
    };
    assert_eq!(probe_cluster_id, 113);
    assert_eq!(packet.rtx_original_sequence_number, None);
    let incoming_copy = incoming.try_clone().unwrap();
    let outgoing_copy = outgoing.try_clone().unwrap();
    let incoming_data = incoming.data();
    let outgoing_data = outgoing.data();
    drop((incoming, outgoing, bytes));
    assert_eq!(incoming_copy.data(), incoming_data);
    assert_eq!(outgoing_copy.data(), outgoing_data);
    assert!(matches!(
        RtcEvent::from_data(incoming_data),
        Err(EventLogError::IncompleteEventData)
    ));
    for bytes in [&[][..], &[0x80; 11][..], &[0; 12][..]] {
        assert!(matches!(
            RtcEvent::rtp_incoming(bytes, &[], None),
            Err(EventLogError::InvalidRtpPacket)
        ));
        assert!(matches!(
            RtcEvent::rtp_outgoing(bytes, &[], 0, None),
            Err(EventLogError::InvalidRtpPacket)
        ));
    }
}

#[test]
fn boundary_timestamps_and_constructor_domain_errors() {
    let begin = RtcEvent::begin_log(987_654, 123_456_789).unwrap();
    assert_eq!(begin.kind(), RtcEventKind::BeginLog);
    assert_eq!(begin.data(), RtcEventData::BeginLog);
    assert_eq!(begin.timestamp_us(), 987_654);
    let end = RtcEvent::end_log(999_999).unwrap();
    assert_eq!(end.kind(), RtcEventKind::EndLog);
    assert_eq!(end.timestamp_ms(), 999);
    assert!(matches!(
        begin.try_clone(),
        Err(EventLogError::UnsupportedEventKind)
    ));
    assert!(matches!(
        end.try_clone(),
        Err(EventLogError::UnsupportedEventKind)
    ));
    assert_eq!(RtcEvent::begin_log(-1, 0).unwrap().timestamp_us(), -1);
    assert!(matches!(
        RtcEvent::begin_log(i64::MIN, 0),
        Err(EventLogError::InvalidTimestamp)
    ));
    assert!(matches!(
        RtcEvent::begin_log(0, i64::MAX),
        Err(EventLogError::InvalidTimestamp)
    ));
    assert!(matches!(
        RtcEvent::end_log(i64::MAX),
        Err(EventLogError::InvalidTimestamp)
    ));
    assert!(matches!(
        RtcEvent::from_data(RtcEventData::DtlsTransportState(
            RtcEventDtlsState::Unknown(5)
        )),
        Err(EventLogError::InvalidEventEnum)
    ));
    assert!(matches!(
        RtcEvent::from_data(RtcEventData::RemoteEstimate {
            link_capacity_lower: RtcEventDataRate::BitsPerSecond(-1),
            link_capacity_upper: RtcEventDataRate::BitsPerSecond(i64::MAX),
        }),
        Err(EventLogError::InvalidRate)
    ));
    let bytes = [0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    for id in [0, -1, 256, i32::MAX] {
        let extensions = [RtcEventRtpExtension {
            id,
            uri: "urn:application:open".into(),
            encrypted: false,
        }];
        assert!(matches!(
            RtcEvent::rtp_incoming(&bytes, &extensions, None),
            Err(EventLogError::InvalidRtpExtensionId)
        ));
    }
}
