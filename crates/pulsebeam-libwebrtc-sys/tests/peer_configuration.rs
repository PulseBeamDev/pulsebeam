use pulsebeam_webrtc_sys::*;

fn factory() -> PeerConnectionFactory {
    PeerConnectionFactory::builder().build().unwrap()
}

#[test]
fn native_presets_and_compatibility_default_are_distinct() {
    let safe = RtcConfiguration::native_default();
    assert!(!safe.always_negotiate_data_channels);
    assert_eq!(safe.options.bundle_policy, BundlePolicy::Balanced);
    assert_eq!(safe.options.sdp_semantics, SdpSemantics::UnifiedPlan);
    assert_eq!(safe.options.ice_connection_receiving_timeout, None);
    let aggressive = RtcConfiguration::aggressive();
    assert_eq!(aggressive.options.bundle_policy, BundlePolicy::MaxBundle);
    assert_eq!(
        aggressive.options.ice_connection_receiving_timeout,
        Some(1000)
    );
    let mut pruning = safe.options.clone();
    pruning.turn_port_prune_policy = TurnPortPrunePolicy::KeepFirstReady;
    assert_eq!(
        pruning.effective_turn_port_prune_policy(),
        TurnPortPrunePolicy::KeepFirstReady
    );
    pruning.prune_turn_ports = true;
    assert_eq!(
        pruning.effective_turn_port_prune_policy(),
        TurnPortPrunePolicy::BasedOnPriority
    );
    assert!(aggressive.options.enable_ice_renomination);
    assert!(!aggressive.options.redetermine_role_on_ice_restart);
    assert_eq!(
        RtcConfiguration::from(PeerConfiguration::default()),
        RtcConfiguration {
            always_negotiate_data_channels: true,
            ..safe
        }
    );
}

#[test]
fn owned_native_configuration_roundtrips_application_values() {
    let mut config = RtcConfiguration::aggressive();
    let v = &mut config.options;
    v.disable_ipv6_on_wifi = true;
    v.max_ipv6_networks = 9;
    v.disable_link_local_networks = true;
    v.screencast_min_bitrate = Some(42000);
    v.tcp_candidate_policy = TcpCandidatePolicy::Disabled;
    v.candidate_network_policy = CandidateNetworkPolicy::LowCost;
    v.audio_jitter_buffer_max_packets = 70;
    v.audio_jitter_buffer_fast_accelerate = true;
    v.audio_jitter_buffer_min_delay_ms = 19;
    v.ice_connection_receiving_timeout = Some(5000);
    v.ice_backup_candidate_pair_ping_interval = Some(7000);
    v.continual_gathering_policy = ContinualGatheringPolicy::Continually;
    v.prioritize_most_likely_ice_candidate_pairs = true;
    v.prune_turn_ports = true;
    v.turn_port_prune_policy = TurnPortPrunePolicy::KeepFirstReady;
    v.presume_writable_when_fully_relayed = true;
    v.surface_ice_candidates_on_ice_transport_type_changed = true;
    v.ice_check_interval_strong_connectivity = Some(1200);
    v.ice_check_interval_weak_connectivity = Some(800);
    v.ice_check_min_interval = Some(400);
    v.ice_unwritable_timeout = Some(1000);
    v.ice_unwritable_min_checks = Some(3);
    v.ice_inactive_timeout = Some(9000);
    v.stun_candidate_keepalive_interval = Some(6000);
    v.stable_writable_connection_ping_interval_ms = Some(10000);
    v.report_usage_pattern_delay_ms = Some(17000);
    v.network_preference = Some(NetworkAdapterMask::WIFI);
    v.crypto_options = CryptoOptions::prefer_gcm();
    v.offer_extmap_allow_mixed = false;
    v.turn_logging_id = "native-roundtrip".into();
    v.enable_implicit_rollback = true;
    v.vpn_preference = VpnPreference::AvoidVpn;
    v.vpn_list = vec![NetworkMask {
        address: "2001:db8::1".parse().unwrap(),
        prefix_length: 48,
    }];
    v.port_allocator = PortAllocatorOptions {
        min_port: 35000,
        max_port: 36000,
        flags: 0,
    };
    v.pacer_burst_interval = Some(NativeTimeDelta::from_microseconds(12500));
    v.enable_any_address_ports = true;
    v.max_sctp_streams = 123;
    v.enable_sctp_snap = true;
    v.dscp = false;
    v.stats_timestamp_with_environment_clock = true;
    v.cpu_adaptation = false;
    v.suspend_below_min_bitrate = true;
    v.prerenderer_smoothing = false;
    v.experiment_cpu_load_estimator = true;
    v.audio_rtcp_report_interval_ms = 7000;
    v.video_rtcp_report_interval_ms = 1100;
    config.ice_transport_policy = IceTransportPolicy::NoHost;
    config.ice_servers = vec![RtcIceServer {
        urls: vec!["turns:127.0.0.1:5349".into(), "turn:127.0.0.1:3478".into()],
        username: "user".into(),
        password: "sensitive".into(),
        hostname: "relay.example.test".into(),
        tls_certificate_policy: TlsCertificatePolicy::InsecureNoCheck,
        tls_alpn_protocols: vec!["stun.turn".into()],
        tls_elliptic_curves: vec!["P-256".into()],
        ..Default::default()
    }];
    assert!(!format!("{config:?}").contains("sensitive"));
    let factory = factory();
    let peer = factory
        .create_peer_connection_with_configuration(config.clone())
        .unwrap();
    let actual = peer.configuration();
    assert_eq!(actual, config);
    assert_eq!(actual.ice_servers.len(), 1);
    assert_eq!(actual.ice_servers[0].urls.len(), 2);
    peer.set_configuration(actual).unwrap();
}

#[test]
fn updates_report_native_errors_and_preserve_effective_state() {
    let factory = factory();
    let peer = factory
        .create_peer_connection_with_configuration(RtcConfiguration::default())
        .unwrap();
    let original = peer.configuration();
    let mut changed = original.clone();
    changed.ice_transport_policy = IceTransportPolicy::None;
    changed.options.turn_logging_id = "updated".into();
    changed.options.crypto_options = CryptoOptions::prefer_gcm();
    peer.set_configuration(changed.clone()).unwrap();
    assert_eq!(peer.configuration(), changed);
    let mut unsupported = changed.clone();
    unsupported.options.bundle_policy = BundlePolicy::MaxCompat;
    assert_eq!(
        peer.set_configuration(unsupported).unwrap_err().kind,
        PeerErrorKind::InvalidModification
    );
    let mut invalid = changed.clone();
    invalid.options.ice_check_interval_strong_connectivity = Some(1);
    invalid.options.ice_check_interval_weak_connectivity = Some(100);
    assert_eq!(
        peer.set_configuration(invalid.clone()).unwrap_err().kind,
        PeerErrorKind::InvalidParameter
    );
    assert_eq!(peer.configuration(), changed);
    assert_eq!(
        factory
            .create_peer_connection_with_configuration(invalid)
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::InvalidParameter
    );
    for pool in [-1, 65536] {
        assert_eq!(
            factory
                .create_peer_connection_with_configuration(RtcConfiguration {
                    ice_candidate_pool_size: pool,
                    ..Default::default()
                })
                .err()
                .unwrap()
                .kind,
            PeerErrorKind::InvalidRange
        );
    }
    let mut invalid_url = changed.clone();
    invalid_url.ice_servers.push(RtcIceServer {
        urls: vec!["http://invalid.test".into()],
        ..Default::default()
    });
    assert_eq!(
        peer.set_configuration(invalid_url).unwrap_err().kind,
        PeerErrorKind::Syntax
    );
    assert_eq!(peer.configuration(), changed);
}

#[test]
fn legacy_server_uri_and_plan_b_remain_native_options() {
    let factory = factory();
    let mut config = RtcConfiguration::native_default();
    config.options.sdp_semantics = SdpSemantics::PlanB;
    config.options.rtcp_mux_policy = RtcpMuxPolicy::Negotiate;
    config.ice_servers.push(RtcIceServer {
        uri: "stun:127.0.0.1:3478".into(),
        ..Default::default()
    });
    let peer = factory
        .create_peer_connection_with_configuration(config.clone())
        .unwrap();
    assert_eq!(peer.configuration(), config);
}

#[test]
fn certificate_resources_survive_native_readback_and_actor_migration() {
    let certificate = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let mut config = RtcConfiguration::default();
    config.always_negotiate_data_channels = true;
    config.options.certificates.push(certificate.clone());
    let mut actor = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    let peer = actor
        .create_peer_with_configuration(config.clone())
        .unwrap();
    let actual = actor.peer_configuration(peer).unwrap();
    assert_eq!(actual, config);
    assert!(actual.options.certificates[0].same_resource(&certificate));
    actor = std::thread::spawn(move || {
        actor.set_peer_configuration(peer, actual).unwrap();
        assert_eq!(actor.peer_configuration(peer).unwrap(), config);
        actor
    })
    .join()
    .unwrap();
    let mut replacement = actor.peer_configuration(peer).unwrap();
    replacement.options.certificates = vec![Certificate::from_pem(&certificate.to_pem()).unwrap()];
    assert_eq!(
        actor
            .set_peer_configuration(peer, replacement)
            .unwrap_err()
            .kind,
        PeerErrorKind::InvalidModification
    );
    let operation = actor.create_offer(peer, false).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "offer completion timed out"
        );
        if let Some(SessionEvent::OperationComplete(done)) = actor.try_peer_event(peer).unwrap()
            && done.operation_id == operation
        {
            let offer = done.result.unwrap().unwrap();
            let fingerprint = certificate.fingerprint("sha-256").unwrap();
            assert!(offer.sdp.contains("m=application"));
            assert!(offer.sdp.contains(&format!(
                "a=fingerprint:{} {}",
                fingerprint.algorithm, fingerprint.value
            )));
            break;
        }
        std::thread::yield_now();
    }
    actor.close_peer(peer).unwrap();
    assert_eq!(
        actor
            .set_peer_configuration(peer, RtcConfiguration::default())
            .unwrap_err()
            .kind,
        PeerErrorKind::Closed
    );
}
