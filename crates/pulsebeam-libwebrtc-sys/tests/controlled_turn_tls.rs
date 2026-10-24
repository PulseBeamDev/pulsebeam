//! A real TLS server driven entirely by caller-delivered simulated TCP bytes.
#[path = "support/tools.rs"]
mod tools;
#[path = "support/turnserver.rs"]
mod turnserver;
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    process::{Child, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use pulsebeam_webrtc_sys::{
    ControlledPeerDriver, ControlledSimulatedNetwork, DataChannelConfiguration, DataChannelEvent,
    DataChannelMessage, DataChannelSendResult, DataChannelState, Environment, IceGatheringState,
    IceServer, IceTransportPolicy, ManualClock, NetworkAddress, OperationId, OutboundKind,
    PeerConfiguration, PeerConnection, PeerConnectionEvent, PeerConnectionFactory,
    SessionDescription, TaskQueueFactory,
};
use rustls::{
    ServerConfig, ServerConnection,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};

#[path = "support/tls_certificate.rs"]
mod tls_certificate;
use tls_certificate::certificate;

#[test]
fn turn_tls_exchanges_handshake_and_encrypted_turn_request_via_simulator() {
    let temp =
        std::env::temp_dir().join(format!("pulsebeam-controlled-tls-{}", std::process::id()));
    fs::create_dir_all(&temp).unwrap();
    let (ca_pem, cert_der, key_der) = certificate(&temp);
    let config =
        ServerConfig::builder_with_provider(Arc::new(oxitls_rustcrypto_provider::provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert_der)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
            )
            .unwrap();
    let mut server = ServerConnection::new(Arc::new(config)).unwrap();
    let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
    let queues = TaskQueueFactory::cooperative(&clock).unwrap();
    let environment = Environment::builder()
        .task_queue_factory(&queues)
        .build()
        .unwrap();
    let driver = ControlledPeerDriver::new(&clock).unwrap();
    let network = ControlledSimulatedNetwork::new(&clock, &driver).unwrap();
    let client_ip = Ipv4Addr::new(10, 71, 0, 1).into();
    let server_ip = Ipv4Addr::new(10, 71, 0, 2).into();
    let client = network.register_endpoint(client_ip).unwrap();
    let remote = network.register_endpoint(server_ip).unwrap();
    network.add_dns_record("relay.test", server_ip).unwrap();
    let factory = PeerConnectionFactory::builder()
        .environment(environment)
        .controlled_driver(&driver)
        .network_manager(client.network_manager().unwrap())
        .packet_socket_factory(client.packet_socket_factory().unwrap())
        .build()
        .unwrap();
    let mut peer = factory
        .create_peer_connection(PeerConfiguration {
            ice_servers: vec![IceServer {
                urls: vec!["turns:relay.test:5349?transport=tcp".into()],
                username: "client".into(),
                password: "secret".into(),
            }],
            turn_tls_ca_pem: Some(ca_pem),
            ..PeerConfiguration::default()
        })
        .unwrap();
    let operation = peer.create_offer().unwrap();
    let mut offer = None;
    for _ in 0..1_000 {
        driver.run_ready();
        queues.run_ready();
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::OperationComplete(result) = event {
                if result.operation_id == operation {
                    offer = Some(result.result.unwrap().unwrap());
                }
            }
        }
        if offer.is_some() {
            break;
        }
        clock.advance(Duration::from_millis(1)).unwrap();
    }
    peer.set_local_description(offer.expect("offer not created"))
        .unwrap();

    let mut connected = false;
    let mut request = false;
    for _ in 0..10_000 {
        driver.run_ready();
        queues.run_ready();
        while let Some(packet) = network.next_packet() {
            match packet.kind {
                OutboundKind::TcpConnect => {
                    assert_eq!(packet.destination.ip(), server_ip);
                    assert_eq!(packet.destination.port(), 5349);
                    connected = true;
                    network.deliver(packet.id).unwrap();
                }
                OutboundKind::TcpData => {
                    assert!(connected);
                    server.read_tls(&mut Cursor::new(&packet.payload)).unwrap();
                    server.process_new_packets().unwrap();
                    let mut decrypted = [0u8; 1024];
                    match server.reader().read(&mut decrypted) {
                        Ok(size) if size > 0 => {
                            assert_eq!(decrypted[0] & 0b1100_0000, 0, "not a STUN request");
                            request = true;
                        }
                        Ok(_) | Err(_) => {}
                    }
                    network.drop_packet(packet.id).unwrap();
                    let mut response = Vec::new();
                    while server.wants_write() {
                        server.write_tls(&mut response).unwrap();
                    }
                    if !response.is_empty() {
                        network
                            .inject_tcp_data(packet.destination, packet.source, &response)
                            .unwrap();
                    }
                }
                OutboundKind::Udp => network.drop_packet(packet.id).unwrap(),
            }
        }
        if request {
            break;
        }
        clock.advance(Duration::from_millis(1)).unwrap();
    }
    assert!(connected, "no simulated TCP connection");
    assert!(
        request,
        "no decrypted TURN request after verified TLS handshake"
    );
    peer.close().unwrap();
    drop(peer);
    drop(factory);
    drop(remote);
    drop(client);
    drop(network);
    drop(driver);
    fs::remove_dir_all(temp).unwrap();
}

struct TurnServer(Child);

impl Drop for TurnServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn caller_delivered_turn_tls_authenticates_and_gathers_relay_candidate() {
    let temp = std::env::temp_dir().join(format!(
        "pulsebeam-controlled-coturn-{}",
        std::process::id()
    ));
    fs::create_dir_all(&temp).unwrap();
    let (ca_pem, _, _) = certificate(&temp);
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut server = TurnServer(
        turnserver::without_dtls()
            .args([
                "-n",
                "--no-cli",
                "--no-tcp",
                "--no-udp",
                "--no-multicast-peers",
                "--lt-cred-mech",
                "--realm=fixture.invalid",
                "--user=client:secret",
                "--listening-ip",
                "127.0.0.1",
                "--relay-ip",
                "127.0.0.1",
                "--tls-listening-port",
                &port.to_string(),
                "--cert",
                temp.join("server.pem").to_str().unwrap(),
                "--pkey",
                temp.join("server.key").to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let startup = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "coturn exited early"
        );
        if TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok() {
            break;
        }
        assert!(Instant::now() < startup, "coturn did not listen");
        thread::sleep(Duration::from_millis(10));
    }

    let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
    let queues = TaskQueueFactory::cooperative(&clock).unwrap();
    let environment = Environment::builder()
        .task_queue_factory(&queues)
        .build()
        .unwrap();
    let driver = ControlledPeerDriver::new(&clock).unwrap();
    let network = ControlledSimulatedNetwork::new(&clock, &driver).unwrap();
    let client = network
        .register_endpoint(Ipv4Addr::new(10, 72, 0, 1).into())
        .unwrap();
    let remote_ip = Ipv4Addr::new(10, 72, 0, 2).into();
    let remote = network.register_endpoint(remote_ip).unwrap();
    network.add_dns_record("relay.test", remote_ip).unwrap();
    let factory = PeerConnectionFactory::builder()
        .environment(environment)
        .controlled_driver(&driver)
        .network_manager(client.network_manager().unwrap())
        .packet_socket_factory(client.packet_socket_factory().unwrap())
        .build()
        .unwrap();
    let mut peer = factory
        .create_peer_connection(PeerConfiguration {
            ice_servers: vec![IceServer {
                urls: vec![format!("turns:relay.test:{port}?transport=tcp")],
                username: "client".into(),
                password: "secret".into(),
            }],
            ice_transport_policy: IceTransportPolicy::RelayOnly,
            turn_tls_ca_pem: Some(ca_pem),
            ..PeerConfiguration::default()
        })
        .unwrap();
    let offer_operation = peer.create_offer().unwrap();
    let mut offer = None;
    for _ in 0..1000 {
        driver.run_ready();
        queues.run_ready();
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::OperationComplete(result) = event {
                if result.operation_id == offer_operation {
                    offer = Some(result.result.unwrap().unwrap());
                }
            }
        }
        if offer.is_some() {
            break;
        }
        clock.advance(Duration::from_millis(1)).unwrap();
    }
    peer.set_local_description(offer.expect("offer not created"))
        .unwrap();

    let mut streams: BTreeMap<NetworkAddress, TcpStream> = BTreeMap::new();
    let mut relayed = false;
    for iteration in 0..10_000 {
        driver.run_ready();
        queues.run_ready();
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::IceCandidate(candidate) = event {
                relayed |= candidate.candidate.contains("typ relay");
            }
        }
        while let Some(packet) = network.next_packet() {
            match packet.kind {
                OutboundKind::TcpConnect => {
                    assert_eq!(packet.destination.ip(), remote_ip);
                    assert_eq!(packet.destination.port(), port);
                    let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
                    stream.set_nonblocking(true).unwrap();
                    streams.insert(packet.source, stream);
                    network.deliver(packet.id).unwrap();
                }
                OutboundKind::TcpData => {
                    let stream = streams.get_mut(&packet.source).unwrap();
                    // Small TURN/TLS records fit the local socket buffer; a
                    // partial write would require a separate bridge buffer.
                    let sent = stream.write(&packet.payload).unwrap();
                    assert_eq!(sent, packet.payload.len());
                    network.drop_packet(packet.id).unwrap();
                }
                OutboundKind::Udp => network.drop_packet(packet.id).unwrap(),
            }
        }
        for (&local, stream) in &mut streams {
            let mut buffer = [0u8; 16_384];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(size) => {
                        let source = NetworkAddress::new(remote_ip, port).unwrap();
                        network
                            .inject_tcp_data(source, local, &buffer[..size])
                            .unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("coturn bridge read: {error}"),
                }
            }
        }
        if relayed {
            break;
        }
        if iteration % 25 == 0 {
            thread::sleep(Duration::from_millis(1));
        }
        clock.advance(Duration::from_millis(1)).unwrap();
    }
    assert!(
        relayed,
        "TURN/TLS did not authenticate and gather a relay candidate"
    );
    peer.close().unwrap();
    drop(peer);
    drop(factory);
    drop(remote);
    drop(client);
    drop(network);
    drop(driver);
    drop(server);
    fs::remove_dir_all(temp).unwrap();
}

struct TurnTlsPump<'a> {
    clock: &'a ManualClock,
    queues: &'a TaskQueueFactory,
    driver: &'a ControlledPeerDriver,
    network: &'a ControlledSimulatedNetwork,
    streams: BTreeMap<NetworkAddress, TcpStream>,
    server_ip: Ipv4Addr,
    server_port: u16,
    ticks: usize,
}

impl TurnTlsPump<'_> {
    fn tick(&mut self) {
        self.driver.run_ready();
        self.queues.run_ready();
        for _ in 0..10_000 {
            let Some(packet) = self.network.next_packet() else {
                break;
            };
            match packet.kind {
                OutboundKind::TcpConnect => {
                    assert_eq!(
                        packet.destination.ip(),
                        std::net::IpAddr::V4(self.server_ip)
                    );
                    assert_eq!(packet.destination.port(), self.server_port);
                    let stream =
                        TcpStream::connect((Ipv4Addr::LOCALHOST, self.server_port)).unwrap();
                    stream.set_nonblocking(true).unwrap();
                    assert!(self.streams.insert(packet.source, stream).is_none());
                    self.network.deliver(packet.id).unwrap();
                }
                OutboundKind::TcpData => {
                    let stream = self.streams.get_mut(&packet.source).unwrap();
                    let sent = stream.write(&packet.payload).unwrap();
                    assert_eq!(sent, packet.payload.len());
                    self.network.drop_packet(packet.id).unwrap();
                }
                OutboundKind::Udp => self.network.drop_packet(packet.id).unwrap(),
            }
        }
        for (&local, stream) in &mut self.streams {
            let mut buffer = [0u8; 16_384];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(size) => {
                        self.network
                            .inject_tcp_data(
                                NetworkAddress::new(self.server_ip.into(), self.server_port)
                                    .unwrap(),
                                local,
                                &buffer[..size],
                            )
                            .unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("coturn bridge read: {error}"),
                }
            }
        }
        self.ticks += 1;
        if self.ticks % 25 == 0 {
            thread::sleep(Duration::from_millis(1));
        }
        self.clock.advance(Duration::from_millis(1)).unwrap();
    }
}

fn turn_complete(
    peer: &PeerConnection,
    operation: OperationId,
    events: &mut Vec<PeerConnectionEvent>,
    pump: &mut TurnTlsPump<'_>,
) -> Option<SessionDescription> {
    for _ in 0..10_000 {
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::OperationComplete(result) = &event
                && result.operation_id == operation
            {
                return result.result.clone().unwrap();
            }
            events.push(event);
        }
        pump.tick();
    }
    panic!("TURN/TLS operation {} timed out", operation.get());
}

fn turn_gathered(
    peer: &PeerConnection,
    events: &mut Vec<PeerConnectionEvent>,
    pump: &mut TurnTlsPump<'_>,
) -> SessionDescription {
    for _ in 0..20_000 {
        events.extend(std::iter::from_fn(|| peer.try_next_event()));
        if events.iter().any(|event| {
            matches!(
                event,
                PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
            )
        }) {
            let descriptions = peer.descriptions().unwrap();
            let sdp = descriptions
                .pending_local
                .or(descriptions.current_local)
                .unwrap();
            assert!(
                sdp.sdp.contains("typ relay"),
                "no relay candidate in gathered SDP"
            );
            assert!(
                !sdp.sdp.contains("typ host"),
                "host candidate bypassed relay-only policy"
            );
            return sdp;
        }
        pump.tick();
    }
    panic!("TURN/TLS relay-only ICE gathering timed out");
}

#[test]
fn two_caller_pumped_peers_connect_over_relay_only_turn_tls() {
    let temp = std::env::temp_dir().join(format!(
        "pulsebeam-controlled-coturn-pair-{}",
        std::process::id()
    ));
    fs::create_dir_all(&temp).unwrap();
    let (ca_pem, _, _) = certificate(&temp);
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut server = TurnServer(
        turnserver::without_dtls()
            .args([
                "-n",
                "--no-cli",
                "--no-tcp",
                "--no-udp",
                "--no-multicast-peers",
                "--allow-loopback-peers",
                "--lt-cred-mech",
                "--realm=fixture.invalid",
                "--user=client:secret",
                "--listening-ip",
                "127.0.0.1",
                "--relay-ip",
                "127.0.0.1",
                "--tls-listening-port",
                &port.to_string(),
                "--cert",
                temp.join("server.pem").to_str().unwrap(),
                "--pkey",
                temp.join("server.key").to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let startup = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "coturn exited early"
        );
        if TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok() {
            break;
        }
        assert!(Instant::now() < startup, "coturn did not listen");
        thread::sleep(Duration::from_millis(10));
    }
    let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
    let queues = TaskQueueFactory::cooperative(&clock).unwrap();
    let environment = Environment::builder()
        .task_queue_factory(&queues)
        .build()
        .unwrap();
    let driver = ControlledPeerDriver::new(&clock).unwrap();
    let network = ControlledSimulatedNetwork::new(&clock, &driver).unwrap();
    let server_ip = Ipv4Addr::new(10, 73, 0, 3);
    let _remote = network.register_endpoint(server_ip.into()).unwrap();
    network
        .add_dns_record("relay.test", server_ip.into())
        .unwrap();
    let a = network
        .register_endpoint(Ipv4Addr::new(10, 73, 0, 1).into())
        .unwrap();
    let b = network
        .register_endpoint(Ipv4Addr::new(10, 73, 0, 2).into())
        .unwrap();
    let factory = |endpoint: &pulsebeam_webrtc_sys::NetworkEndpoint| {
        PeerConnectionFactory::builder()
            .environment(environment.clone())
            .controlled_driver(&driver)
            .network_manager(endpoint.network_manager().unwrap())
            .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
            .build()
            .unwrap()
    };
    let factory_a = factory(&a);
    let factory_b = factory(&b);
    let configuration = || PeerConfiguration {
        ice_servers: vec![IceServer {
            urls: vec![format!("turns:relay.test:{port}?transport=tcp")],
            username: "client".into(),
            password: "secret".into(),
        }],
        ice_transport_policy: IceTransportPolicy::RelayOnly,
        turn_tls_ca_pem: Some(ca_pem.clone()),
        ..PeerConfiguration::default()
    };
    let mut alice = factory_a.create_peer_connection(configuration()).unwrap();
    let mut bob = factory_b.create_peer_connection(configuration()).unwrap();
    let alice_channel = alice
        .create_data_channel("over-turn", DataChannelConfiguration::default())
        .unwrap();
    let mut pump = TurnTlsPump {
        clock: &clock,
        queues: &queues,
        driver: &driver,
        network: &network,
        streams: BTreeMap::new(),
        server_ip,
        server_port: port,
        ticks: 0,
    };
    let mut a_events = Vec::new();
    let mut b_events = Vec::new();
    let offer = turn_complete(
        &alice,
        alice.create_offer().unwrap(),
        &mut a_events,
        &mut pump,
    )
    .unwrap();
    turn_complete(
        &alice,
        alice.set_local_description(offer).unwrap(),
        &mut a_events,
        &mut pump,
    );
    let offer = turn_gathered(&alice, &mut a_events, &mut pump);
    turn_complete(
        &bob,
        bob.set_remote_description(offer).unwrap(),
        &mut b_events,
        &mut pump,
    );
    let answer =
        turn_complete(&bob, bob.create_answer().unwrap(), &mut b_events, &mut pump).unwrap();
    turn_complete(
        &bob,
        bob.set_local_description(answer).unwrap(),
        &mut b_events,
        &mut pump,
    );
    let answer = turn_gathered(&bob, &mut b_events, &mut pump);
    turn_complete(
        &alice,
        alice.set_remote_description(answer).unwrap(),
        &mut a_events,
        &mut pump,
    );

    let mut connected_a = a_events.iter().any(|event| {
        matches!(event,
        PeerConnectionEvent::ConnectionStateChanged(state) if format!("{state:?}") == "Connected")
    });
    let mut connected_b = b_events.iter().any(|event| {
        matches!(event,
        PeerConnectionEvent::ConnectionStateChanged(state) if format!("{state:?}") == "Connected")
    });
    for _ in 0..30_000 {
        a_events.extend(std::iter::from_fn(|| alice.try_next_event()));
        b_events.extend(std::iter::from_fn(|| bob.try_next_event()));
        connected_a |= a_events.iter().any(|event| matches!(event,
            PeerConnectionEvent::ConnectionStateChanged(state) if format!("{state:?}") == "Connected"));
        connected_b |= b_events.iter().any(|event| matches!(event,
            PeerConnectionEvent::ConnectionStateChanged(state) if format!("{state:?}") == "Connected"));
        if connected_a && connected_b {
            break;
        }
        pump.tick();
    }
    assert!(
        connected_a && connected_b,
        "relay-only TURN/TLS peers did not connect: alice={a_events:?}, bob={b_events:?}"
    );
    let mut bob_channel = None;
    for _ in 0..10_000 {
        b_events.extend(std::iter::from_fn(|| bob.try_next_event()));
        if let Some(index) = b_events
            .iter()
            .position(|event| matches!(event, PeerConnectionEvent::DataChannel(_)))
        {
            bob_channel = Some(match b_events.remove(index) {
                PeerConnectionEvent::DataChannel(channel) => channel,
                _ => unreachable!(),
            });
            break;
        }
        pump.tick();
    }
    let bob_channel = bob_channel.expect("remote data channel was not announced over relay");
    for _ in 0..10_000 {
        if alice_channel.state() == DataChannelState::Open
            && bob_channel.state() == DataChannelState::Open
        {
            break;
        }
        pump.tick();
    }
    assert_eq!(alice_channel.state(), DataChannelState::Open);
    assert_eq!(bob_channel.state(), DataChannelState::Open);
    assert_eq!(
        alice_channel.send(DataChannelMessage::binary([7, 0, 9])),
        DataChannelSendResult::Sent
    );
    let mut received = false;
    for _ in 0..10_000 {
        pump.tick();
        while let Some(event) = bob_channel.try_next_event() {
            if let DataChannelEvent::Message(message) = event {
                assert_eq!(message.bytes, [7, 0, 9]);
                received = true;
            }
        }
        if received {
            break;
        }
    }
    assert!(received, "data did not traverse both TURN/TLS allocations");
    alice.close().unwrap();
    bob.close().unwrap();
    drop(pump);
    drop(server);
    fs::remove_dir_all(temp).unwrap();
}
