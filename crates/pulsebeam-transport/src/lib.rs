#![forbid(unsafe_code)]

//! Authenticated, unreliable datagrams without sockets, tasks or clock reads.
//! Certificate-pinned DTLS 1.3 or Noq QUIC DATAGRAM, with outer feedback and SCReAM.

use std::net::SocketAddr;
use std::time::{Instant, SystemTime};

mod backend;
mod demux;
pub use demux::{PacketKind, classify};
mod quic;
mod service;
mod wire;
pub use quic::{QuicCredential, QuicPeer, QuicStats, QuicTuning};
pub use service::{Application, Datagram, Error, Health, SendOutcome, Staging, Transport};

pub const UDP_BUDGET: usize = wire::UDP_BUDGET;
pub const PAYLOAD_BUDGET: usize = wire::PAYLOAD_BUDGET;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Time(pub u64);

impl Time {
    pub const fn from_nanos(ticks: u64) -> Self {
        Self(ticks)
    }
    pub const fn as_nanos(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiveTime {
    pub monotonic: Time,
    /// Original host observation, unavailable when the host did not provide it.
    pub wall: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerFingerprint([u8; 32]);

impl PeerFingerprint {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    pub const fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

// Credentials are shared only during explicit, cold-path provisioning.
#[allow(
    clippy::disallowed_types,
    reason = "cold-path immutable certificate ownership, never packet-path refcounting"
)]
#[derive(Clone)]
pub struct Credential(std::sync::Arc<pulsebeam_dtls::LocalIdentity>);

impl Credential {
    #[allow(
        clippy::disallowed_types,
        reason = "cold-path immutable certificate provisioning"
    )]
    pub fn generate() -> Result<Self, Error> {
        pulsebeam_dtls::LocalIdentity::generate()
            .map(|identity| Self(std::sync::Arc::new(identity)))
            .map_err(|_| Error::Security)
    }

    pub fn fingerprint(&self) -> PeerFingerprint {
        PeerFingerprint(self.0.fingerprint().to_bytes())
    }
}

#[derive(Clone, Copy, Debug)]
pub enum NetworkClass {
    Lan,
    Wan,
}

#[derive(Clone, Copy, Debug)]
pub enum PeerRole {
    Initiator,
    Responder,
}

/// Fixed-address, certificate-pinned DTLS profile. Opening is explicit and does
/// not authorize unknown peers, enable migration, or enable application 0-RTT.
#[derive(Clone)]
pub struct Peer {
    pub endpoint: SocketAddr,
    pub credential: Credential,
    pub expected_peer: PeerFingerprint,
    pub role: PeerRole,
    pub class: NetworkClass,
}

impl Peer {
    fn dtls_role(&self) -> pulsebeam_dtls::Role {
        match self.role {
            PeerRole::Initiator => pulsebeam_dtls::Role::Client,
            PeerRole::Responder => pulsebeam_dtls::Role::Server,
        }
    }

    fn association(&self, now: Instant) -> Result<pulsebeam_dtls::Association, Error> {
        pulsebeam_dtls::Association::new(
            self.dtls_role(),
            &self.credential.0,
            pulsebeam_dtls::Fingerprint::from_bytes(self.expected_peer.0),
            UDP_BUDGET,
            now,
        )
        .map_err(|_| Error::Security)
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn association_with_test_key_limit(
        &self,
        now: Instant,
        limit: u64,
    ) -> Result<pulsebeam_dtls::Association, Error> {
        pulsebeam_dtls::Association::with_test_key_limit(
            self.dtls_role(),
            &self.credential.0,
            pulsebeam_dtls::Fingerprint::from_bytes(self.expected_peer.0),
            UDP_BUDGET,
            now,
            limit,
        )
        .map_err(|_| Error::Security)
    }
}

#[derive(Clone)]
pub enum PeerConfig {
    Dtls(Peer),
    Quic(QuicPeer),
}

impl From<Peer> for PeerConfig {
    fn from(peer: Peer) -> Self {
        Self::Dtls(peer)
    }
}

impl From<QuicPeer> for PeerConfig {
    fn from(peer: QuicPeer) -> Self {
        Self::Quic(peer)
    }
}

impl PeerConfig {
    pub fn expected_peer(&self) -> PeerFingerprint {
        match self {
            Self::Dtls(peer) => peer.expected_peer,
            Self::Quic(peer) => peer.expected_peer,
        }
    }
    pub fn endpoint(&self) -> SocketAddr {
        match self {
            Self::Dtls(peer) => peer.endpoint,
            Self::Quic(peer) => peer.endpoint,
        }
    }
    fn class(&self) -> NetworkClass {
        match self {
            Self::Dtls(peer) => peer.class,
            Self::Quic(peer) => peer.class,
        }
    }
    fn is_quic(&self) -> bool {
        matches!(self, Self::Quic(_))
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wire_tests;
