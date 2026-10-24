use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    thread,
};

#[path = "support/non_trickle.rs"]
mod non_trickle;

use pulsebeam_webrtc_sys::{
    ConnectionState, DataChannelConfiguration, DataChannelEvent, DataChannelMessage,
    DataChannelSendResult, DataChannelState, Environment, IceGatheringState, IceServer,
    IceTransportPolicy, ManualClock, OperationCompletion, OperationId, PeerConfiguration,
    PeerConnection, PeerConnectionEvent, PeerConnectionFactory, PeerErrorKind, PeerStatsRecord,
    SessionDescription, SimulatedNetwork,
};

fn factory(
    environment: Environment,
    endpoint: &pulsebeam_webrtc_sys::NetworkEndpoint,
) -> PeerConnectionFactory {
    PeerConnectionFactory::builder()
        .environment(environment)
        .network_manager(endpoint.network_manager().unwrap())
        .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
        .build()
        .unwrap()
}

fn drain_until_completion(
    peer: &PeerConnection,
    operation_id: OperationId,
    side_events: &mut Vec<PeerConnectionEvent>,
) -> OperationCompletion {
    for _ in 0..1_000_000 {
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::OperationComplete(completion) = &event
                && completion.operation_id == operation_id
            {
                return completion.clone();
            }
            side_events.push(event);
        }
        thread::yield_now();
    }
    panic!("operation {} did not complete", operation_id.get());
}

fn require_success(
    peer: &PeerConnection,
    operation_id: OperationId,
    events: &mut Vec<PeerConnectionEvent>,
) -> Option<SessionDescription> {
    drain_until_completion(peer, operation_id, events)
        .result
        .unwrap()
}

fn connected(event: PeerConnectionEvent) -> bool {
    matches!(
        event,
        PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected)
    )
}

#[test]
fn two_peers_negotiate_gathered_sdp_without_trickled_candidates() {
    let clock = ManualClock::new(std::time::Duration::from_secs(1)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let network = SimulatedNetwork::new(&clock).unwrap();
    let alice_endpoint = network
        .register_endpoint(Ipv4Addr::new(10, 20, 0, 1).into())
        .unwrap();
    let bob_endpoint = network
        .register_endpoint(Ipv4Addr::new(10, 20, 0, 2).into())
        .unwrap();
    let alice_factory = factory(environment.clone(), &alice_endpoint);
    let bob_factory = factory(environment, &bob_endpoint);
    let mut alice = alice_factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let mut bob = bob_factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    drop(alice_factory);
    drop(bob_factory);

    let channel_config = DataChannelConfiguration {
        negotiated: true,
        id: Some(2),
        ..Default::default()
    };
    let alice_channel = alice
        .create_data_channel("restart-probe", channel_config.clone())
        .unwrap();
    let bob_channel = bob
        .create_data_channel("restart-probe", channel_config)
        .unwrap();

    let mut alice_events = Vec::new();
    let mut bob_events = Vec::new();
    let offer_id = alice.create_offer().unwrap();
    let offer = require_success(&alice, offer_id, &mut alice_events).unwrap();
    assert!(offer.sdp.contains("m=application"));
    let set_local_offer = alice.set_local_description(offer).unwrap();
    require_success(&alice, set_local_offer, &mut alice_events);
    let gathered_offer =
        non_trickle::gathered_local_description(&alice, &clock, &network, &mut alice_events);
    assert!(alice.descriptions().unwrap().current_local.is_none());
    let set_remote_offer = bob.set_remote_description(gathered_offer).unwrap();
    require_success(&bob, set_remote_offer, &mut bob_events);
    assert!(bob.descriptions().unwrap().pending_remote.is_some());
    assert!(bob.descriptions().unwrap().current_remote.is_none());

    let answer_id = bob.create_answer().unwrap();
    let answer = require_success(&bob, answer_id, &mut bob_events).unwrap();
    let set_local_answer = bob.set_local_description(answer).unwrap();
    require_success(&bob, set_local_answer, &mut bob_events);
    let gathered_answer =
        non_trickle::gathered_local_description(&bob, &clock, &network, &mut bob_events);
    let set_remote_answer = alice.set_remote_description(gathered_answer).unwrap();
    require_success(&alice, set_remote_answer, &mut alice_events);
    for peer in [&alice, &bob] {
        let descriptions = peer.descriptions().unwrap();
        assert!(descriptions.current_local.is_some());
        assert!(descriptions.current_remote.is_some());
        assert!(descriptions.pending_local.is_none());
        assert!(descriptions.pending_remote.is_none());
    }

    let mut alice_connected = alice_events.drain(..).any(connected);
    let mut bob_connected = bob_events.drain(..).any(connected);
    let mut alice_observed = Vec::new();
    let mut bob_observed = Vec::new();
    let mut delivered_packets = 0;
    for _ in 0..2_000_000 {
        while let Some(event) = alice.try_next_event() {
            alice_observed.push(format!("{event:?}"));
            alice_connected |= connected(event);
        }
        while let Some(event) = bob.try_next_event() {
            bob_observed.push(format!("{event:?}"));
            bob_connected |= connected(event);
        }
        while let Some(packet) = network.next_packet() {
            network.deliver(packet.id).unwrap();
            delivered_packets += 1;
        }
        if alice_connected
            && bob_connected
            && alice_channel.state() == DataChannelState::Open
            && bob_channel.state() == DataChannelState::Open
        {
            break;
        }
        clock.advance(std::time::Duration::from_millis(1)).unwrap();
        thread::yield_now();
    }
    assert!(
        alice_connected && bob_connected,
        "delivered {delivered_packets} packets\nalice events: {alice_observed:#?}\nbob events: {bob_observed:#?}"
    );

    let stats_id = alice.request_stats().unwrap();
    assert_eq!(
        alice.request_stats().unwrap_err().kind,
        PeerErrorKind::InvalidState
    );
    let mut snapshot = None;
    for _ in 0..1_000_000 {
        if let Some(event) = alice.try_next_event() {
            match event {
                PeerConnectionEvent::Stats(stats) if stats.operation_id == stats_id => {
                    snapshot = Some(stats);
                    break;
                }
                PeerConnectionEvent::OperationComplete(completion)
                    if completion.operation_id == stats_id =>
                {
                    panic!("stats failed: {:?}", completion.result);
                }
                _ => {}
            }
        }
        thread::yield_now();
    }
    let snapshot = snapshot.expect("stats snapshot did not arrive");
    assert!(snapshot.records.len() <= 256);
    let pairs: Vec<_> = snapshot
        .records
        .iter()
        .filter_map(|record| {
            if let PeerStatsRecord::CandidatePair(pair) = record {
                Some(pair.id.as_slice())
            } else {
                None
            }
        })
        .collect();
    assert!(snapshot.records.iter().any(|record| {
        matches!(record, PeerStatsRecord::Transport(transport)
            if transport.selected_candidate_pair_id.as_ref().is_some_and(|id| pairs.contains(&id.as_slice())))
    }));
    assert!(
        alice.request_stats().is_ok(),
        "snapshot consumption frees the capacity"
    );

    let previous_ufrag = alice
        .descriptions()
        .unwrap()
        .current_local
        .unwrap()
        .sdp
        .lines()
        .find_map(|line| line.strip_prefix("a=ice-ufrag:"))
        .unwrap()
        .to_owned();
    let restart = alice.create_ice_restart_offer().unwrap();
    let offer = require_success(&alice, restart, &mut alice_events).unwrap();
    let new_ufrag = offer
        .sdp
        .lines()
        .find_map(|line| line.strip_prefix("a=ice-ufrag:"))
        .unwrap();
    assert_ne!(new_ufrag, previous_ufrag);
    let set_local = alice.set_local_description(offer).unwrap();
    require_success(&alice, set_local, &mut alice_events);
    let gathered_offer =
        non_trickle::gathered_local_description(&alice, &clock, &network, &mut alice_events);
    let set_remote = bob.set_remote_description(gathered_offer).unwrap();
    require_success(&bob, set_remote, &mut bob_events);
    let answer = require_success(&bob, bob.create_answer().unwrap(), &mut bob_events).unwrap();
    let set_local = bob.set_local_description(answer).unwrap();
    require_success(&bob, set_local, &mut bob_events);
    let gathered_answer =
        non_trickle::gathered_local_description(&bob, &clock, &network, &mut bob_events);
    let set_remote = alice.set_remote_description(gathered_answer).unwrap();
    require_success(&alice, set_remote, &mut alice_events);
    // An ICE restart may keep the aggregate PeerConnection state Connected.
    // Upstream suppresses duplicate state notifications, so require actual
    // bidirectional application delivery after exchanging the new credentials.
    let from_alice = DataChannelMessage::binary(b"alice-after-restart".to_vec());
    let from_bob = DataChannelMessage::binary(b"bob-after-restart".to_vec());
    assert_eq!(
        alice_channel.send(from_alice.clone()),
        DataChannelSendResult::Sent
    );
    assert_eq!(
        bob_channel.send(from_bob.clone()),
        DataChannelSendResult::Sent
    );
    let mut alice_received = false;
    let mut bob_received = false;
    let mut restart_packets = 0;
    for _ in 0..2_000_000 {
        while let Some(event) = alice_channel.try_next_event() {
            if let DataChannelEvent::Message(message) = event {
                assert_eq!(message, from_bob);
                alice_received = true;
            }
        }
        while let Some(event) = bob_channel.try_next_event() {
            if let DataChannelEvent::Message(message) = event {
                assert_eq!(message, from_alice);
                bob_received = true;
            }
        }
        while let Some(packet) = network.next_packet() {
            network.deliver(packet.id).unwrap();
            restart_packets += 1;
        }
        if alice_received && bob_received {
            break;
        }
        clock.advance(std::time::Duration::from_millis(1)).unwrap();
        thread::yield_now();
    }
    assert!(
        alice_received && bob_received && restart_packets > 0,
        "ICE restart did not deliver bidirectionally: packets={restart_packets}, alice={alice_received}, bob={bob_received}"
    );

    alice.close().unwrap();
    bob.close().unwrap();
    alice.close().unwrap();
}

#[test]
fn rollback_restores_stable_descriptions_and_rejects_nonempty_sdp() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let mut events = Vec::new();
    assert_eq!(peer.descriptions().unwrap(), Default::default());
    let offer_id = peer.create_offer().unwrap();
    let offer = require_success(&peer, offer_id, &mut events).unwrap();
    let local_id = peer.set_local_description(offer).unwrap();
    require_success(&peer, local_id, &mut events);
    assert!(peer.descriptions().unwrap().pending_local.is_some());
    let rollback = peer
        .set_local_description(SessionDescription {
            kind: pulsebeam_webrtc_sys::SessionDescriptionType::Rollback,
            sdp: String::new(),
        })
        .unwrap();
    require_success(&peer, rollback, &mut events);
    assert_eq!(peer.descriptions().unwrap(), Default::default());
    let invalid = peer
        .set_local_description(SessionDescription {
            kind: pulsebeam_webrtc_sys::SessionDescriptionType::Rollback,
            sdp: "unexpected SDP".into(),
        })
        .unwrap();
    let error = drain_until_completion(&peer, invalid, &mut events)
        .result
        .unwrap_err();
    assert_eq!(error.kind, PeerErrorKind::Syntax);
    peer.close().unwrap();
    assert_eq!(peer.descriptions().unwrap_err().kind, PeerErrorKind::Closed);
}

fn gather_from_simulated_stun(client_ip: IpAddr, server_ip: IpAddr, mapped_ip: IpAddr) {
    let clock = ManualClock::new(std::time::Duration::from_secs(1)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let network = SimulatedNetwork::new(&clock).unwrap();
    let endpoint = network.register_endpoint(client_ip).unwrap();
    let server = network.register_endpoint(server_ip).unwrap();
    let server_socket = server.bind_udp(3478).unwrap();
    let factory = factory(environment, &endpoint);
    let url = match server_ip {
        IpAddr::V4(_) => format!("stun:{server_ip}:3478"),
        IpAddr::V6(_) => format!("stun:[{server_ip}]:3478"),
    };
    let mut peer = factory
        .create_peer_connection(PeerConfiguration {
            ice_servers: vec![IceServer {
                urls: vec![url],
                username: String::new(),
                password: String::new(),
            }],
            ..Default::default()
        })
        .unwrap();
    let mut events = Vec::new();
    let offer_id = peer.create_offer().unwrap();
    let offer = require_success(&peer, offer_id, &mut events).unwrap();
    let local_id = peer.set_local_description(offer).unwrap();
    require_success(&peer, local_id, &mut events);

    let mut responded = false;
    let mut gathered = false;
    for _ in 0..10_000 {
        for event in events
            .drain(..)
            .chain(std::iter::from_fn(|| peer.try_next_event()))
        {
            gathered |= matches!(
                event,
                PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
            );
        }
        while let Some(packet) = network.next_packet() {
            if packet.destination == server_socket.local_address() {
                network.deliver(packet.id).unwrap();
                while let Some(request) = server_socket.try_receive() {
                    let body = &request.payload;
                    assert!(body.len() >= 20 && body[0..2] == [0, 1]);
                    assert_eq!(&body[4..8], &[0x21, 0x12, 0xa4, 0x42]);
                    // RFC 5389 Binding Success with XOR-MAPPED-ADDRESS, using a
                    // different public mapping to distinguish srflx from host.
                    let (family, ip_octets): (u8, Vec<u8>) = match mapped_ip {
                        IpAddr::V4(ip) => (1, ip.octets().to_vec()),
                        IpAddr::V6(ip) => (2, ip.octets().to_vec()),
                    };
                    let attribute_size = ip_octets.len() + 4;
                    let mut response = vec![0x01, 0x01, 0, (attribute_size + 4) as u8];
                    response.extend_from_slice(&body[4..20]);
                    response.extend_from_slice(&[0, 0x20, 0, attribute_size as u8, 0, family]);
                    response.extend_from_slice(&(45678_u16 ^ 0x2112).to_be_bytes());
                    let key = &body[4..20];
                    response.extend(ip_octets.iter().zip(key).map(|(ip, mask)| ip ^ mask));
                    server_socket.send_to(request.source, response).unwrap();
                    responded = true;
                }
            } else {
                network.deliver(packet.id).unwrap();
            }
        }
        if responded && gathered {
            break;
        }
        clock.advance(std::time::Duration::from_millis(1)).unwrap();
        // ICE allocator phases use RTC worker-thread timers outside the
        // simulated packet clock.
        thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(responded, "no STUN Binding Request reached the fixture");
    assert!(
        gathered,
        "ICE gathering did not complete after the Binding Success"
    );
    assert!(
        peer.descriptions()
            .unwrap()
            .pending_local
            .unwrap()
            .sdp
            .contains(" typ srflx "),
        "non-trickle SDP did not include the server-reflexive candidate"
    );
    peer.close().unwrap();
}

#[test]
fn simulated_stun_ipv4_gathers_server_reflexive_candidate() {
    gather_from_simulated_stun(
        Ipv4Addr::new(10, 20, 0, 1).into(),
        Ipv4Addr::new(10, 20, 0, 3).into(),
        Ipv4Addr::new(198, 51, 100, 40).into(),
    );
}

#[test]
fn simulated_stun_ipv6_gathers_server_reflexive_candidate() {
    gather_from_simulated_stun(
        "2001:db8::1".parse::<Ipv6Addr>().unwrap().into(),
        "2001:db8::3".parse::<Ipv6Addr>().unwrap().into(),
        "2001:db8:1::40".parse::<Ipv6Addr>().unwrap().into(),
    );
}

#[test]
fn invalid_inputs_closed_operations_and_construction_failures_are_explicit() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    assert!(
        factory
            .create_peer_connection_with_configuration(pulsebeam_webrtc_sys::RtcConfiguration {
                ice_candidate_pool_size: 65536,
                ..Default::default()
            })
            .is_err()
    );

    let mut peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let mut events = Vec::new();
    let invalid_sdp = peer
        .set_remote_description(SessionDescription {
            kind: pulsebeam_webrtc_sys::SessionDescriptionType::Offer,
            sdp: "not SDP".into(),
        })
        .unwrap();
    let error = drain_until_completion(&peer, invalid_sdp, &mut events)
        .result
        .unwrap_err();
    assert_eq!(error.kind, PeerErrorKind::Syntax);

    peer.close().unwrap();
    peer.close().unwrap();
    assert_eq!(peer.create_offer().unwrap_err().kind, PeerErrorKind::Closed);

    while peer.try_next_event().is_some() {}
    for _ in 0..10_000 {
        thread::yield_now();
    }
    assert!(peer.try_next_event().is_none());
}

#[test]
fn ice_server_policy_and_restart_offer_are_explicit() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    for urls in [
        vec![],
        vec!["http://invalid.example".into()],
        vec!["turn:".into()],
    ] {
        let error = factory
            .create_peer_connection(PeerConfiguration {
                ice_servers: vec![IceServer {
                    urls,
                    username: "user".into(),
                    password: "secret".into(),
                }],
                ..Default::default()
            })
            .err()
            .expect("invalid URL should fail");
        assert_eq!(error.kind, PeerErrorKind::Syntax);
    }
    let error = factory
        .create_peer_connection(PeerConfiguration {
            ice_servers: vec![IceServer {
                urls: vec!["turn:relay.example.test:3478?transport=udp".into()],
                username: String::new(),
                password: String::new(),
            }],
            ..Default::default()
        })
        .err()
        .expect("TURN requires credentials");
    assert_eq!(error.kind, PeerErrorKind::InvalidParameter);
    for invalid_ca in ["", "not a PEM certificate"] {
        let error = factory
            .create_peer_connection(PeerConfiguration {
                turn_tls_ca_pem: Some(invalid_ca.into()),
                ..Default::default()
            })
            .err()
            .expect("invalid TURN CA should fail");
        assert_eq!(error.kind, PeerErrorKind::InvalidParameter);
    }

    let config = PeerConfiguration {
        ice_servers: vec![
            IceServer {
                urls: vec!["stun:stun.example.test:3478".into()],
                username: String::new(),
                password: String::new(),
            },
            IceServer {
                urls: vec![
                    "turn:relay.example.test:3478?transport=udp".into(),
                    "turn:relay.example.test:3478?transport=tcp".into(),
                    "turns:relay.example.test:5349?transport=tcp".into(),
                ],
                username: "user".into(),
                password: "secret".into(),
            },
        ],
        ice_transport_policy: IceTransportPolicy::RelayOnly,
        ..Default::default()
    };
    assert!(!format!("{config:?}").contains("secret"));
    let peer = factory.create_peer_connection(config).unwrap();
    let mut events = Vec::new();
    let first = require_success(&peer, peer.create_offer().unwrap(), &mut events).unwrap();
    require_success(
        &peer,
        peer.set_local_description(first).unwrap(),
        &mut events,
    );
    let restart =
        require_success(&peer, peer.create_ice_restart_offer().unwrap(), &mut events).unwrap();
    assert!(restart.sdp.contains("a=ice-ufrag:"));
}

#[test]
fn repeated_partial_lifecycles_quiesce_on_drop() {
    for _ in 0..32 {
        let factory = PeerConnectionFactory::builder().build().unwrap();
        let mut peer = factory
            .create_peer_connection(PeerConfiguration::default())
            .unwrap();
        peer.create_offer().unwrap();
        peer.close().unwrap();
        drop(factory);
        drop(peer);
    }
}
