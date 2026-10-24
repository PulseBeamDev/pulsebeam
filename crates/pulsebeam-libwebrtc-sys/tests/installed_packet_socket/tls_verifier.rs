//! Capture an actual SDK-selected socket verifier, retire its originating peer,
//! then use the retained capability in real TLS handshakes. This does not claim
//! that the rejecting capture factory itself transports TURN or runs SDK TLS.
use super::*;
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, ServerConfig,
    ServerConnection, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};
use std::{
    fmt, fs,
    io::{Cursor, Read, Write},
    path::PathBuf,
    sync::atomic::AtomicBool,
};
#[path = "../support/tls_certificate.rs"]
mod tls_certificate;

#[derive(Default)]
struct Captured(Mutex<Vec<PacketSocketTcpOptions>>);
struct CaptureSockets {
    udp: Sockets,
    captured: Arc<Captured>,
}
impl PacketSocketFactoryImplementation for CaptureSockets {
    fn create_udp_socket(
        &self,
        c: PacketSocketContext,
        l: SocketAddress,
        min: u16,
        max: u16,
    ) -> Option<Box<dyn PacketSocket>> {
        self.udp.create_udp_socket(c, l, min, max)
    }
    fn create_server_tcp_socket(
        &self,
        c: ListenSocketContext,
        l: SocketAddress,
        min: u16,
        max: u16,
        o: PacketSocketFlags,
    ) -> Option<Box<dyn ListenSocket>> {
        self.udp.create_server_tcp_socket(c, l, min, max, o)
    }
    fn create_client_tcp_socket(
        &self,
        _: PacketSocketContext,
        _: SocketAddress,
        _: SocketAddress,
        options: PacketSocketTcpOptions,
    ) -> Option<Box<dyn PacketSocket>> {
        self.captured.0.lock().unwrap().push(options);
        None // Explicit rejection, not a fake TLS socket or successful I/O.
    }
    fn create_client_udp_socket(
        &self,
        c: PacketSocketContext,
        l: SocketAddress,
        r: SocketAddress,
        min: u16,
        max: u16,
        o: PacketSocketTcpOptions,
    ) -> Option<Box<dyn PacketSocket>> {
        self.udp.create_client_udp_socket(c, l, r, min, max, o)
    }
    fn create_async_dns_resolver(&self, environment: Environment) -> Box<dyn AsyncDnsResolver> {
        self.udp.create_async_dns_resolver(environment)
    }
    fn is_controlled(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct Calls {
    leaf: AtomicUsize,
    chain: AtomicUsize,
    drops: AtomicUsize,
    panic: AtomicBool,
}
struct Selected {
    der: Vec<u8>,
    calls: Arc<Calls>,
}
impl TlsCertificateVerifier for Selected {
    fn verify(&self, certificate: &TlsCertificate) -> bool {
        self.calls.leaf.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.calls.panic.load(Ordering::SeqCst),
            "selected verifier failure"
        );
        certificate.to_der() == self.der
    }
    fn verify_chain(&self, chain: &TlsCertificateChain) -> bool {
        self.calls.chain.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.calls.panic.load(Ordering::SeqCst),
            "selected verifier failure"
        );
        chain
            .get(0)
            .is_some_and(|certificate| certificate.to_der() == self.der)
    }
}
impl Drop for Selected {
    fn drop(&mut self) {
        self.calls.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct RetainedVerifier {
    native: SocketTlsVerifier,
    leaf: TlsCertificate,
    leaf_mode: bool,
    signatures: Arc<WebPkiServerVerifier>,
}
impl fmt::Debug for RetainedVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RetainedSocketVerifier")
    }
}
impl ServerCertVerifier for RetainedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // The real TLS handshake must present exactly the native certificate
        // supplied to the retained SDK verifier. Also check actual CA trust,
        // hostname and validity independently, without bypassing signatures.
        self.signatures
            .verify_server_cert(end_entity, intermediates, name, ocsp, now)?;
        if end_entity.as_ref() != self.leaf.to_der() || !intermediates.is_empty() {
            return Err(rustls::Error::General(
                "unexpected fixture certificate chain".into(),
            ));
        }
        let accepted = if self.leaf_mode {
            self.native.verify(&self.leaf)
        } else {
            self.native
                .verify_chain(&TlsCertificateChain::from_certificate(self.leaf.clone()))
        };
        if !accepted {
            return Err(rustls::Error::General(
                "selected retained verifier rejected TLS".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signatures
            .verify_tls12_signature(message, certificate, signature)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signatures
            .verify_tls13_signature(message, certificate, signature)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.signatures.supported_verify_schemes()
    }
}
struct Credentials {
    directory: PathBuf,
    ca: String,
    der: Vec<u8>,
    key: Vec<u8>,
    leaf: TlsCertificate,
}
impl Credentials {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "pulsebeam-retained-socket-tls-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let (ca, der, key) = tls_certificate::certificate(&directory);
        let leaf =
            TlsCertificate::from_pem(&fs::read_to_string(directory.join("server.pem")).unwrap())
                .unwrap();
        Self {
            directory,
            ca,
            der,
            key,
            leaf,
        }
    }
    fn handshake(&self, native: &SocketTlsVerifier, leaf_mode: bool) -> bool {
        let crypto = Arc::new(oxitls_rustcrypto_provider::provider());
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(
                TlsCertificate::from_pem(&self.ca).unwrap().to_der(),
            ))
            .unwrap();
        let signatures =
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), crypto.clone())
                .build()
                .unwrap();
        let client = ClientConfig::builder_with_provider(crypto.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(RetainedVerifier {
                native: native.clone(),
                leaf: self.leaf.clone(),
                leaf_mode,
                signatures,
            }))
            .with_no_client_auth();
        let server = ServerConfig::builder_with_provider(crypto)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(self.der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone())),
            )
            .unwrap();
        let mut client = ClientConnection::new(
            Arc::new(client),
            ServerName::try_from("relay.test").unwrap().to_owned(),
        )
        .unwrap();
        let mut server = ServerConnection::new(Arc::new(server)).unwrap();
        for _ in 0..64 {
            let mut wire = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut wire).unwrap();
            }
            if !wire.is_empty() {
                server.read_tls(&mut Cursor::new(wire)).unwrap();
                if server.process_new_packets().is_err() {
                    return false;
                }
            }
            let mut wire = Vec::new();
            while server.wants_write() {
                server.write_tls(&mut wire).unwrap();
            }
            if !wire.is_empty() {
                client.read_tls(&mut Cursor::new(wire)).unwrap();
                if client.process_new_packets().is_err() {
                    return false;
                }
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                let payload = b"authenticated through the retained SDK verifier";
                client.writer().write_all(payload).unwrap();
                let mut encrypted = Vec::new();
                while client.wants_write() {
                    client.write_tls(&mut encrypted).unwrap();
                }
                server.read_tls(&mut Cursor::new(encrypted)).unwrap();
                server.process_new_packets().unwrap();
                let mut received = vec![0; payload.len()];
                server.reader().read_exact(&mut received).unwrap();
                assert_eq!(received, payload);
                return true;
            }
        }
        panic!("real TLS handshake made no bounded progress");
    }
}
impl Drop for Credentials {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn sdk_selected_ca_and_rust_verifier_aliases_authenticate_tls_after_peer_retirement() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let credentials = Credentials::new();
    let certificate = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let harness = Harness::new();
    for custom in [false, true] {
        let ip = Ipv4Addr::new(10, 84, 0, 1);
        let captured = Arc::new(Captured::default());
        let sockets = PacketSocketFactory::from_provider(CaptureSockets {
            udp: Sockets {
                ip,
                link: harness.link.clone(),
            },
            captured: captured.clone(),
        });
        let managers = NetworkManagerFactory::from_provider(Managers {
            ip,
            link: harness.link.clone(),
        });
        let factory = PeerConnectionFactory::builder()
            .environment(harness.environment.clone())
            .controlled_driver(&harness.driver)
            .options(FactoryOptions {
                disable_network_monitor: true,
                ..Default::default()
            })
            .network_manager_factory(&managers)
            .socket_factory(&sockets)
            .build()
            .unwrap();
        let calls = Arc::new(Calls::default());
        let selected = Arc::new(Selected {
            der: credentials.der.clone(),
            calls: calls.clone(),
        });
        let weak = Arc::downgrade(&selected);
        let mut dependencies = PeerConnectionDependencies::default();
        if custom {
            dependencies.tls_certificate_verifier = Some(selected.clone());
        }
        drop(selected);
        let mut configuration = RtcConfiguration::from(PeerConfiguration {
            ice_servers: vec![IceServer {
                urls: vec!["turns:10.84.0.2:5349?transport=tcp".into()],
                username: "client".into(),
                password: "secret".into(),
            }],
            turn_tls_ca_pem: (!custom).then(|| credentials.ca.clone()),
            ..Default::default()
        });
        configuration.options.certificates.push(certificate.clone());
        configuration.options.tcp_candidate_policy = TcpCandidatePolicy::Disabled;
        let mut peer = factory
            .create_peer_connection_with_dependencies(configuration, dependencies)
            .unwrap();
        let channel = peer
            .create_data_channel("capture-verifier-origin", Default::default())
            .unwrap();
        let mut events = Vec::new();
        let offer = harness
            .completed(&peer, peer.create_offer().unwrap(), &mut events)
            .unwrap();
        harness.completed(
            &peer,
            peer.set_local_description(offer).unwrap(),
            &mut events,
        );
        for _ in 0..2_000 {
            if !captured.0.lock().unwrap().is_empty() {
                break;
            }
            harness.step();
        }
        let options = std::mem::take(&mut *captured.0.lock().unwrap());
        assert!(
            !options.is_empty(),
            "actual SDK TURN port did not reach selected TCP creator"
        );
        assert!(
            options
                .iter()
                .all(|options| options.options.0 & PacketSocketFlags::TLS.0 != 0
                    && options.tls_cert_verifier.is_some())
        );
        peer.close().unwrap();
        drop((channel, peer, factory, sockets, managers, captured));
        assert_eq!(
            harness.link.created.load(Ordering::SeqCst),
            harness.link.dropped.load(Ordering::SeqCst)
        );
        let aliases: Vec<_> = options
            .into_iter()
            .map(|options| options.tls_cert_verifier.unwrap())
            .collect();
        if custom {
            assert!(weak.upgrade().is_some());
            assert_eq!(calls.drops.load(Ordering::SeqCst), 0);
        } else {
            assert!(weak.upgrade().is_none());
        }
        for leaf_mode in [false, true] {
            assert!(credentials.handshake(&aliases[0], leaf_mode));
        }
        assert!(aliases.iter().all(|alias| !alias.has_failed()));
        if custom {
            assert!(
                calls.leaf.load(Ordering::SeqCst) > 0 && calls.chain.load(Ordering::SeqCst) > 0
            );
            calls.panic.store(true, Ordering::SeqCst);
            assert!(
                !credentials.handshake(&aliases[0], false),
                "provider panic must reject the real TLS handshake"
            );
            let attempts = calls.chain.load(Ordering::SeqCst);
            assert!(aliases.iter().all(SocketTlsVerifier::has_failed));
            assert!(!credentials.handshake(&aliases[0], false));
            assert_eq!(
                calls.chain.load(Ordering::SeqCst),
                attempts,
                "poisoned shared backing must not reenter the provider"
            );
        }
        drop(aliases);
        assert!(weak.upgrade().is_none());
        assert_eq!(calls.drops.load(Ordering::SeqCst), 1);
    }
}
