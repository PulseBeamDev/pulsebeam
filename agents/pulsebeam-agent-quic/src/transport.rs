use std::num::NonZeroUsize;
#[allow(
    clippy::disallowed_types,
    reason = "Noq and rustls accept shared configuration and clock providers"
)]
use std::sync::Arc;
use std::time::Instant;

use noq_proto::crypto::rustls::{HandshakeData, QuicClientConfig, QuicServerConfig};
use noq_proto::{
    ClientConfig, Connection, ConnectionHandle, Endpoint, EndpointConfig, SendDatagramError,
    ServerConfig, Transmit, TransportConfig,
};
use pulsebeam_proto::data_record::MAX_ENDPOINT_RECORD_BYTES;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{RootCertStore, time_provider::TimeProvider};

pub const ALPN: &[u8] = b"pulsebeam-rt-v1";
pub const UDP_PAYLOAD_BYTES: u16 = 1200;
const CONNECTION_ID_BYTES: usize = 8;
const DATA_ENVELOPE_BYTES: usize = 3;
const DATAGRAM_QUEUE_BYTES: usize = 8 * 1024;
const CONTROL_WINDOW: u32 = 32 * 1024;

#[derive(Debug)]
pub enum ConfigError {
    Tls(rustls::Error),
    QuicCipherSuite,
}

fn limits() -> TransportConfig {
    let mut config = TransportConfig::default();
    config
        .max_concurrent_bidi_streams(1u32.into())
        .max_concurrent_uni_streams(0u32.into())
        .stream_receive_window(CONTROL_WINDOW.into())
        .receive_window(CONTROL_WINDOW.into())
        .send_window(u64::from(CONTROL_WINDOW))
        .datagram_receive_buffer_size(Some(DATAGRAM_QUEUE_BYTES))
        .datagram_send_buffer_size(DATAGRAM_QUEUE_BYTES)
        .initial_mtu(UDP_PAYLOAD_BYTES)
        .min_mtu(UDP_PAYLOAD_BYTES)
        .mtu_discovery_config(None)
        .max_idle_timeout(Some(noq_proto::VarInt::from_u32(15_000).into()))
        .max_concurrent_multipath_paths(0)
        .enable_segmentation_offload(false);
    config
}

#[allow(
    clippy::disallowed_types,
    reason = "Noq requires shared immutable endpoint configuration"
)]
pub fn endpoint(server: Option<ServerConfig>) -> Result<Endpoint, noq_proto::ConfigError> {
    let mut config = EndpointConfig::default();
    config
        .supported_versions(vec![1])
        .cid_generator(Arc::new(|| {
            Box::new(noq_proto::RandomConnectionIdGenerator::new(
                CONNECTION_ID_BYTES,
            ))
        }))
        .max_udp_payload_size(UDP_PAYLOAD_BYTES)?;
    Ok(Endpoint::new(Arc::new(config), server.map(Arc::new), true))
}

#[allow(
    clippy::disallowed_types,
    reason = "rustls and Noq require shared immutable TLS and transport configuration"
)]
pub fn client_config(
    roots: RootCertStore,
    time: Arc<dyn TimeProvider>,
) -> Result<ClientConfig, ConfigError> {
    client_config_for_alpn(roots, time, ALPN)
}

#[allow(
    clippy::disallowed_types,
    reason = "rustls and Noq require shared immutable TLS and transport configuration"
)]
fn client_config_for_alpn(
    roots: RootCertStore,
    time: Arc<dyn TimeProvider>,
    alpn: &[u8],
) -> Result<ClientConfig, ConfigError> {
    let mut tls = rustls::ClientConfig::builder_with_details(
        Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        time,
    )
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(ConfigError::Tls)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];
    tls.enable_early_data = false;
    tls.resumption = rustls::client::Resumption::disabled();
    let crypto = QuicClientConfig::try_from(tls).map_err(|_| ConfigError::QuicCipherSuite)?;
    let mut config = ClientConfig::new(Arc::new(crypto));
    config.version(1).transport_config(Arc::new(limits()));
    Ok(config)
}

#[allow(
    clippy::disallowed_types,
    reason = "rustls and Noq require shared immutable TLS and transport configuration and a clock"
)]
pub fn server_config(
    certificates: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    tls_time: Arc<dyn TimeProvider>,
    token_time: Arc<dyn noq_proto::TimeSource>,
    incoming_limit: NonZeroUsize,
) -> Result<ServerConfig, ConfigError> {
    let mut tls = rustls::ServerConfig::builder_with_details(
        Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        tls_time,
    )
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(ConfigError::Tls)?
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .map_err(ConfigError::Tls)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;
    tls.send_tls13_tickets = 0;
    let crypto = QuicServerConfig::try_from(tls).map_err(|_| ConfigError::QuicCipherSuite)?;
    let mut config = ServerConfig::with_crypto(Arc::new(crypto));
    config
        .transport_config(Arc::new(limits()))
        .migration(true)
        .max_incoming(incoming_limit.get())
        .incoming_buffer_size(16 * 1024)
        .incoming_buffer_size_total(256 * 1024)
        .time_source(token_time);
    Ok(config)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Readiness {
    Handshaking,
    Closed,
    WrongAlpn,
    InsufficientDatagramBudget,
    Ready,
}

pub fn data_readiness(connection: &mut Connection) -> Readiness {
    if connection.is_closed() {
        return Readiness::Closed;
    }
    if connection.is_handshaking() {
        return Readiness::Handshaking;
    }
    let protocol = connection
        .crypto_session()
        .handshake_data()
        .and_then(|data| data.downcast::<HandshakeData>().ok())
        .and_then(|data| data.protocol);
    if protocol.as_deref() != Some(ALPN) {
        return Readiness::WrongAlpn;
    }
    if !connection
        .datagrams()
        .max_size()
        .is_some_and(|size| size.saturating_sub(DATA_ENVELOPE_BYTES) >= MAX_ENDPOINT_RECORD_BYTES)
    {
        return Readiness::InsufficientDatagramBudget;
    }
    Readiness::Ready
}

/// Service one endpoint event so the shard can bound the work in each turn.
pub fn forward_endpoint_event(
    endpoint: &mut Endpoint,
    handle: ConnectionHandle,
    connection: &mut Connection,
) -> bool {
    let Some(event) = connection.poll_endpoint_events() else {
        return false;
    };
    if let Some(event) = endpoint.handle_event(handle, event) {
        connection.handle_event(event);
    }
    true
}

/// Service the transport timer when due, using the same clock as packet I/O.
pub fn service_timeout(connection: &mut Connection, now: Instant) {
    if connection
        .poll_timeout()
        .is_some_and(|deadline| deadline <= now)
    {
        connection.handle_timeout(now);
    }
}

/// Produces at most one UDP datagram. The socket owner must flush or retire it
/// before polling again: Noq has already committed this transmit internally.
fn poll_packet(connection: &mut Connection, now: Instant, out: &mut Vec<u8>) -> Option<Transmit> {
    out.clear();
    connection.poll_transmit(now, NonZeroUsize::MIN, out)
}

/// Admit only what can fit without discarding a previously admitted payload.
/// Call only after the association and binding have both been admitted.
#[allow(
    clippy::disallowed_types,
    reason = "Noq takes owned Bytes for queued datagrams; this copies once after capacity preflight"
)]
fn try_send_datagram(connection: &mut Connection, bytes: &[u8]) -> Result<(), DatagramAdmission> {
    let mut datagrams = connection.datagrams();
    let limit = datagrams.max_size().ok_or(DatagramAdmission::Unsupported)?;
    if bytes.is_empty() || bytes.len() > limit {
        return Err(DatagramAdmission::Size);
    }
    if bytes.len() > datagrams.send_buffer_space() {
        return Err(DatagramAdmission::Full);
    }
    datagrams
        .send(bytes::Bytes::copy_from_slice(bytes), false)
        .map_err(|error| match error {
            SendDatagramError::UnsupportedByPeer | SendDatagramError::Disabled => {
                DatagramAdmission::Unsupported
            }
            SendDatagramError::TooLarge => DatagramAdmission::Size,
            SendDatagramError::Blocked(_) => DatagramAdmission::Full,
        })
}

#[derive(Debug, PartialEq, Eq)]
pub enum DatagramAdmission {
    Unsupported,
    Size,
    Full,
    ControlTurn,
}

/// Noq serializes DATAGRAM frames before STREAM frames. Keep only one
/// payload in that FIFO and reserve a payload-free turn between submissions.
#[derive(Default)]
pub struct PacketScheduler {
    turn: Turn,
}

#[derive(Default)]
enum Turn {
    #[default]
    Control,
    Payload,
    Queued {
        empty_space: usize,
    },
}

impl PacketScheduler {
    /// Call only after the association and binding have been admitted.
    pub fn offer(
        &mut self,
        connection: &mut Connection,
        bytes: &[u8],
    ) -> Result<(), DatagramAdmission> {
        match self.turn {
            Turn::Control => return Err(DatagramAdmission::ControlTurn),
            Turn::Queued { .. } => return Err(DatagramAdmission::Full),
            Turn::Payload => {}
        }
        let empty_space = connection.datagrams().send_buffer_space();
        try_send_datagram(connection, bytes)?;
        self.turn = Turn::Queued { empty_space };
        Ok(())
    }

    /// A returned transmit must be flushed before this connection is polled again.
    /// `control_runnable` denotes pending stream bytes eligible under flow control,
    /// not data still waiting for send credit or an application acknowledgment.
    pub fn poll(
        &mut self,
        connection: &mut Connection,
        now: Instant,
        out: &mut Vec<u8>,
        control_runnable: bool,
    ) -> Option<Transmit> {
        let packet = poll_packet(connection, now, out);
        match self.turn {
            Turn::Control if packet.is_some() || !control_runnable => self.turn = Turn::Payload,
            Turn::Queued { empty_space }
                if connection.datagrams().send_buffer_space() >= empty_space =>
            {
                self.turn = Turn::Control;
            }
            _ => {}
        }
        packet
    }
}

pub mod control;
pub mod socket;

#[cfg(test)]
mod tests;
