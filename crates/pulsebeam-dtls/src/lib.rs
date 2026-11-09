//! Caller-driven, mutually pinned DTLS 1.3 datagrams using dimpl and AWS-LC.
//!
//! Feed only the selected peer path. Drain [`Association::poll_output`] through
//! [`Output::Timeout`] after each operation. Address changes require a new
//! association. Certificate pins must come from authenticated provisioning.
#![forbid(unsafe_code)]

use std::{fmt, time::Instant};

use aws_lc_rs::digest::{SHA256, digest};
use dimpl::{Config, Dtls, DtlsCertificate};

/// Public SHA-256 fingerprint of the entire DER leaf certificate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn to_bytes(self) -> [u8; 32] {
        self.0
    }

    fn of_certificate(certificate: &[u8]) -> Self {
        let mut bytes = [0; 32];
        bytes.copy_from_slice(digest(&SHA256, certificate).as_ref());
        Self(bytes)
    }
}

/// Local certificate and private signing key. There is no private-key accessor.
///
/// Reuse this identity across associations to keep its provisioned pin stable.
/// Each association has independent handshake and traffic-key state.
pub struct LocalIdentity(DtlsCertificate);

impl LocalIdentity {
    /// Generate an ECDSA P-256 identity using dimpl's AWS-LC-backed generator.
    pub fn generate() -> Result<Self, Error> {
        Ok(Self(dimpl::certificate::generate_self_signed_certificate()?))
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of_certificate(&self.0.certificate)
    }
}

impl fmt::Debug for LocalIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalIdentity")
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Client,
    Server,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("identity generation failed: {0}")]
    Identity(#[from] dimpl::certificate::CertificateError),
    #[error("DTLS failed: {0}")]
    Protocol(#[from] dimpl::Error),
    #[error("peer certificate does not match the provisioned fingerprint")]
    PeerMismatch,
    #[error("application traffic requires a completed, pinned handshake")]
    NotConnected,
    #[error("caller time moved backwards")]
    TimeWentBackwards,
    #[error("unsupported DTLS output event")]
    UnsupportedOutput,
    #[error("association is closed")]
    Closed,
}

/// One output per poll. Continue polling until `Timeout` or `Closed`.
pub enum Output<'a> {
    Datagram(&'a [u8]),
    ApplicationData(&'a [u8]),
    /// The handshake and the expected certificate pin have both been verified.
    Connected,
    Timeout(Instant),
    BufferTooSmall {
        needed: usize,
    },
    /// Internal progress, with no application-visible data.
    Progress,
    Closed,
}

impl fmt::Debug for Output<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Datagram(bytes) => f.debug_tuple("Datagram").field(&bytes.len()).finish(),
            Self::ApplicationData(bytes) => f
                .debug_tuple("ApplicationData")
                .field(&bytes.len())
                .finish(),
            Self::Connected => f.write_str("Connected"),
            Self::Timeout(at) => f.debug_tuple("Timeout").field(at).finish(),
            Self::BufferTooSmall { needed } => f
                .debug_struct("BufferTooSmall")
                .field("needed", needed)
                .finish(),
            Self::Progress => f.write_str("Progress"),
            Self::Closed => f.write_str("Closed"),
        }
    }
}

/// One single-path DTLS 1.3 association. No protocol or provider selector exists.
///
/// A protocol error, pin mismatch or premature inbound application data destroys
/// the engine. Local readiness/time errors do not destroy it. Dropping
/// or closing an association does not guarantee notification of the peer.
pub struct Association {
    inner: Option<Dtls>,
    expected_peer: Fingerprint,
    peer_verified: bool,
    handshake_complete: bool,
    now: Instant,
}

impl fmt::Debug for Association {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Association")
            .field("connected", &self.is_connected())
            .field("closed", &self.inner.is_none())
            .finish_non_exhaustive()
    }
}

#[allow(
    clippy::disallowed_types,
    reason = "dimpl requires Arc<Config>; this is immutable, cold-path configuration, not shared packet state."
)]
fn config(mtu: usize, key_limit: Option<u64>) -> Result<std::sync::Arc<Config>, dimpl::Error> {
    let mut builder = Config::builder()
        .with_crypto_provider(dimpl::crypto::aws_lc_rs::default_provider())
        .dtls12_cipher_suites(&[])
        .require_client_certificate(true)
        .mtu(mtu)
        .max_queue_rx(32)
        .max_queue_tx(32);
    if let Some(limit) = key_limit {
        builder = builder.aead_encryption_limit(limit);
    }
    Ok(std::sync::Arc::new(builder.build()?))
}

impl Association {
    /// Forward the caller-selected MTU to dimpl for packetization.
    /// Application sizing and fragmentation remain the caller's responsibility.
    pub fn new(
        role: Role,
        local: &LocalIdentity,
        expected_peer: Fingerprint,
        mtu: usize,
        now: Instant,
    ) -> Result<Self, Error> {
        Self::with_key_limit(role, local, expected_peer, mtu, now, None)
    }

    /// Exercise automatic key rotation with real cryptography and a reduced
    /// record-use limit. Production construction retains the upstream default.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_test_key_limit(
        role: Role,
        local: &LocalIdentity,
        expected_peer: Fingerprint,
        mtu: usize,
        now: Instant,
        limit: u64,
    ) -> Result<Self, Error> {
        Self::with_key_limit(role, local, expected_peer, mtu, now, Some(limit))
    }

    fn with_key_limit(
        role: Role,
        local: &LocalIdentity,
        expected_peer: Fingerprint,
        mtu: usize,
        now: Instant,
        key_limit: Option<u64>,
    ) -> Result<Self, Error> {
        // Only static certificate credentials are copied, never live traffic state.
        let mut inner = Dtls::new_13(config(mtu, key_limit)?, local.0.clone(), now);
        inner.set_active(role == Role::Client);
        Ok(Self {
            inner: Some(inner),
            expected_peer,
            peer_verified: false,
            handshake_complete: false,
            now,
        })
    }

    pub fn is_connected(&self) -> bool {
        self.inner.is_some() && self.peer_verified && self.handshake_complete
    }

    /// Feed a UDP payload from the selected peer after advancing time and draining.
    pub fn handle_packet(&mut self, datagram: &[u8]) -> Result<(), Error> {
        let result = self
            .inner
            .as_mut()
            .ok_or(Error::Closed)?
            .handle_packet(datagram);
        self.protocol_result(result)
    }

    /// Advance virtual or real caller time, without reading a clock internally.
    pub fn handle_timeout(&mut self, now: Instant) -> Result<(), Error> {
        if now < self.now {
            return Err(Error::TimeWentBackwards);
        }
        let result = self
            .inner
            .as_mut()
            .ok_or(Error::Closed)?
            .handle_timeout(now);
        self.now = now;
        self.protocol_result(result)
    }

    /// Queue one caller-sized application datagram, without splitting or capping it.
    /// Drain output before the next call; dimpl errors remain fatal.
    pub fn send_application_data(&mut self, data: &[u8]) -> Result<(), Error> {
        if self.inner.is_none() {
            return Err(Error::Closed);
        }
        if !self.is_connected() {
            return Err(Error::NotConnected);
        }
        let result = self
            .inner
            .as_mut()
            .ok_or(Error::Closed)?
            .send_application_data(data)
            .map(|_| ());
        self.protocol_result(result)
    }

    pub fn poll_output<'a>(&mut self, buffer: &'a mut [u8]) -> Result<Output<'a>, Error> {
        let was_connected = self.is_connected();
        let Some(inner) = self.inner.as_mut() else {
            return Ok(Output::Closed);
        };
        match inner.poll_output(buffer) {
            dimpl::Output::Packet(bytes) => return Ok(Output::Datagram(bytes)),
            dimpl::Output::ApplicationData(bytes) => {
                if !self.is_connected() {
                    self.close();
                    return Err(Error::NotConnected);
                }
                return Ok(Output::ApplicationData(bytes));
            }
            dimpl::Output::PeerCert(der) => {
                if Fingerprint::of_certificate(der) != self.expected_peer {
                    self.close();
                    return Err(Error::PeerMismatch);
                }
                self.peer_verified = true;
            }
            dimpl::Output::Connected => self.handshake_complete = true,
            dimpl::Output::Timeout(at) => return Ok(Output::Timeout(at)),
            dimpl::Output::BufferTooSmall { needed } => {
                return Ok(Output::BufferTooSmall { needed });
            }
            dimpl::Output::KeyingMaterial(_, _) => return Ok(Output::Progress),
            dimpl::Output::CloseNotify => {
                self.close();
                return Ok(Output::Closed);
            }
            _ => {
                self.close();
                return Err(Error::UnsupportedOutput);
            }
        }
        Ok(if !was_connected && self.is_connected() {
            Output::Connected
        } else {
            Output::Progress
        })
    }

    /// Application records in the immediately preceding `Output::Datagram`.
    /// This local provenance excludes handshake, ACK and KeyUpdate records.
    /// Read before polling or mutating the association again.
    pub fn last_output_application_records(&self) -> usize {
        self.inner
            .as_ref()
            .map_or(0, Dtls::last_output_application_records)
    }

    /// Immediately discard local association state, without a peer-close guarantee.
    pub fn close(&mut self) {
        self.inner = None;
        self.peer_verified = false;
        self.handshake_complete = false;
    }

    fn protocol_result(&mut self, result: Result<(), dimpl::Error>) -> Result<(), Error> {
        result.map_err(|error| {
            self.close();
            Error::Protocol(error)
        })
    }
}

#[cfg(test)]
mod tests;
