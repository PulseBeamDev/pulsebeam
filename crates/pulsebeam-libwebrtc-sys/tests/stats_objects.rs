use pulsebeam_webrtc_sys::*;
use std::collections::BTreeMap;

#[test]
fn every_public_stats_field_is_observed_through_independent_native_attributes() {
    let time = Timestamp::from_microseconds(23).unwrap();
    {
        let mut stat =
            RtcStats::new(RtcStatsClass::CertificateStats, b"CertificateStats", time).unwrap();
        let values = RtcStatsValues::CertificateStats(RtcCertificateStatsValues {
            fingerprint: Some(vec![0xff, 0, 1]),
            fingerprint_algorithm: Some(vec![0xff, 0, 2]),
            base64_certificate: Some(vec![0xff, 0, 3]),
            issuer_certificate_id: Some(vec![0xff, 0, 4]),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"fingerprint".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 1])),
            ),
            (
                b"fingerprintAlgorithm".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
            (
                b"base64Certificate".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"issuerCertificateId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "CertificateStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("CertificateStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(RtcStatsClass::CodecStats, b"CodecStats", time).unwrap();
        let values = RtcStatsValues::CodecStats(RtcCodecStatsValues {
            transport_id: Some(vec![0xff, 0, 1]),
            payload_type: Some(u32::MAX - 2),
            mime_type: Some(vec![0xff, 0, 3]),
            clock_rate: Some(u32::MAX - 4),
            channels: Some(u32::MAX - 5),
            sdp_fmtp_line: Some(vec![0xff, 0, 6]),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"transportId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 1])),
            ),
            (
                b"payloadType".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 2)),
            ),
            (
                b"mimeType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"clockRate".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 4)),
            ),
            (
                b"channels".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 5)),
            ),
            (
                b"sdpFmtpLine".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 6])),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "CodecStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("CodecStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat =
            RtcStats::new(RtcStatsClass::DataChannelStats, b"DataChannelStats", time).unwrap();
        let values = RtcStatsValues::DataChannelStats(RtcDataChannelStatsValues {
            label: Some(vec![0xff, 0, 1]),
            protocol: Some(vec![0xff, 0, 2]),
            data_channel_identifier: Some(i32::MIN + 3),
            state: Some(vec![0xff, 0, 4]),
            messages_sent: Some(u32::MAX - 5),
            bytes_sent: Some(u64::MAX - 6),
            messages_received: Some(u32::MAX - 7),
            bytes_received: Some(u64::MAX - 8),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"label".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 1])),
            ),
            (
                b"protocol".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
            (
                b"dataChannelIdentifier".to_vec(),
                RtcStatsValue::Int32(Some(i32::MIN + 3)),
            ),
            (
                b"state".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
            (
                b"messagesSent".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 5)),
            ),
            (
                b"bytesSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 6)),
            ),
            (
                b"messagesReceived".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 7)),
            ),
            (
                b"bytesReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 8)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "DataChannelStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("DataChannelStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(
            RtcStatsClass::IceCandidatePairStats,
            b"IceCandidatePairStats",
            time,
        )
        .unwrap();
        let values = RtcStatsValues::IceCandidatePairStats(RtcIceCandidatePairStatsValues {
            transport_id: Some(vec![0xff, 0, 1]),
            local_candidate_id: Some(vec![0xff, 0, 2]),
            remote_candidate_id: Some(vec![0xff, 0, 3]),
            state: Some(vec![0xff, 0, 4]),
            priority: Some(u64::MAX - 5),
            nominated: Some(true),
            writable: Some(false),
            packets_sent: Some(u64::MAX - 8),
            packets_received: Some(u64::MAX - 9),
            bytes_sent: Some(u64::MAX - 10),
            bytes_received: Some(u64::MAX - 11),
            total_round_trip_time: Some(12.25),
            current_round_trip_time: Some(13.25),
            available_outgoing_bitrate: Some(14.25),
            available_incoming_bitrate: Some(15.25),
            requests_received: Some(u64::MAX - 16),
            requests_sent: Some(u64::MAX - 17),
            responses_received: Some(u64::MAX - 18),
            responses_sent: Some(u64::MAX - 19),
            consent_requests_sent: Some(u64::MAX - 20),
            packets_discarded_on_send: Some(u64::MAX - 21),
            bytes_discarded_on_send: Some(u64::MAX - 22),
            last_packet_received_timestamp: Some(23.25),
            last_packet_sent_timestamp: Some(24.25),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"transportId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 1])),
            ),
            (
                b"localCandidateId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
            (
                b"remoteCandidateId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"state".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
            (
                b"priority".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 5)),
            ),
            (b"nominated".to_vec(), RtcStatsValue::Bool(Some(true))),
            (b"writable".to_vec(), RtcStatsValue::Bool(Some(false))),
            (
                b"packetsSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 8)),
            ),
            (
                b"packetsReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 9)),
            ),
            (
                b"bytesSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 10)),
            ),
            (
                b"bytesReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 11)),
            ),
            (
                b"totalRoundTripTime".to_vec(),
                RtcStatsValue::Double(Some(12.25)),
            ),
            (
                b"currentRoundTripTime".to_vec(),
                RtcStatsValue::Double(Some(13.25)),
            ),
            (
                b"availableOutgoingBitrate".to_vec(),
                RtcStatsValue::Double(Some(14.25)),
            ),
            (
                b"availableIncomingBitrate".to_vec(),
                RtcStatsValue::Double(Some(15.25)),
            ),
            (
                b"requestsReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 16)),
            ),
            (
                b"requestsSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 17)),
            ),
            (
                b"responsesReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 18)),
            ),
            (
                b"responsesSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 19)),
            ),
            (
                b"consentRequestsSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 20)),
            ),
            (
                b"packetsDiscardedOnSend".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 21)),
            ),
            (
                b"bytesDiscardedOnSend".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 22)),
            ),
            (
                b"lastPacketReceivedTimestamp".to_vec(),
                RtcStatsValue::Double(Some(23.25)),
            ),
            (
                b"lastPacketSentTimestamp".to_vec(),
                RtcStatsValue::Double(Some(24.25)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "IceCandidatePairStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("IceCandidatePairStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(
            RtcStatsClass::LocalIceCandidateStats,
            b"IceCandidateStats",
            time,
        )
        .unwrap()
        .copy_as(RtcStatsClass::IceCandidateStats)
        .unwrap();
        let values = RtcStatsValues::IceCandidateStats(RtcIceCandidateStatsValues {
            transport_id: Some(vec![0xff, 0, 1]),
            is_remote: Some(true),
            network_type: Some(vec![0xff, 0, 3]),
            ip: Some(vec![0xff, 0, 4]),
            address: Some(vec![0xff, 0, 5]),
            port: Some(i32::MIN + 6),
            protocol: Some(vec![0xff, 0, 7]),
            relay_protocol: Some(vec![0xff, 0, 8]),
            candidate_type: Some(vec![0xff, 0, 9]),
            priority: Some(i32::MIN + 10),
            url: Some(vec![0xff, 0, 11]),
            foundation: Some(vec![0xff, 0, 12]),
            related_address: Some(vec![0xff, 0, 13]),
            related_port: Some(i32::MIN + 14),
            username_fragment: Some(vec![0xff, 0, 15]),
            tcp_type: Some(vec![0xff, 0, 16]),
            vpn: Some(false),
            network_adapter_type: Some(vec![0xff, 0, 18]),
            network_slice: Some(vec![0xff, 0, 19]),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"transportId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 1])),
            ),
            (b"isRemote".to_vec(), RtcStatsValue::Bool(Some(true))),
            (
                b"networkType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"ip".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
            (
                b"address".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 5])),
            ),
            (b"port".to_vec(), RtcStatsValue::Int32(Some(i32::MIN + 6))),
            (
                b"protocol".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 7])),
            ),
            (
                b"relayProtocol".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 8])),
            ),
            (
                b"candidateType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 9])),
            ),
            (
                b"priority".to_vec(),
                RtcStatsValue::Int32(Some(i32::MIN + 10)),
            ),
            (
                b"url".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 11])),
            ),
            (
                b"foundation".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 12])),
            ),
            (
                b"relatedAddress".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 13])),
            ),
            (
                b"relatedPort".to_vec(),
                RtcStatsValue::Int32(Some(i32::MIN + 14)),
            ),
            (
                b"usernameFragment".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 15])),
            ),
            (
                b"tcpType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 16])),
            ),
            (b"vpn".to_vec(), RtcStatsValue::Bool(Some(false))),
            (
                b"networkAdapterType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 18])),
            ),
            (
                b"networkSlice".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 19])),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "IceCandidateStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("IceCandidateStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(
            RtcStatsClass::LocalIceCandidateStats,
            b"LocalIceCandidateStats",
            time,
        )
        .unwrap();
        let values = RtcStatsValues::LocalIceCandidateStats(RtcLocalIceCandidateStatsValues {
            transport_id: Some(vec![0xff, 0, 1]),
            is_remote: Some(true),
            network_type: Some(vec![0xff, 0, 3]),
            ip: Some(vec![0xff, 0, 4]),
            address: Some(vec![0xff, 0, 5]),
            port: Some(i32::MIN + 6),
            protocol: Some(vec![0xff, 0, 7]),
            relay_protocol: Some(vec![0xff, 0, 8]),
            candidate_type: Some(vec![0xff, 0, 9]),
            priority: Some(i32::MIN + 10),
            url: Some(vec![0xff, 0, 11]),
            foundation: Some(vec![0xff, 0, 12]),
            related_address: Some(vec![0xff, 0, 13]),
            related_port: Some(i32::MIN + 14),
            username_fragment: Some(vec![0xff, 0, 15]),
            tcp_type: Some(vec![0xff, 0, 16]),
            vpn: Some(false),
            network_adapter_type: Some(vec![0xff, 0, 18]),
            network_slice: Some(vec![0xff, 0, 19]),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"transportId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 1])),
            ),
            (b"isRemote".to_vec(), RtcStatsValue::Bool(Some(true))),
            (
                b"networkType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"ip".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
            (
                b"address".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 5])),
            ),
            (b"port".to_vec(), RtcStatsValue::Int32(Some(i32::MIN + 6))),
            (
                b"protocol".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 7])),
            ),
            (
                b"relayProtocol".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 8])),
            ),
            (
                b"candidateType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 9])),
            ),
            (
                b"priority".to_vec(),
                RtcStatsValue::Int32(Some(i32::MIN + 10)),
            ),
            (
                b"url".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 11])),
            ),
            (
                b"foundation".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 12])),
            ),
            (
                b"relatedAddress".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 13])),
            ),
            (
                b"relatedPort".to_vec(),
                RtcStatsValue::Int32(Some(i32::MIN + 14)),
            ),
            (
                b"usernameFragment".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 15])),
            ),
            (
                b"tcpType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 16])),
            ),
            (b"vpn".to_vec(), RtcStatsValue::Bool(Some(false))),
            (
                b"networkAdapterType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 18])),
            ),
            (
                b"networkSlice".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 19])),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "LocalIceCandidateStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("LocalIceCandidateStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(
            RtcStatsClass::RemoteIceCandidateStats,
            b"RemoteIceCandidateStats",
            time,
        )
        .unwrap();
        let values = RtcStatsValues::RemoteIceCandidateStats(RtcRemoteIceCandidateStatsValues {
            transport_id: Some(vec![0xff, 0, 1]),
            is_remote: Some(true),
            network_type: Some(vec![0xff, 0, 3]),
            ip: Some(vec![0xff, 0, 4]),
            address: Some(vec![0xff, 0, 5]),
            port: Some(i32::MIN + 6),
            protocol: Some(vec![0xff, 0, 7]),
            relay_protocol: Some(vec![0xff, 0, 8]),
            candidate_type: Some(vec![0xff, 0, 9]),
            priority: Some(i32::MIN + 10),
            url: Some(vec![0xff, 0, 11]),
            foundation: Some(vec![0xff, 0, 12]),
            related_address: Some(vec![0xff, 0, 13]),
            related_port: Some(i32::MIN + 14),
            username_fragment: Some(vec![0xff, 0, 15]),
            tcp_type: Some(vec![0xff, 0, 16]),
            vpn: Some(false),
            network_adapter_type: Some(vec![0xff, 0, 18]),
            network_slice: Some(vec![0xff, 0, 19]),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"transportId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 1])),
            ),
            (b"isRemote".to_vec(), RtcStatsValue::Bool(Some(true))),
            (
                b"networkType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"ip".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
            (
                b"address".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 5])),
            ),
            (b"port".to_vec(), RtcStatsValue::Int32(Some(i32::MIN + 6))),
            (
                b"protocol".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 7])),
            ),
            (
                b"relayProtocol".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 8])),
            ),
            (
                b"candidateType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 9])),
            ),
            (
                b"priority".to_vec(),
                RtcStatsValue::Int32(Some(i32::MIN + 10)),
            ),
            (
                b"url".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 11])),
            ),
            (
                b"foundation".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 12])),
            ),
            (
                b"relatedAddress".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 13])),
            ),
            (
                b"relatedPort".to_vec(),
                RtcStatsValue::Int32(Some(i32::MIN + 14)),
            ),
            (
                b"usernameFragment".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 15])),
            ),
            (
                b"tcpType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 16])),
            ),
            (b"vpn".to_vec(), RtcStatsValue::Bool(Some(false))),
            (
                b"networkAdapterType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 18])),
            ),
            (
                b"networkSlice".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 19])),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "RemoteIceCandidateStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("RemoteIceCandidateStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(
            RtcStatsClass::PeerConnectionStats,
            b"PeerConnectionStats",
            time,
        )
        .unwrap();
        let values = RtcStatsValues::PeerConnectionStats(RtcPeerConnectionStatsValues {
            data_channels_opened: Some(u32::MAX - 1),
            data_channels_closed: Some(u32::MAX - 2),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"dataChannelsOpened".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 1)),
            ),
            (
                b"dataChannelsClosed".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 2)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "PeerConnectionStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("PeerConnectionStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(
            RtcStatsClass::InboundRtpStreamStats,
            b"RtpStreamStats",
            time,
        )
        .unwrap()
        .copy_as(RtcStatsClass::RtpStreamStats)
        .unwrap();
        let values = RtcStatsValues::RtpStreamStats(RtcRtpStreamStatsValues {
            ssrc: Some(u32::MAX - 1),
            kind: Some(vec![0xff, 0, 2]),
            transport_id: Some(vec![0xff, 0, 3]),
            codec_id: Some(vec![0xff, 0, 4]),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (b"ssrc".to_vec(), RtcStatsValue::Uint32(Some(u32::MAX - 1))),
            (
                b"kind".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
            (
                b"transportId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"codecId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "RtpStreamStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("RtpStreamStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(
            RtcStatsClass::InboundRtpStreamStats,
            b"ReceivedRtpStreamStats",
            time,
        )
        .unwrap()
        .copy_as(RtcStatsClass::ReceivedRtpStreamStats)
        .unwrap();
        let values = RtcStatsValues::ReceivedRtpStreamStats(RtcReceivedRtpStreamStatsValues {
            ssrc: Some(u32::MAX - 1),
            kind: Some(vec![0xff, 0, 2]),
            transport_id: Some(vec![0xff, 0, 3]),
            codec_id: Some(vec![0xff, 0, 4]),
            jitter: Some(5.25),
            packets_lost: Some(i32::MIN + 6),
            packets_received_with_ect1: Some(i64::MIN + 7),
            packets_received_with_ce: Some(i64::MIN + 8),
            packets_reported_as_lost: Some(i64::MIN + 9),
            packets_reported_as_lost_but_recovered: Some(i64::MIN + 10),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (b"ssrc".to_vec(), RtcStatsValue::Uint32(Some(u32::MAX - 1))),
            (
                b"kind".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
            (
                b"transportId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"codecId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
            (b"jitter".to_vec(), RtcStatsValue::Double(Some(5.25))),
            (
                b"packetsLost".to_vec(),
                RtcStatsValue::Int32(Some(i32::MIN + 6)),
            ),
            (
                b"packetsReceivedWithEct1".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 7)),
            ),
            (
                b"packetsReceivedWithCe".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 8)),
            ),
            (
                b"packetsReportedAsLost".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 9)),
            ),
            (
                b"packetsReportedAsLostButRecovered".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 10)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "ReceivedRtpStreamStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("ReceivedRtpStreamStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(
            RtcStatsClass::OutboundRtpStreamStats,
            b"SentRtpStreamStats",
            time,
        )
        .unwrap()
        .copy_as(RtcStatsClass::SentRtpStreamStats)
        .unwrap();
        let values = RtcStatsValues::SentRtpStreamStats(RtcSentRtpStreamStatsValues {
            ssrc: Some(u32::MAX - 1),
            kind: Some(vec![0xff, 0, 2]),
            transport_id: Some(vec![0xff, 0, 3]),
            codec_id: Some(vec![0xff, 0, 4]),
            packets_sent: Some(u64::MAX - 5),
            bytes_sent: Some(u64::MAX - 6),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (b"ssrc".to_vec(), RtcStatsValue::Uint32(Some(u32::MAX - 1))),
            (
                b"kind".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
            (
                b"transportId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"codecId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
            (
                b"packetsSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 5)),
            ),
            (
                b"bytesSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 6)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "SentRtpStreamStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("SentRtpStreamStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(
            RtcStatsClass::InboundRtpStreamStats,
            b"InboundRtpStreamStats",
            time,
        )
        .unwrap();
        let values = RtcStatsValues::InboundRtpStreamStats(RtcInboundRtpStreamStatsValues {
            ssrc: Some(u32::MAX - 1),
            kind: Some(vec![0xff, 0, 2]),
            transport_id: Some(vec![0xff, 0, 3]),
            codec_id: Some(vec![0xff, 0, 4]),
            jitter: Some(5.25),
            packets_lost: Some(i32::MIN + 6),
            packets_received_with_ect1: Some(i64::MIN + 7),
            packets_received_with_ce: Some(i64::MIN + 8),
            packets_reported_as_lost: Some(i64::MIN + 9),
            packets_reported_as_lost_but_recovered: Some(i64::MIN + 10),
            playout_id: Some(vec![0xff, 0, 11]),
            track_identifier: Some(vec![0xff, 0, 12]),
            mid: Some(vec![0xff, 0, 13]),
            remote_id: Some(vec![0xff, 0, 14]),
            packets_received: Some(u32::MAX - 15),
            packets_discarded: Some(u64::MAX - 16),
            fec_packets_received: Some(u64::MAX - 17),
            fec_bytes_received: Some(u64::MAX - 18),
            fec_packets_discarded: Some(u64::MAX - 19),
            fec_ssrc: Some(u32::MAX - 20),
            bytes_received: Some(u64::MAX - 21),
            header_bytes_received: Some(u64::MAX - 22),
            retransmitted_packets_received: Some(u64::MAX - 23),
            retransmitted_bytes_received: Some(u64::MAX - 24),
            rtx_ssrc: Some(u32::MAX - 25),
            last_packet_received_timestamp: Some(26.25),
            jitter_buffer_delay: Some(27.25),
            jitter_buffer_target_delay: Some(28.25),
            jitter_buffer_minimum_delay: Some(29.25),
            jitter_buffer_emitted_count: Some(u64::MAX - 30),
            total_samples_received: Some(u64::MAX - 31),
            concealed_samples: Some(u64::MAX - 32),
            silent_concealed_samples: Some(u64::MAX - 33),
            concealment_events: Some(u64::MAX - 34),
            inserted_samples_for_deceleration: Some(u64::MAX - 35),
            removed_samples_for_acceleration: Some(u64::MAX - 36),
            audio_level: Some(37.25),
            total_audio_energy: Some(38.25),
            total_samples_duration: Some(39.25),
            frames_received: Some(u32::MAX - 40),
            frame_width: Some(u32::MAX - 41),
            frame_height: Some(u32::MAX - 42),
            frames_per_second: Some(43.25),
            frames_decoded: Some(u32::MAX - 44),
            key_frames_decoded: Some(u32::MAX - 45),
            frames_dropped: Some(u32::MAX - 46),
            total_decode_time: Some(47.25),
            total_processing_delay: Some(48.25),
            total_assembly_time: Some(49.25),
            frames_assembled_from_multiple_packets: Some(u32::MAX - 50),
            total_inter_frame_delay: Some(51.25),
            total_squared_inter_frame_delay: Some(52.25),
            pause_count: Some(u32::MAX - 53),
            total_pauses_duration: Some(54.25),
            freeze_count: Some(u32::MAX - 55),
            total_freezes_duration: Some(56.25),
            content_type: Some(vec![0xff, 0, 57]),
            estimated_playout_timestamp: Some(58.25),
            decoder_implementation: Some(vec![0xff, 0, 59]),
            fir_count: Some(u32::MAX - 60),
            pli_count: Some(u32::MAX - 61),
            nack_count: Some(u32::MAX - 62),
            qp_sum: Some(u64::MAX - 63),
            total_corruption_probability: Some(64.25),
            total_squared_corruption_probability: Some(65.25),
            corruption_measurements: Some(u64::MAX - 66),
            goog_timing_frame_info: Some(vec![0xff, 0, 67]),
            power_efficient_decoder: Some(true),
            jitter_buffer_flushes: Some(u64::MAX - 69),
            delayed_packet_outage_samples: Some(u64::MAX - 70),
            relative_packet_arrival_delay: Some(71.25),
            interruption_count: Some(u32::MAX - 72),
            total_interruption_duration: Some(73.25),
            min_playout_delay: Some(74.25),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (b"ssrc".to_vec(), RtcStatsValue::Uint32(Some(u32::MAX - 1))),
            (
                b"kind".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
            (
                b"transportId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"codecId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
            (b"jitter".to_vec(), RtcStatsValue::Double(Some(5.25))),
            (
                b"packetsLost".to_vec(),
                RtcStatsValue::Int32(Some(i32::MIN + 6)),
            ),
            (
                b"packetsReceivedWithEct1".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 7)),
            ),
            (
                b"packetsReceivedWithCe".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 8)),
            ),
            (
                b"packetsReportedAsLost".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 9)),
            ),
            (
                b"packetsReportedAsLostButRecovered".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 10)),
            ),
            (
                b"playoutId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 11])),
            ),
            (
                b"trackIdentifier".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 12])),
            ),
            (
                b"mid".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 13])),
            ),
            (
                b"remoteId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 14])),
            ),
            (
                b"packetsReceived".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 15)),
            ),
            (
                b"packetsDiscarded".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 16)),
            ),
            (
                b"fecPacketsReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 17)),
            ),
            (
                b"fecBytesReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 18)),
            ),
            (
                b"fecPacketsDiscarded".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 19)),
            ),
            (
                b"fecSsrc".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 20)),
            ),
            (
                b"bytesReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 21)),
            ),
            (
                b"headerBytesReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 22)),
            ),
            (
                b"retransmittedPacketsReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 23)),
            ),
            (
                b"retransmittedBytesReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 24)),
            ),
            (
                b"rtxSsrc".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 25)),
            ),
            (
                b"lastPacketReceivedTimestamp".to_vec(),
                RtcStatsValue::Double(Some(26.25)),
            ),
            (
                b"jitterBufferDelay".to_vec(),
                RtcStatsValue::Double(Some(27.25)),
            ),
            (
                b"jitterBufferTargetDelay".to_vec(),
                RtcStatsValue::Double(Some(28.25)),
            ),
            (
                b"jitterBufferMinimumDelay".to_vec(),
                RtcStatsValue::Double(Some(29.25)),
            ),
            (
                b"jitterBufferEmittedCount".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 30)),
            ),
            (
                b"totalSamplesReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 31)),
            ),
            (
                b"concealedSamples".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 32)),
            ),
            (
                b"silentConcealedSamples".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 33)),
            ),
            (
                b"concealmentEvents".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 34)),
            ),
            (
                b"insertedSamplesForDeceleration".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 35)),
            ),
            (
                b"removedSamplesForAcceleration".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 36)),
            ),
            (b"audioLevel".to_vec(), RtcStatsValue::Double(Some(37.25))),
            (
                b"totalAudioEnergy".to_vec(),
                RtcStatsValue::Double(Some(38.25)),
            ),
            (
                b"totalSamplesDuration".to_vec(),
                RtcStatsValue::Double(Some(39.25)),
            ),
            (
                b"framesReceived".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 40)),
            ),
            (
                b"frameWidth".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 41)),
            ),
            (
                b"frameHeight".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 42)),
            ),
            (
                b"framesPerSecond".to_vec(),
                RtcStatsValue::Double(Some(43.25)),
            ),
            (
                b"framesDecoded".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 44)),
            ),
            (
                b"keyFramesDecoded".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 45)),
            ),
            (
                b"framesDropped".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 46)),
            ),
            (
                b"totalDecodeTime".to_vec(),
                RtcStatsValue::Double(Some(47.25)),
            ),
            (
                b"totalProcessingDelay".to_vec(),
                RtcStatsValue::Double(Some(48.25)),
            ),
            (
                b"totalAssemblyTime".to_vec(),
                RtcStatsValue::Double(Some(49.25)),
            ),
            (
                b"framesAssembledFromMultiplePackets".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 50)),
            ),
            (
                b"totalInterFrameDelay".to_vec(),
                RtcStatsValue::Double(Some(51.25)),
            ),
            (
                b"totalSquaredInterFrameDelay".to_vec(),
                RtcStatsValue::Double(Some(52.25)),
            ),
            (
                b"pauseCount".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 53)),
            ),
            (
                b"totalPausesDuration".to_vec(),
                RtcStatsValue::Double(Some(54.25)),
            ),
            (
                b"freezeCount".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 55)),
            ),
            (
                b"totalFreezesDuration".to_vec(),
                RtcStatsValue::Double(Some(56.25)),
            ),
            (
                b"contentType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 57])),
            ),
            (
                b"estimatedPlayoutTimestamp".to_vec(),
                RtcStatsValue::Double(Some(58.25)),
            ),
            (
                b"decoderImplementation".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 59])),
            ),
            (
                b"firCount".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 60)),
            ),
            (
                b"pliCount".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 61)),
            ),
            (
                b"nackCount".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 62)),
            ),
            (
                b"qpSum".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 63)),
            ),
            (
                b"totalCorruptionProbability".to_vec(),
                RtcStatsValue::Double(Some(64.25)),
            ),
            (
                b"totalSquaredCorruptionProbability".to_vec(),
                RtcStatsValue::Double(Some(65.25)),
            ),
            (
                b"corruptionMeasurements".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 66)),
            ),
            (
                b"googTimingFrameInfo".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 67])),
            ),
            (
                b"powerEfficientDecoder".to_vec(),
                RtcStatsValue::Bool(Some(true)),
            ),
            (
                b"jitterBufferFlushes".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 69)),
            ),
            (
                b"delayedPacketOutageSamples".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 70)),
            ),
            (
                b"relativePacketArrivalDelay".to_vec(),
                RtcStatsValue::Double(Some(71.25)),
            ),
            (
                b"interruptionCount".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 72)),
            ),
            (
                b"totalInterruptionDuration".to_vec(),
                RtcStatsValue::Double(Some(73.25)),
            ),
            (
                b"minPlayoutDelay".to_vec(),
                RtcStatsValue::Double(Some(74.25)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "InboundRtpStreamStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("InboundRtpStreamStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(
            RtcStatsClass::OutboundRtpStreamStats,
            b"OutboundRtpStreamStats",
            time,
        )
        .unwrap();
        let values = RtcStatsValues::OutboundRtpStreamStats(RtcOutboundRtpStreamStatsValues {
            ssrc: Some(u32::MAX - 1),
            kind: Some(vec![0xff, 0, 2]),
            transport_id: Some(vec![0xff, 0, 3]),
            codec_id: Some(vec![0xff, 0, 4]),
            packets_sent: Some(u64::MAX - 5),
            bytes_sent: Some(u64::MAX - 6),
            media_source_id: Some(vec![0xff, 0, 7]),
            remote_id: Some(vec![0xff, 0, 8]),
            mid: Some(vec![0xff, 0, 9]),
            rid: Some(vec![0xff, 0, 10]),
            encoding_index: Some(u32::MAX - 11),
            retransmitted_packets_sent: Some(u64::MAX - 12),
            header_bytes_sent: Some(u64::MAX - 13),
            retransmitted_bytes_sent: Some(u64::MAX - 14),
            target_bitrate: Some(15.25),
            frames_encoded: Some(u32::MAX - 16),
            key_frames_encoded: Some(u32::MAX - 17),
            total_encode_time: Some(18.25),
            total_encoded_bytes_target: Some(u64::MAX - 19),
            frame_width: Some(u32::MAX - 20),
            frame_height: Some(u32::MAX - 21),
            frames_per_second: Some(22.25),
            frames_sent: Some(u32::MAX - 23),
            huge_frames_sent: Some(u32::MAX - 24),
            total_packet_send_delay: Some(25.25),
            quality_limitation_reason: Some(vec![0xff, 0, 26]),
            quality_limitation_durations: Some(BTreeMap::from([(vec![0xff, 0, 27], 27.5)])),
            quality_limitation_resolution_changes: Some(u32::MAX - 28),
            content_type: Some(vec![0xff, 0, 29]),
            encoder_implementation: Some(vec![0xff, 0, 30]),
            fir_count: Some(u32::MAX - 31),
            pli_count: Some(u32::MAX - 32),
            nack_count: Some(u32::MAX - 33),
            qp_sum: Some(u64::MAX - 34),
            psnr_sum: Some(BTreeMap::from([(vec![0xff, 0, 35], 35.5)])),
            psnr_measurements: Some(u32::MAX - 36),
            active: Some(false),
            power_efficient_encoder: Some(true),
            scalability_mode: Some(vec![0xff, 0, 39]),
            rtx_ssrc: Some(u32::MAX - 40),
            packets_sent_with_ect1: Some(i64::MIN + 41),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (b"ssrc".to_vec(), RtcStatsValue::Uint32(Some(u32::MAX - 1))),
            (
                b"kind".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
            (
                b"transportId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"codecId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
            (
                b"packetsSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 5)),
            ),
            (
                b"bytesSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 6)),
            ),
            (
                b"mediaSourceId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 7])),
            ),
            (
                b"remoteId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 8])),
            ),
            (
                b"mid".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 9])),
            ),
            (
                b"rid".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 10])),
            ),
            (
                b"encodingIndex".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 11)),
            ),
            (
                b"retransmittedPacketsSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 12)),
            ),
            (
                b"headerBytesSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 13)),
            ),
            (
                b"retransmittedBytesSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 14)),
            ),
            (
                b"targetBitrate".to_vec(),
                RtcStatsValue::Double(Some(15.25)),
            ),
            (
                b"framesEncoded".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 16)),
            ),
            (
                b"keyFramesEncoded".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 17)),
            ),
            (
                b"totalEncodeTime".to_vec(),
                RtcStatsValue::Double(Some(18.25)),
            ),
            (
                b"totalEncodedBytesTarget".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 19)),
            ),
            (
                b"frameWidth".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 20)),
            ),
            (
                b"frameHeight".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 21)),
            ),
            (
                b"framesPerSecond".to_vec(),
                RtcStatsValue::Double(Some(22.25)),
            ),
            (
                b"framesSent".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 23)),
            ),
            (
                b"hugeFramesSent".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 24)),
            ),
            (
                b"totalPacketSendDelay".to_vec(),
                RtcStatsValue::Double(Some(25.25)),
            ),
            (
                b"qualityLimitationReason".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 26])),
            ),
            (
                b"qualityLimitationDurations".to_vec(),
                RtcStatsValue::DoubleMap(Some(BTreeMap::from([(vec![0xff, 0, 27], 27.5)]))),
            ),
            (
                b"qualityLimitationResolutionChanges".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 28)),
            ),
            (
                b"contentType".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 29])),
            ),
            (
                b"encoderImplementation".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 30])),
            ),
            (
                b"firCount".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 31)),
            ),
            (
                b"pliCount".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 32)),
            ),
            (
                b"nackCount".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 33)),
            ),
            (
                b"qpSum".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 34)),
            ),
            (
                b"psnrSum".to_vec(),
                RtcStatsValue::DoubleMap(Some(BTreeMap::from([(vec![0xff, 0, 35], 35.5)]))),
            ),
            (
                b"psnrMeasurements".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 36)),
            ),
            (b"active".to_vec(), RtcStatsValue::Bool(Some(false))),
            (
                b"powerEfficientEncoder".to_vec(),
                RtcStatsValue::Bool(Some(true)),
            ),
            (
                b"scalabilityMode".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 39])),
            ),
            (
                b"rtxSsrc".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 40)),
            ),
            (
                b"packetsSentWithEct1".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 41)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "OutboundRtpStreamStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("OutboundRtpStreamStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(
            RtcStatsClass::RemoteInboundRtpStreamStats,
            b"RemoteInboundRtpStreamStats",
            time,
        )
        .unwrap();
        let values =
            RtcStatsValues::RemoteInboundRtpStreamStats(RtcRemoteInboundRtpStreamStatsValues {
                ssrc: Some(u32::MAX - 1),
                kind: Some(vec![0xff, 0, 2]),
                transport_id: Some(vec![0xff, 0, 3]),
                codec_id: Some(vec![0xff, 0, 4]),
                jitter: Some(5.25),
                packets_lost: Some(i32::MIN + 6),
                packets_received_with_ect1: Some(i64::MIN + 7),
                packets_received_with_ce: Some(i64::MIN + 8),
                packets_reported_as_lost: Some(i64::MIN + 9),
                packets_reported_as_lost_but_recovered: Some(i64::MIN + 10),
                local_id: Some(vec![0xff, 0, 11]),
                round_trip_time: Some(12.25),
                fraction_lost: Some(13.25),
                total_round_trip_time: Some(14.25),
                round_trip_time_measurements: Some(i32::MIN + 15),
                packets_with_bleached_ect1_marking: Some(i64::MIN + 16),
            });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (b"ssrc".to_vec(), RtcStatsValue::Uint32(Some(u32::MAX - 1))),
            (
                b"kind".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
            (
                b"transportId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"codecId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
            (b"jitter".to_vec(), RtcStatsValue::Double(Some(5.25))),
            (
                b"packetsLost".to_vec(),
                RtcStatsValue::Int32(Some(i32::MIN + 6)),
            ),
            (
                b"packetsReceivedWithEct1".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 7)),
            ),
            (
                b"packetsReceivedWithCe".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 8)),
            ),
            (
                b"packetsReportedAsLost".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 9)),
            ),
            (
                b"packetsReportedAsLostButRecovered".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 10)),
            ),
            (
                b"localId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 11])),
            ),
            (
                b"roundTripTime".to_vec(),
                RtcStatsValue::Double(Some(12.25)),
            ),
            (b"fractionLost".to_vec(), RtcStatsValue::Double(Some(13.25))),
            (
                b"totalRoundTripTime".to_vec(),
                RtcStatsValue::Double(Some(14.25)),
            ),
            (
                b"roundTripTimeMeasurements".to_vec(),
                RtcStatsValue::Int32(Some(i32::MIN + 15)),
            ),
            (
                b"packetsWithBleachedEct1Marking".to_vec(),
                RtcStatsValue::Int64(Some(i64::MIN + 16)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "RemoteInboundRtpStreamStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("RemoteInboundRtpStreamStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(
            RtcStatsClass::RemoteOutboundRtpStreamStats,
            b"RemoteOutboundRtpStreamStats",
            time,
        )
        .unwrap();
        let values =
            RtcStatsValues::RemoteOutboundRtpStreamStats(RtcRemoteOutboundRtpStreamStatsValues {
                ssrc: Some(u32::MAX - 1),
                kind: Some(vec![0xff, 0, 2]),
                transport_id: Some(vec![0xff, 0, 3]),
                codec_id: Some(vec![0xff, 0, 4]),
                packets_sent: Some(u64::MAX - 5),
                bytes_sent: Some(u64::MAX - 6),
                local_id: Some(vec![0xff, 0, 7]),
                remote_timestamp: Some(8.25),
                reports_sent: Some(u64::MAX - 9),
                round_trip_time: Some(10.25),
                round_trip_time_measurements: Some(u64::MAX - 11),
                total_round_trip_time: Some(12.25),
            });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (b"ssrc".to_vec(), RtcStatsValue::Uint32(Some(u32::MAX - 1))),
            (
                b"kind".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
            (
                b"transportId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 3])),
            ),
            (
                b"codecId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 4])),
            ),
            (
                b"packetsSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 5)),
            ),
            (
                b"bytesSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 6)),
            ),
            (
                b"localId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 7])),
            ),
            (
                b"remoteTimestamp".to_vec(),
                RtcStatsValue::Double(Some(8.25)),
            ),
            (
                b"reportsSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 9)),
            ),
            (
                b"roundTripTime".to_vec(),
                RtcStatsValue::Double(Some(10.25)),
            ),
            (
                b"roundTripTimeMeasurements".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 11)),
            ),
            (
                b"totalRoundTripTime".to_vec(),
                RtcStatsValue::Double(Some(12.25)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "RemoteOutboundRtpStreamStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("RemoteOutboundRtpStreamStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat = RtcStats::new(RtcStatsClass::AudioSourceStats, b"MediaSourceStats", time)
            .unwrap()
            .copy_as(RtcStatsClass::MediaSourceStats)
            .unwrap();
        let values = RtcStatsValues::MediaSourceStats(RtcMediaSourceStatsValues {
            track_identifier: Some(vec![0xff, 0, 1]),
            kind: Some(vec![0xff, 0, 2]),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"trackIdentifier".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 1])),
            ),
            (
                b"kind".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "MediaSourceStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("MediaSourceStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat =
            RtcStats::new(RtcStatsClass::AudioSourceStats, b"AudioSourceStats", time).unwrap();
        let values = RtcStatsValues::AudioSourceStats(RtcAudioSourceStatsValues {
            track_identifier: Some(vec![0xff, 0, 1]),
            kind: Some(vec![0xff, 0, 2]),
            audio_level: Some(3.25),
            total_audio_energy: Some(4.25),
            total_samples_duration: Some(5.25),
            echo_return_loss: Some(6.25),
            echo_return_loss_enhancement: Some(7.25),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"trackIdentifier".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 1])),
            ),
            (
                b"kind".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
            (b"audioLevel".to_vec(), RtcStatsValue::Double(Some(3.25))),
            (
                b"totalAudioEnergy".to_vec(),
                RtcStatsValue::Double(Some(4.25)),
            ),
            (
                b"totalSamplesDuration".to_vec(),
                RtcStatsValue::Double(Some(5.25)),
            ),
            (
                b"echoReturnLoss".to_vec(),
                RtcStatsValue::Double(Some(6.25)),
            ),
            (
                b"echoReturnLossEnhancement".to_vec(),
                RtcStatsValue::Double(Some(7.25)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "AudioSourceStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("AudioSourceStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat =
            RtcStats::new(RtcStatsClass::VideoSourceStats, b"VideoSourceStats", time).unwrap();
        let values = RtcStatsValues::VideoSourceStats(RtcVideoSourceStatsValues {
            track_identifier: Some(vec![0xff, 0, 1]),
            kind: Some(vec![0xff, 0, 2]),
            width: Some(u32::MAX - 3),
            height: Some(u32::MAX - 4),
            frames: Some(u32::MAX - 5),
            frames_per_second: Some(6.25),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"trackIdentifier".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 1])),
            ),
            (
                b"kind".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 2])),
            ),
            (b"width".to_vec(), RtcStatsValue::Uint32(Some(u32::MAX - 3))),
            (
                b"height".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 4)),
            ),
            (
                b"frames".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 5)),
            ),
            (
                b"framesPerSecond".to_vec(),
                RtcStatsValue::Double(Some(6.25)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "VideoSourceStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("VideoSourceStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat =
            RtcStats::new(RtcStatsClass::TransportStats, b"TransportStats", time).unwrap();
        let values = RtcStatsValues::TransportStats(RtcTransportStatsValues {
            bytes_sent: Some(u64::MAX - 1),
            packets_sent: Some(u64::MAX - 2),
            bytes_received: Some(u64::MAX - 3),
            packets_received: Some(u64::MAX - 4),
            rtcp_transport_stats_id: Some(vec![0xff, 0, 5]),
            dtls_state: Some(vec![0xff, 0, 6]),
            selected_candidate_pair_id: Some(vec![0xff, 0, 7]),
            local_certificate_id: Some(vec![0xff, 0, 8]),
            remote_certificate_id: Some(vec![0xff, 0, 9]),
            tls_version: Some(vec![0xff, 0, 10]),
            dtls_cipher: Some(vec![0xff, 0, 11]),
            dtls_role: Some(vec![0xff, 0, 12]),
            srtp_cipher: Some(vec![0xff, 0, 13]),
            selected_candidate_pair_changes: Some(u32::MAX - 14),
            ice_role: Some(vec![0xff, 0, 15]),
            ice_local_username_fragment: Some(vec![0xff, 0, 16]),
            ice_state: Some(vec![0xff, 0, 17]),
            ccfb_messages_received: Some(i32::MIN + 18),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"bytesSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 1)),
            ),
            (
                b"packetsSent".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 2)),
            ),
            (
                b"bytesReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 3)),
            ),
            (
                b"packetsReceived".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 4)),
            ),
            (
                b"rtcpTransportStatsId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 5])),
            ),
            (
                b"dtlsState".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 6])),
            ),
            (
                b"selectedCandidatePairId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 7])),
            ),
            (
                b"localCertificateId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 8])),
            ),
            (
                b"remoteCertificateId".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 9])),
            ),
            (
                b"tlsVersion".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 10])),
            ),
            (
                b"dtlsCipher".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 11])),
            ),
            (
                b"dtlsRole".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 12])),
            ),
            (
                b"srtpCipher".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 13])),
            ),
            (
                b"selectedCandidatePairChanges".to_vec(),
                RtcStatsValue::Uint32(Some(u32::MAX - 14)),
            ),
            (
                b"iceRole".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 15])),
            ),
            (
                b"iceLocalUsernameFragment".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 16])),
            ),
            (
                b"iceState".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 17])),
            ),
            (
                b"ccfbMessagesReceived".to_vec(),
                RtcStatsValue::Int32(Some(i32::MIN + 18)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "TransportStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("TransportStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
    {
        let mut stat =
            RtcStats::new(RtcStatsClass::AudioPlayoutStats, b"AudioPlayoutStats", time).unwrap();
        let values = RtcStatsValues::AudioPlayoutStats(RtcAudioPlayoutStatsValues {
            kind: Some(vec![0xff, 0, 1]),
            synthesized_samples_duration: Some(2.25),
            synthesized_samples_events: Some(u64::MAX - 3),
            total_samples_duration: Some(4.25),
            total_playout_delay: Some(5.25),
            total_samples_count: Some(u64::MAX - 6),
        });
        assert!(stat.set_values(values.clone()));
        assert_eq!(stat.values(), values);
        let actual: Vec<_> = stat
            .attributes()
            .into_iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        let mut expected = vec![
            (
                b"kind".to_vec(),
                RtcStatsValue::String(Some(vec![0xff, 0, 1])),
            ),
            (
                b"synthesizedSamplesDuration".to_vec(),
                RtcStatsValue::Double(Some(2.25)),
            ),
            (
                b"synthesizedSamplesEvents".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 3)),
            ),
            (
                b"totalSamplesDuration".to_vec(),
                RtcStatsValue::Double(Some(4.25)),
            ),
            (
                b"totalPlayoutDelay".to_vec(),
                RtcStatsValue::Double(Some(5.25)),
            ),
            (
                b"totalSamplesCount".to_vec(),
                RtcStatsValue::Uint64(Some(u64::MAX - 6)),
            ),
        ];
        assert_eq!(
            actual.len(),
            expected.len(),
            "AudioPlayoutStats native attribute coverage"
        );
        for value in actual {
            let index = expected
                .iter()
                .position(|v| v == &value)
                .expect("AudioPlayoutStats native field value");
            expected.remove(index);
        }
        assert!(expected.is_empty());
    }
}
