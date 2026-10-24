//! Fixture-backed production ICE tests. The Linux runtime image supplies coturn.
//! Signaling transfers complete gathered SDP, never individual candidates.
#[path = "support/tools.rs"]
mod tools;
#[path = "support/turnserver.rs"]
mod turnserver;

use std::{
    fs,
    net::{IpAddr, Ipv6Addr, TcpListener, ToSocketAddrs, UdpSocket},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    thread,
    time::{Duration, Instant},
};

use pulsebeam_webrtc_sys::{
    ConnectionState, IceGatheringState, IceServer, IceTransportPolicy, OperationId,
    PeerConfiguration, PeerConnection, PeerConnectionEvent, PeerConnectionFactory,
    SessionDescription,
};

struct TurnServer(Child);

impl TurnServer {
    fn stop(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }

    fn start() -> (Self, IpAddr, u16) {
        Self::start_with_tls(None)
    }

    fn start_with_tls(cert_and_key: Option<(&Path, &Path)>) -> (Self, IpAddr, u16) {
        // Use a routable interface, not loopback: the default WebRTC network
        // manager does not offer loopback ICE interfaces in production mode.
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        socket.connect("192.0.2.1:9").unwrap();
        let ip = socket.local_addr().unwrap().ip();
        Self::start_on(ip, cert_and_key)
    }

    fn start_on(ip: IpAddr, cert_and_key: Option<(&Path, &Path)>) -> (Self, IpAddr, u16) {
        let port = TcpListener::bind((ip, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut command = turnserver::without_dtls();
        command.args([
            "-n",
            "--no-cli",
            "--no-multicast-peers",
            "--lt-cred-mech",
            "--realm=fixture.invalid",
            "--user=alice:secret",
            "--listening-ip",
            &ip.to_string(),
            "--relay-ip",
            &ip.to_string(),
            "--listening-port",
            &port.to_string(),
        ]);
        if ip.is_ipv4() {
            // A local hostname may resolve to both interface families. The
            // production allocator then contacts TURN over both, so listen
            // on usable IPv6 addresses too rather than leave UDP gathering
            // waiting for an unanswered IPv6 endpoint. Keep the IPv4 relay
            // address and all hostname/credential/connection assertions.
            let hostname = tools::command("hostname").output().unwrap();
            let hostname = String::from_utf8(hostname.stdout).unwrap();
            let listeners: std::collections::BTreeSet<_> = (hostname.trim(), port)
                .to_socket_addrs()
                .unwrap()
                .filter_map(|address| match address.ip() {
                    IpAddr::V6(address)
                        if !address.is_loopback()
                            && !address.is_unspecified()
                            && !address.is_multicast()
                            && !address.is_unicast_link_local() =>
                    {
                        Some(address)
                    }
                    _ => None,
                })
                .collect();
            for listener in listeners {
                command.args(["--listening-ip", &listener.to_string()]);
            }
        }
        if let Some((cert, key)) = cert_and_key {
            command.args([
                "--no-tcp",
                "--no-udp",
                "--tls-listening-port",
                &port.to_string(),
                "--cert",
                cert.to_str().unwrap(),
                "--pkey",
                key.to_str().unwrap(),
            ]);
        } else {
            command.arg("--no-tls");
        }
        if ip.is_ipv6() {
            // TURN defaults unqualified allocations to IPv4, even when the
            // client and relay interface are IPv6. Match this fixture's relay.
            command.arg("--allocation-default-address-family=ipv6");
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("coturn is required by the Linux runtime test image");
        let mut server = Self(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "coturn exited early"
            );
            if std::net::TcpStream::connect_timeout(&(ip, port).into(), Duration::from_millis(20))
                .is_ok()
            {
                return (server, ip, port);
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("coturn did not listen on {ip}:{port}");
    }
}

impl Drop for TurnServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct FixtureCertificate {
    directory: PathBuf,
    ca_pem: String,
    cert: PathBuf,
    key: PathBuf,
}

impl FixtureCertificate {
    fn new(hostname: &str) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "pulsebeam-turn-cert-{}-{}",
            std::process::id(),
            TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        ));
        fs::create_dir(&directory).unwrap();
        let ca = directory.join("ca.pem");
        let ca_key = directory.join("ca.key");
        let cert = directory.join("server.pem");
        let key = directory.join("server.key");
        let request = directory.join("server.csr");
        let extensions = directory.join("server.ext");
        fs::write(
            &extensions,
            format!("subjectAltName=DNS:{hostname}\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\nkeyUsage=digitalSignature,keyEncipherment\n"),
        )
        .unwrap();
        run_openssl(&[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-sha256",
            "-days",
            "1",
            "-keyout",
            ca_key.to_str().unwrap(),
            "-out",
            ca.to_str().unwrap(),
            "-subj",
            "/CN=Fixture TURN CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
        ]);
        run_openssl(&[
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            key.to_str().unwrap(),
            "-out",
            request.to_str().unwrap(),
            "-subj",
            &format!("/CN={hostname}"),
        ]);
        run_openssl(&[
            "x509",
            "-req",
            "-in",
            request.to_str().unwrap(),
            "-CA",
            ca.to_str().unwrap(),
            "-CAkey",
            ca_key.to_str().unwrap(),
            "-CAcreateserial",
            "-out",
            cert.to_str().unwrap(),
            "-days",
            "1",
            "-sha256",
            "-extfile",
            extensions.to_str().unwrap(),
        ]);
        let ca_pem = fs::read_to_string(ca).unwrap();
        Self {
            directory,
            ca_pem,
            cert,
            key,
        }
    }
}

impl Drop for FixtureCertificate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn run_openssl(args: &[&str]) {
    let output = tools::command("openssl").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "openssl: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture_hostname(ip: IpAddr, port: u16) -> String {
    let hostname = String::from_utf8(tools::command("hostname").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert!(
        (hostname.as_str(), port)
            .to_socket_addrs()
            .unwrap()
            .any(|address| address.ip() == ip),
        "fixture hostname must resolve to the listening interface"
    );
    hostname
}

fn wait_for<T>(
    peer: &PeerConnection,
    label: &str,
    events: &mut Vec<PeerConnectionEvent>,
    check: impl FnMut(&PeerConnectionEvent) -> Option<T>,
) -> T {
    wait_for_timeout(peer, label, events, Duration::from_secs(15), check)
}

fn wait_for_timeout<T>(
    peer: &PeerConnection,
    label: &str,
    events: &mut Vec<PeerConnectionEvent>,
    timeout: Duration,
    mut check: impl FnMut(&PeerConnectionEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + timeout;
    if let Some(found) = events.iter().find_map(&mut check) {
        return found;
    }
    while Instant::now() < deadline {
        while let Some(event) = peer.try_next_event() {
            let found = check(&event);
            events.push(event);
            if let Some(found) = found {
                return found;
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("timed out waiting for {label}: {events:#?}");
}

fn succeeded(
    peer: &PeerConnection,
    id: OperationId,
    events: &mut Vec<PeerConnectionEvent>,
) -> Option<SessionDescription> {
    wait_for(peer, "operation completion", events, |event| match event {
        PeerConnectionEvent::OperationComplete(result) if result.operation_id == id => {
            Some(result.result.clone().expect("operation failed"))
        }
        _ => None,
    })
}

fn gathered(peer: &PeerConnection, events: &mut Vec<PeerConnectionEvent>) -> SessionDescription {
    wait_for(peer, "ICE gathering completion", events, |event| {
        if matches!(
            event,
            PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
        ) {
            Some(
                peer.descriptions()
                    .unwrap()
                    .pending_local
                    .or_else(|| peer.descriptions().unwrap().current_local)
                    .expect("missing local description"),
            )
        } else {
            None
        }
    })
}

// Run explicitly on a Linux host with a routable global IPv6 interface.
// Unlike the STUN simulation, this exercises WebRTC's production interfaces.
#[test]
#[ignore = "requires a real globally addressed IPv6 interface; run with --ignored"]
fn relay_only_ipv6_udp_with_gathered_sdp() {
    let interfaces = fs::read_to_string("/proc/net/if_inet6").expect("Linux IPv6 interfaces");
    let ip = interfaces
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .find(|hex| hex.starts_with('2') || hex.starts_with('3'))
        .map(|hex| Ipv6Addr::from(u128::from_str_radix(hex, 16).unwrap()))
        .expect("a global IPv6 interface is required for production IPv6 qualification");
    let (_server, ip, port) = TurnServer::start_on(ip.into(), None);
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let config = PeerConfiguration {
        ice_servers: vec![IceServer {
            urls: vec![format!("turn:[{ip}]:{port}?transport=udp")],
            username: "alice".into(),
            password: "secret".into(),
        }],
        ice_transport_policy: IceTransportPolicy::RelayOnly,
        ..PeerConfiguration::default()
    };
    let mut alice = factory.create_peer_connection(config.clone()).unwrap();
    let mut bob = factory.create_peer_connection(config).unwrap();
    let mut alice_events = Vec::new();
    let mut bob_events = Vec::new();
    let offer = succeeded(&alice, alice.create_offer().unwrap(), &mut alice_events).unwrap();
    succeeded(
        &alice,
        alice.set_local_description(offer).unwrap(),
        &mut alice_events,
    );
    let gathered_offer = gathered(&alice, &mut alice_events);
    assert!(
        gathered_offer.sdp.contains("typ relay"),
        "IPv6 relay gathering failed: {alice_events:#?}"
    );
    assert!(gathered_offer.sdp.contains(&ip.to_string()));
    succeeded(
        &bob,
        bob.set_remote_description(gathered_offer).unwrap(),
        &mut bob_events,
    );
    let answer = succeeded(&bob, bob.create_answer().unwrap(), &mut bob_events).unwrap();
    succeeded(
        &bob,
        bob.set_local_description(answer).unwrap(),
        &mut bob_events,
    );
    let gathered_answer = gathered(&bob, &mut bob_events);
    assert!(
        gathered_answer.sdp.contains("typ relay"),
        "{}",
        gathered_answer.sdp
    );
    succeeded(
        &alice,
        alice.set_remote_description(gathered_answer).unwrap(),
        &mut alice_events,
    );
    wait_for(
        &alice,
        "IPv6 relay connection",
        &mut alice_events,
        |event| {
            matches!(
                event,
                PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected)
            )
            .then_some(())
        },
    );
    alice.close().unwrap();
    bob.close().unwrap();
}

#[test]
fn bad_turn_credentials_fail_without_relay_candidates() {
    let (_server, ip, port) = TurnServer::start();
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut peer = factory
        .create_peer_connection(PeerConfiguration {
            ice_servers: vec![IceServer {
                urls: vec![format!("turn:{ip}:{port}?transport=udp")],
                username: "alice".into(),
                password: "invalid".into(),
            }],
            ice_transport_policy: IceTransportPolicy::RelayOnly,
            ..PeerConfiguration::default()
        })
        .unwrap();
    let mut events = Vec::new();
    let offer = succeeded(&peer, peer.create_offer().unwrap(), &mut events).unwrap();
    succeeded(
        &peer,
        peer.set_local_description(offer).unwrap(),
        &mut events,
    );
    let description = gathered(&peer, &mut events);
    assert!(!description.sdp.contains("typ relay"), "{description:?}");
    assert!(
        events.iter().any(|event| matches!(event, PeerConnectionEvent::IceCandidateError { url, .. } if url.contains("turn:"))),
        "TURN authentication failure must surface as an ICE candidate error: {events:#?}"
    );
    peer.close().unwrap();
}

#[path = "common/panic_payload.rs"]
mod panic_payload;

#[test]
fn custom_tls_verifier_owns_chain_values_and_controls_native_turn_handshake() {
    use pulsebeam_webrtc_sys::{
        PeerConnectionDependencies, TlsCertificate, TlsCertificateChain, TlsCertificateVerifier,
    };
    use std::sync::{Arc, Mutex};
    struct Verify {
        expected: Vec<u8>,
        observed: Arc<Mutex<Vec<TlsCertificateChain>>>,
        permit: bool,
        panic: bool,
    }
    impl TlsCertificateVerifier for Verify {
        fn verify_chain(&self, chain: &TlsCertificateChain) -> bool {
            self.observed.lock().unwrap().push(chain.clone());
            if self.panic {
                panic_payload::raise();
            }
            self.permit
                && chain
                    .get(0)
                    .is_some_and(|leaf| leaf.to_der() == self.expected)
        }
    }
    impl Drop for Verify {
        fn drop(&mut self) {
            if self.panic {
                panic_payload::raise();
            }
        }
    }
    let hostname = String::from_utf8(tools::command("hostname").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned();
    let fixture = FixtureCertificate::new(&hostname);
    let expected = TlsCertificate::from_pem(&std::fs::read_to_string(&fixture.cert).unwrap())
        .unwrap()
        .to_der();
    let (_server, _ip, port) = TurnServer::start_with_tls(Some((&fixture.cert, &fixture.key)));
    let factory = PeerConnectionFactory::builder().build().unwrap();
    for (permit, panic) in [(true, false), (false, false), (true, true)] {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Verify {
            expected: expected.clone(),
            observed: observed.clone(),
            permit,
            panic,
        });
        let weak = Arc::downgrade(&provider);
        let mut peer = factory
            .create_peer_connection_with_dependencies(
                PeerConfiguration {
                    ice_servers: vec![IceServer {
                        urls: vec![format!("turns:{hostname}:{port}?transport=tcp")],
                        username: "alice".into(),
                        password: "secret".into(),
                    }],
                    ice_transport_policy: IceTransportPolicy::RelayOnly,
                    ..PeerConfiguration::default()
                }
                .into(),
                PeerConnectionDependencies::default().tls_certificate_verifier(provider.clone()),
            )
            .unwrap();
        drop(provider);
        assert!(
            weak.upgrade().is_some(),
            "native peer must retain its provider"
        );
        let mut events = Vec::new();
        let offer = succeeded(&peer, peer.create_offer().unwrap(), &mut events).unwrap();
        succeeded(
            &peer,
            peer.set_local_description(offer).unwrap(),
            &mut events,
        );
        let description = gathered(&peer, &mut events);
        assert_eq!(
            description.sdp.contains("typ relay"),
            permit && !panic,
            "{description:?}"
        );
        assert!(
            !observed.lock().unwrap().is_empty(),
            "native TLS must invoke the verifier"
        );
        peer.close().unwrap();
        drop(peer);
        assert!(
            weak.upgrade().is_none(),
            "retired peer must release its provider"
        );
        let chains = std::mem::take(&mut *observed.lock().unwrap());
        let expected = expected.clone();
        std::thread::spawn(move || {
            for chain in chains {
                assert_eq!(chain.get(0).unwrap().to_der(), expected);
                assert!(chain.stats().is_some());
            }
        })
        .join()
        .unwrap();
    }
}

#[test]
fn turn_tls_requires_trusted_ca_and_matching_hostname() {
    let hostname = String::from_utf8(tools::command("hostname").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned();
    let fixture = FixtureCertificate::new(&hostname);
    let (_server, ip, port) = TurnServer::start_with_tls(Some((&fixture.cert, &fixture.key)));
    assert_eq!(fixture_hostname(ip, port), hostname);
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let ice_server = IceServer {
        urls: vec![format!("turns:{hostname}:{port}?transport=tcp")],
        username: "alice".into(),
        password: "secret".into(),
    };
    let config = PeerConfiguration {
        ice_servers: vec![ice_server.clone()],
        ice_transport_policy: IceTransportPolicy::RelayOnly,
        turn_tls_ca_pem: Some(fixture.ca_pem.clone()),
        ..PeerConfiguration::default()
    };
    let mut alice = factory.create_peer_connection(config.clone()).unwrap();
    let mut bob = factory.create_peer_connection(config).unwrap();
    let mut alice_events = Vec::new();
    let mut bob_events = Vec::new();
    let offer = succeeded(&alice, alice.create_offer().unwrap(), &mut alice_events).unwrap();
    succeeded(
        &alice,
        alice.set_local_description(offer).unwrap(),
        &mut alice_events,
    );
    let gathered_offer = gathered(&alice, &mut alice_events);
    assert!(
        gathered_offer.sdp.contains("typ relay"),
        "{}",
        gathered_offer.sdp
    );
    succeeded(
        &bob,
        bob.set_remote_description(gathered_offer).unwrap(),
        &mut bob_events,
    );
    let answer = succeeded(&bob, bob.create_answer().unwrap(), &mut bob_events).unwrap();
    succeeded(
        &bob,
        bob.set_local_description(answer).unwrap(),
        &mut bob_events,
    );
    let gathered_answer = gathered(&bob, &mut bob_events);
    assert!(
        gathered_answer.sdp.contains("typ relay"),
        "{}",
        gathered_answer.sdp
    );
    succeeded(
        &alice,
        alice.set_remote_description(gathered_answer).unwrap(),
        &mut alice_events,
    );
    wait_for(
        &alice,
        "TURN/TLS relay connection",
        &mut alice_events,
        |event| {
            matches!(
                event,
                PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected)
            )
            .then_some(())
        },
    );
    alice.close().unwrap();
    bob.close().unwrap();

    for (url, ca) in [
        (format!("turns:{hostname}:{port}?transport=tcp"), None),
        (
            format!("turns:{ip}:{port}?transport=tcp"),
            Some(fixture.ca_pem.clone()),
        ),
    ] {
        let mut peer = factory
            .create_peer_connection(PeerConfiguration {
                ice_servers: vec![IceServer {
                    urls: vec![url],
                    ..ice_server.clone()
                }],
                ice_transport_policy: IceTransportPolicy::RelayOnly,
                turn_tls_ca_pem: ca,
                ..PeerConfiguration::default()
            })
            .unwrap();
        let mut events = Vec::new();
        let offer = succeeded(&peer, peer.create_offer().unwrap(), &mut events).unwrap();
        succeeded(
            &peer,
            peer.set_local_description(offer).unwrap(),
            &mut events,
        );
        let description = gathered(&peer, &mut events);
        assert!(!description.sdp.contains("typ relay"), "{description:?}");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, PeerConnectionEvent::IceCandidateError { .. })),
            "TLS failure must surface: {events:#?}"
        );
        peer.close().unwrap();
    }
}

#[test]
fn relay_only_udp_and_tcp_with_hostname_and_credentials() {
    let (mut server, ip, port) = TurnServer::start();
    let hostname = fixture_hostname(ip, port);
    for transport in ["udp", "tcp"] {
        let factory = PeerConnectionFactory::builder().build().unwrap();
        let ice_server = IceServer {
            urls: vec![format!("turn:{hostname}:{port}?transport={transport}")],
            username: "alice".into(),
            password: "secret".into(),
        };
        let config = PeerConfiguration {
            ice_servers: vec![ice_server],
            ice_transport_policy: IceTransportPolicy::RelayOnly,
            ..PeerConfiguration::default()
        };
        let mut alice = factory.create_peer_connection(config.clone()).unwrap();
        let mut bob = factory.create_peer_connection(config).unwrap();
        let mut alice_events = Vec::new();
        let mut bob_events = Vec::new();
        let offer = succeeded(&alice, alice.create_offer().unwrap(), &mut alice_events).unwrap();
        succeeded(
            &alice,
            alice.set_local_description(offer).unwrap(),
            &mut alice_events,
        );
        let gathered_offer = gathered(&alice, &mut alice_events);
        assert!(
            gathered_offer.sdp.contains("typ relay"),
            "{transport}: {}",
            gathered_offer.sdp
        );
        assert!(gathered_offer.sdp.contains(&ip.to_string()));
        succeeded(
            &bob,
            bob.set_remote_description(gathered_offer).unwrap(),
            &mut bob_events,
        );
        let answer = succeeded(&bob, bob.create_answer().unwrap(), &mut bob_events).unwrap();
        succeeded(
            &bob,
            bob.set_local_description(answer).unwrap(),
            &mut bob_events,
        );
        let gathered_answer = gathered(&bob, &mut bob_events);
        assert!(
            gathered_answer.sdp.contains("typ relay"),
            "{transport}: {}",
            gathered_answer.sdp
        );
        succeeded(
            &alice,
            alice.set_remote_description(gathered_answer).unwrap(),
            &mut alice_events,
        );
        wait_for(&alice, "relay connection", &mut alice_events, |event| {
            matches!(
                event,
                PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected)
            )
            .then_some(())
        });
        if transport == "tcp" {
            // The only candidate pair uses this relay. Once it disappears,
            // the native peer must expose the broken connection to Rust.
            while let Some(event) = alice.try_next_event() {
                alice_events.push(event);
            }
            server.stop();
            wait_for_timeout(
                &alice,
                "connection loss after TURN shutdown",
                &mut Vec::new(),
                Duration::from_secs(45),
                |event| {
                    matches!(
                        event,
                        PeerConnectionEvent::ConnectionStateChanged(
                            ConnectionState::Disconnected | ConnectionState::Failed
                        )
                    )
                    .then_some(())
                },
            );
        }
        alice.close().unwrap();
        bob.close().unwrap();
    }
}
