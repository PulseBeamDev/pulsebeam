//! Unpatched Noq DATAGRAM adapter. One provisioned, fixed-path peer per endpoint.

use crate::backend::Output;
use crate::{Error, NetworkClass, PeerFingerprint, PeerRole, UDP_BUDGET};
use bytes::BytesMut;
use noq_proto::{
    Connection, ConnectionHandle, DatagramEvent, Endpoint, EndpointConfig, Event, FourTuple,
    TransportConfig,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

// Noq/rustls require shared immutable configuration and clock-provider handles.
// Only the engine owner writes this injected clock; there are no clock reads,
// shared connection locks, or packet-path reference-count clones.
#[allow(
    clippy::disallowed_types,
    reason = "Noq/rustls cold configuration and single-writer injected-clock provider handles"
)]
mod ownership {
    pub(super) type Shared<T> = std::sync::Arc<T>;
    pub(super) type ClockWord = std::sync::atomic::AtomicU64;
}
use ownership::{ClockWord, Shared};

const ALPN: &[u8] = b"pulsebeam-datagram/1";
const SERVER_NAME: &str = "pulsebeam";
const IDLE: Duration = Duration::from_secs(60);
const RECEIVE_BYTES: usize = UDP_BUDGET * 32;

#[derive(Clone)]
pub struct QuicCredential(Shared<Identity>);
struct Identity {
    certificate: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

impl QuicCredential {
    pub fn generate() -> Result<Self, Error> {
        let key = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()])
            .map_err(|_| Error::Security)?;
        Ok(Self(Shared::new(Identity {
            certificate: key.cert.der().clone(),
            key: PrivateKeyDer::Pkcs8(key.signing_key.serialize_der().into()),
        })))
    }
    pub fn fingerprint(&self) -> PeerFingerprint {
        fingerprint(&self.0.certificate)
    }
}

fn fingerprint(cert: &CertificateDer<'_>) -> PeerFingerprint {
    PeerFingerprint::from_bytes(Sha256::digest(cert.as_ref()).into())
}

/// Finite inner safety ceilings, not a measured link capacity. The outer
/// controller remains authoritative. These can still block and are not an
/// advertised pacing-disable switch.
#[derive(Clone, Copy, Debug)]
pub struct QuicTuning {
    pub inner_window_bytes: u64,
    pub inner_pacing_bytes_per_second: u64,
    /// Reject this backend when the caller requires cancellation inside Noq's queue.
    pub require_queue_cancellation: bool,
}
impl Default for QuicTuning {
    fn default() -> Self {
        Self {
            inner_window_bytes: 32 * 1024 * 1024,
            inner_pacing_bytes_per_second: 1_000_000_000,
            require_queue_cancellation: false,
        }
    }
}

impl QuicTuning {
    pub const fn queue_cancellation_supported(&self) -> bool {
        false
    }
    pub(crate) fn validate(self) -> Result<(), Error> {
        if self.require_queue_cancellation {
            return Err(Error::UnsupportedProfile);
        }
        if !(UDP_BUDGET as u64..=64 * 1024 * 1024).contains(&self.inner_window_bytes)
            || !(UDP_BUDGET as u64..=10_000_000_000).contains(&self.inner_pacing_bytes_per_second)
        {
            return Err(Error::Capacity);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct QuicPeer {
    pub endpoint: SocketAddr,
    pub credential: QuicCredential,
    pub expected_peer: PeerFingerprint,
    pub role: PeerRole,
    pub class: NetworkClass,
    pub tuning: QuicTuning,
}

#[derive(Clone, Debug)]
struct Ceiling(QuicTuning);
impl noq_proto::congestion::ControllerFactory for Ceiling {
    fn build(
        self: Shared<Self>,
        _now: Instant,
        _mtu: u16,
    ) -> Box<dyn noq_proto::congestion::Controller> {
        Box::new(self.as_ref().clone())
    }
}
impl noq_proto::congestion::Controller for Ceiling {
    fn on_congestion_event(
        &mut self,
        _now: Instant,
        _sent: Instant,
        _persistent: bool,
        _app_limited: bool,
        _lost: u64,
        _largest_lost_packet: u64,
    ) {
    }
    fn on_mtu_update(&mut self, _mtu: u16) {}
    fn window(&self) -> u64 {
        self.0.inner_window_bytes
    }
    fn initial_window(&self) -> u64 {
        self.window()
    }
    fn clone_box(&self) -> Box<dyn noq_proto::congestion::Controller> {
        Box::new(self.clone())
    }
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
    fn metrics(&self) -> noq_proto::congestion::ControllerMetrics {
        let mut metrics = noq_proto::congestion::ControllerMetrics::default();
        metrics.congestion_window = self.window();
        metrics.pacing_rate = Some(self.0.inner_pacing_bytes_per_second);
        metrics.send_quantum = Some(UDP_BUDGET as u64);
        metrics
    }
}

#[derive(Debug)]
struct Clock {
    elapsed: ClockWord,
}
impl Clock {
    fn duration(&self) -> Duration {
        Duration::from_nanos(self.elapsed.load(Ordering::Relaxed))
    }
}
impl rustls::time_provider::TimeProvider for Clock {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(self.duration()))
    }
}
impl noq_proto::TimeSource for Clock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
            .checked_add(self.duration())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

// This matches the initial DTLS provisioning model: trust the intended peer's
// exact certificate fingerprint, not DNS/CA identity or wall-clock validity.
// CertificateVerify signatures are still cryptographically verified below.
#[derive(Debug)]
struct Pin {
    expected: PeerFingerprint,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}
impl Pin {
    fn check(
        &self,
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<(), rustls::Error> {
        if !intermediates.is_empty() || fingerprint(cert) != self.expected {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(())
    }
}
impl rustls::client::danger::ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        self.check(cert, intermediates)?;
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _signature: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 disabled".into()))
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, signature, &self.algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}
impl rustls::server::danger::ClientCertVerifier for Pin {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        self.check(cert, intermediates)?;
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        <Self as rustls::client::danger::ServerCertVerifier>::verify_tls12_signature(
            self, message, cert, signature,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        <Self as rustls::client::danger::ServerCertVerifier>::verify_tls13_signature(
            self, message, cert, signature,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuicStats {
    pub acks_received: u64,
    pub lost_packets: u64,
    pub window_bytes: u64,
    pub queued_payload_bytes: usize,
    pub inner_stall_polls: u64,
}

pub(crate) struct Quic {
    endpoint: Endpoint,
    connection: Option<(ConnectionHandle, Connection)>,
    remote: SocketAddr,
    clock: Shared<Clock>,
    origin: Instant,
    now: Instant,
    buffer: Vec<u8>,
    response: Option<Vec<u8>>,
    last_application: bool,
    stats: QuicStats,
    connected: bool,
    capacity: usize,
    empty_send_space: usize,
    closed: bool,
}

impl Quic {
    pub(crate) fn new(peer: &QuicPeer, now: Instant) -> Result<Self, Error> {
        let tuning = peer.tuning;
        tuning.validate()?;
        let clock = Shared::new(Clock {
            elapsed: ClockWord::new(0),
        });
        let provider = Shared::new(rustls::crypto::aws_lc_rs::default_provider());
        let pin = Shared::new(Pin {
            expected: peer.expected_peer,
            algorithms: provider.signature_verification_algorithms,
        });
        let mtu = u16::try_from(UDP_BUDGET).map_err(|_| Error::Capacity)?;
        let mut transport = TransportConfig::default();
        transport
            .max_concurrent_bidi_streams(0u32.into())
            .max_concurrent_uni_streams(0u32.into())
            .initial_mtu(mtu)
            .min_mtu(mtu)
            .mtu_discovery_config(None)
            .allow_spin(false)
            .send_observed_address_reports(false)
            .receive_observed_address_reports(false)
            .max_remote_nat_traversal_addresses(0)
            .max_concurrent_multipath_paths(0)
            .server_handshake_migration(false)
            .pad_to_mtu(false)
            .keep_alive_interval(None)
            .max_idle_timeout(Some(IDLE.try_into().map_err(|_| Error::Capacity)?))
            .datagram_receive_buffer_size(Some(RECEIVE_BYTES))
            .datagram_send_buffer_size(UDP_BUDGET)
            .enable_segmentation_offload(false)
            .congestion_controller_factory(Shared::new(Ceiling(tuning)));
        let transport = Shared::new(transport);
        let certificates = vec![peer.credential.0.certificate.clone()];
        let mut endpoint_config = EndpointConfig::default();
        endpoint_config
            .grease_quic_bit(false)
            .max_udp_payload_size(mtu)
            .map_err(|_| Error::Capacity)?;
        let (mut endpoint, client) = match peer.role {
            PeerRole::Initiator => {
                let mut tls = rustls::ClientConfig::builder_with_details(provider, clock.clone())
                    .with_protocol_versions(&[&rustls::version::TLS13])
                    .map_err(|_| Error::Security)?
                    .dangerous()
                    .with_custom_certificate_verifier(pin)
                    .with_client_auth_cert(certificates, peer.credential.0.key.clone_key())
                    .map_err(|_| Error::Security)?;
                tls.alpn_protocols = vec![ALPN.into()];
                tls.enable_early_data = false;
                tls.resumption = rustls::client::Resumption::disabled();
                let crypto = noq_proto::crypto::rustls::QuicClientConfig::try_from(tls)
                    .map_err(|_| Error::Security)?;
                let mut client = noq_proto::ClientConfig::new(Shared::new(crypto));
                client.transport_config(transport);
                (
                    Endpoint::new(Shared::new(endpoint_config), None, true),
                    Some(client),
                )
            }
            PeerRole::Responder => {
                let mut tls = rustls::ServerConfig::builder_with_details(provider, clock.clone())
                    .with_protocol_versions(&[&rustls::version::TLS13])
                    .map_err(|_| Error::Security)?
                    .with_client_cert_verifier(pin)
                    .with_single_cert(certificates, peer.credential.0.key.clone_key())
                    .map_err(|_| Error::Security)?;
                tls.alpn_protocols = vec![ALPN.into()];
                tls.max_early_data_size = 0;
                tls.send_tls13_tickets = 0;
                let crypto = noq_proto::crypto::rustls::QuicServerConfig::try_from(tls)
                    .map_err(|_| Error::Security)?;
                let mut server = noq_proto::ServerConfig::with_crypto(Shared::new(crypto));
                server
                    .transport_config(transport)
                    .migration(false)
                    .max_incoming(1)
                    .incoming_buffer_size(RECEIVE_BYTES as u64)
                    .incoming_buffer_size_total(RECEIVE_BYTES as u64)
                    .time_source(clock.clone());
                (
                    Endpoint::new(
                        Shared::new(endpoint_config),
                        Some(Shared::new(server)),
                        true,
                    ),
                    None,
                )
            }
        };
        let connection = client
            .map(|client| endpoint.connect(now, client, peer.endpoint, SERVER_NAME))
            .transpose()
            .map_err(|_| Error::Security)?;
        Ok(Self {
            endpoint,
            connection,
            remote: peer.endpoint,
            clock,
            origin: now,
            now,
            buffer: Vec::with_capacity(UDP_BUDGET),
            response: None,
            last_application: false,
            stats: QuicStats::default(),
            connected: false,
            capacity: 0,
            empty_send_space: 0,
            closed: false,
        })
    }

    pub(crate) fn set_time(&mut self, now: Instant) {
        self.now = now;
        self.clock.elapsed.store(
            u64::try_from(now.saturating_duration_since(self.origin).as_nanos())
                .unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
    pub(crate) fn handle_timeout(&mut self, now: Instant) -> Result<(), Error> {
        self.set_time(now);
        if let Some((_, connection)) = &mut self.connection
            && connection.poll_timeout().is_some_and(|at| at <= now)
        {
            connection.handle_timeout(now);
        }
        Ok(())
    }
    pub(crate) fn handle_packet(&mut self, packet: &[u8]) -> Result<(), Error> {
        self.buffer.clear();
        match self.endpoint.handle(
            self.now,
            FourTuple::from_remote(self.remote),
            None,
            BytesMut::from(packet),
            &mut self.buffer,
        ) {
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                if let Some((current, connection)) = &mut self.connection
                    && *current == handle
                {
                    connection.handle_event(event);
                }
            }
            Some(DatagramEvent::NewConnection(incoming)) => {
                if self.connection.is_some() {
                    self.endpoint.ignore(incoming);
                } else {
                    self.connection = Some(
                        self.endpoint
                            .accept(incoming, self.now, &mut self.buffer, None)
                            .map_err(|_| Error::Security)?,
                    );
                }
            }
            Some(DatagramEvent::Response(transmit)) => {
                if transmit.size > UDP_BUDGET {
                    return Err(Error::Capacity);
                }
                self.response = Some(
                    self.buffer
                        .get(..transmit.size)
                        .ok_or(Error::Capacity)?
                        .to_vec(),
                );
            }
            None => {}
        }
        Ok(())
    }
    pub(crate) fn send_application_data(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.connection
            .as_mut()
            .ok_or(Error::NotReady)?
            .1
            .datagrams()
            .send(owned_datagram(bytes), false)
            .map_err(|_| Error::Blocked)
    }
    pub(crate) fn poll_output<'a>(&mut self, output: &'a mut [u8]) -> Result<Output<'a>, Error> {
        let result = self.poll_output_inner(output);
        if let Some((_, connection)) = &mut self.connection {
            self.stats.queued_payload_bytes = self
                .empty_send_space
                .saturating_sub(connection.datagrams().send_buffer_space());
            if let Some(path) = connection.path_stats(noq_proto::PathId::ZERO) {
                self.stats.acks_received = path.frame_rx.acks;
                self.stats.lost_packets = path.lost_packets;
                self.stats.window_bytes = path.cwnd;
            }
        }
        result
    }

    pub(crate) fn poll_output_inner<'a>(
        &mut self,
        output: &'a mut [u8],
    ) -> Result<Output<'a>, Error> {
        self.last_application = false;
        if self.closed {
            return Ok(Output::Closed);
        }
        if let Some(bytes) = self.response.take() {
            let destination = output.get_mut(..bytes.len()).ok_or(Error::Capacity)?;
            destination.copy_from_slice(&bytes);
            return Ok(Output::Datagram(destination));
        }
        if let Some((handle, connection)) = &mut self.connection {
            if let Some(event) = connection.poll_endpoint_events() {
                if let Some(event) = self.endpoint.handle_event(*handle, event) {
                    connection.handle_event(event);
                }
                return Ok(Output::Progress);
            }
            if let Some(event) = connection.poll() {
                match event {
                    Event::Connected => {
                        self.empty_send_space = connection.datagrams().send_buffer_space();
                        self.capacity = connection
                            .datagrams()
                            .max_size()
                            .ok_or(Error::Security)?
                            .min(self.empty_send_space);
                        if self.capacity < crate::wire::DATA_HEADER {
                            return Err(Error::Security);
                        }
                        self.connected = true;
                        return Ok(Output::Connected);
                    }
                    Event::ConnectionLost {
                        reason: noq_proto::ConnectionError::TimedOut,
                    } => return Err(Error::Timeout),
                    Event::ConnectionLost {
                        reason:
                            noq_proto::ConnectionError::ApplicationClosed(_)
                            | noq_proto::ConnectionError::LocallyClosed,
                    } => return Ok(Output::Closed),
                    Event::ConnectionLost { .. } | Event::Stream(_) => return Err(Error::Security),
                    _ => return Ok(Output::Progress),
                }
            }
            if self.connected
                && let Some(bytes) = connection.datagrams().recv()
            {
                let destination = output.get_mut(..bytes.len()).ok_or(Error::Capacity)?;
                destination.copy_from_slice(&bytes);
                return Ok(Output::ApplicationData(destination));
            }
            let before = connection.datagrams().send_buffer_space();
            self.buffer.clear();
            if let Some(transmit) =
                connection.poll_transmit(self.now, std::num::NonZeroUsize::MIN, &mut self.buffer)
            {
                if transmit.size > UDP_BUDGET || transmit.segment_size.is_some() {
                    return Err(Error::Capacity);
                }
                self.last_application = connection.datagrams().send_buffer_space() > before;
                let destination = output.get_mut(..transmit.size).ok_or(Error::Capacity)?;
                destination
                    .copy_from_slice(self.buffer.get(..transmit.size).ok_or(Error::Capacity)?);
                return Ok(Output::Datagram(destination));
            }
            if before < self.empty_send_space {
                self.stats.inner_stall_polls = self.stats.inner_stall_polls.saturating_add(1);
            }
            return Ok(Output::Timeout(
                connection
                    .poll_timeout()
                    .unwrap_or(self.now.checked_add(IDLE).ok_or(Error::Time)?),
            ));
        }
        Ok(Output::Timeout(
            self.now.checked_add(IDLE).ok_or(Error::Time)?,
        ))
    }
    pub(crate) fn stats(&self) -> QuicStats {
        self.stats
    }

    pub(crate) fn last_application(&self) -> bool {
        self.last_application
    }
    pub(crate) fn frame_capacity(&self) -> usize {
        self.capacity
    }
    pub(crate) fn close(&mut self) {
        self.closed = true;
        self.connection = None;
        self.response = None;
        self.stats.queued_payload_bytes = 0;
    }
}

#[allow(
    clippy::disallowed_types,
    reason = "Noq DATAGRAM API requires Bytes; copied once into single-owner backend staging, never shared across shards or cloned"
)]
fn owned_datagram(bytes: &[u8]) -> bytes::Bytes {
    bytes::Bytes::copy_from_slice(bytes)
}
