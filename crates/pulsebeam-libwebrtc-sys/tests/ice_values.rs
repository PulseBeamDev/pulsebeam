use pulsebeam_webrtc_sys::*;

#[test]
fn native_candidate_owns_all_bytes_and_preserves_its_generated_identity() {
    let mut candidate = TransportCandidate::default();
    candidate.set_component(1);
    candidate.set_protocol(b"udp");
    candidate.set_relay_protocol(b"relay\0\xff");
    candidate.set_username(b"ufrag\0\xff");
    candidate.set_password(b"password\0\x80");
    candidate.set_address(&SocketAddress::new(std::net::Ipv4Addr::LOCALHOST, 43210));
    candidate.set_related_address(&SocketAddress::from_host(b"related\xff\0", 5000));
    candidate.set_priority(u32::MAX);
    candidate.set_candidate_type(TransportCandidateType::Relay);
    candidate.set_network_type(NetworkAdapterType::Vpn);
    candidate.set_underlying_type_for_vpn(NetworkAdapterType::Wifi);
    candidate.set_network_name(b"network\0\xff");
    candidate.set_generation(u32::MAX);
    candidate.set_foundation(b"foundation\0\xff");
    candidate.set_tcptype(b"active");
    candidate.set_network_id(u16::MAX);
    candidate
        .set_network_cost(MAX_ICE_CANDIDATE_NETWORK_COST)
        .unwrap();
    candidate.set_url(b"turn:example\0\xff");
    candidate.set_network_slice(NetworkSlice::UnifiedCommunications);
    let value = candidate.properties();
    assert_eq!(value.protocol, b"udp");
    assert_eq!(value.relay_protocol, b"relay\0\xff");
    assert_eq!(value.username, b"ufrag\0\xff");
    assert_eq!(value.password, b"password\0\x80");
    assert_eq!(value.network_name, b"network\0\xff");
    assert_eq!(value.foundation, b"foundation\0\xff");
    assert_eq!(value.url, b"turn:example\0\xff");
    assert_eq!(value.related_address.hostname(), b"related\xff\0");
    assert_eq!(value.priority, u32::MAX);
    assert_eq!(value.generation, u32::MAX);
    assert_eq!(value.network_id, u16::MAX);
    assert_eq!(value.network_cost, 999);
    assert_eq!(value.network_slice, NetworkSlice::UnifiedCommunications);
    let clone = candidate.clone();
    assert_eq!(clone.properties().id, value.id);
    assert!(candidate.is_equivalent(&clone));
    assert!(candidate.matches_for_removal(&clone));
    candidate.verify().unwrap();
    assert!(candidate.set_network_cost(1000).is_err());
    assert_eq!(candidate.properties().network_cost, 999);
    let serialized = candidate.to_candidate_attribute(true);
    assert!(serialized.starts_with(b"candidate:"));
    assert!(!candidate.to_native_string().is_empty());
    assert!(!candidate.to_sensitive_string().is_empty());
    let error = TransportCandidate::parse(b"not a candidate").unwrap_err();
    assert!(!error.is_ok());
    assert!(!error.message_bytes().is_empty());
    let parsed = TransportCandidate::parse(
        b"candidate:typed 1 udp 2122260223 127.0.0.1 43210 typ host generation 0",
    )
    .unwrap();
    assert_eq!(parsed.properties().foundation, b"typed");
    assert_eq!(parsed.properties().address.port(), 43210);
}

#[test]
fn native_ice_defaults_validation_and_packet_defaults_are_not_normalized() {
    let mut config = IceConfig::default();
    assert!(config.receiving_timeout.is_none());
    assert!(config.ice_unwritable_min_checks.is_none());
    assert!(!config.gather_continually());
    config.validate().unwrap();
    let resolved = config.resolved_defaults();
    assert!(resolved.receiving_timeout.is_some());
    assert!(resolved.ice_unwritable_min_checks.is_some());
    assert!(config.receiving_timeout.is_none());
    config.receiving_timeout = Some(TimeDelta::from_native_microsecond_bits(0));
    let error = config.validate().unwrap_err();
    assert_eq!(error.kind(), RtcErrorType::InvalidParameter);
    assert!(!error.message_bytes().is_empty());
    let options = IcePacketOptions::default();
    assert_eq!(options.packet_id, -1);
    assert_eq!(options.dscp, Dscp::NO_CHANGE);
    assert_eq!(options.packet_time.rtp_sendtime_extension_id, -1);
    assert_eq!(options.packet_time.srtp_auth_tag_len, -1);
    assert_eq!(options.packet_time.srtp_packet_index, -1);
    assert!(options.info_signaled_after_sent.network_id.is_none());
    assert!(!options.batchable);
    assert!(Dscp::new(63).is_some());
    assert!(Dscp::new(64).is_none());
}
