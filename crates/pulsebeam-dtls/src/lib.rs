//! Caller-driven, mutually pinned DTLS datagram and DTLS-SRTP profiles.
//!
//! Feed only the selected peer path. Drain [`Association::poll_output`] through
//! [`Output::Timeout`] after each operation. Address changes require a new
//! association. Certificate pins must come from authenticated provisioning.
#![forbid(unsafe_code)]

use std::{fmt, time::Instant};

use aws_lc_rs::digest::{SHA256, digest};
use dimpl::{Config, Dtls, DtlsCertificate};

pub use dimpl::{KeyingMaterial, ProtocolVersion, SrtpProfile};

/// Version policy for the explicitly selected DTLS-SRTP profile.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SrtpVersion {
    #[default]
    Auto,
    Dtls12,
    Dtls13,
}

#[derive(Clone, Copy)]
enum Profile {
    Datagrams,
    SingleRecordDatagrams,
    DtlsSrtp(SrtpVersion),
    FabricWan,
}

impl Profile {
    fn exports_srtp(self) -> bool {
        matches!(self, Self::DtlsSrtp(_) | Self::FabricWan)
    }
}

/// Pinned DTLS 1.3 AES-128-GCM profile: five header bytes (no CID), one
/// inner content-type byte and a sixteen-byte tag. Dimpl 0.9.0 adds no padding.
pub const SINGLE_RECORD_OVERHEAD: usize = 5 + 1 + 16;
/// Complete UDP payload at the IPv6 minimum MTU, before any extra path overhead.
pub const IPV6_MIN_MTU_UDP_PAYLOAD_BYTES: usize = 1_280 - 40 - 8;
pub const FABRIC_WAN_DATAGRAM_BYTES: usize = IPV6_MIN_MTU_UDP_PAYLOAD_BYTES;
pub const FABRIC_WAN_BOOTSTRAP_BYTES: usize = 1200;

struct SingleRecordState {
    mtu: usize,
    drained: bool,
    expected: Option<usize>,
}

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
#[derive(Clone)]
pub struct LocalIdentity(DtlsCertificate);

impl LocalIdentity {
    /// Generate an ECDSA P-256 identity using dimpl's AWS-LC-backed generator.
    pub fn generate() -> Result<Self, Error> {
        Ok(Self(dimpl::certificate::generate_self_signed_certificate()?))
    }

    /// Import a caller-owned P-256 identity in DER certificate/PKCS#8 form.
    /// The caller supplies a matching certificate and fresh private key.
    pub fn from_der(certificate: Vec<u8>, private_key: Vec<u8>) -> Result<Self, Error> {
        if certificate.is_empty()
            || aws_lc_rs::signature::EcdsaKeyPair::from_pkcs8(
                &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
                &private_key,
            )
            .is_err()
        {
            return Err(Error::InvalidIdentity);
        }
        Ok(Self(DtlsCertificate {
            certificate,
            private_key,
        }))
    }

    pub fn certificate_der(&self) -> &[u8] {
        &self.0.certificate
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
    #[error("invalid local certificate or private key")]
    InvalidIdentity,
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
    #[error("datagram exceeds the configured single-record budget")]
    DatagramBudget,
    #[error("drain DTLS output before sending another application record")]
    DrainRequired,
    #[error("datagram violates the single-record profile")]
    DatagramShape,
    #[error("the Fabric WAN association did not negotiate AES-256-GCM SRTP")]
    WanSrtpProfile,
    #[error("association is closed")]
    Closed,
}

/// One output per poll. Continue polling until `Timeout` or `Closed`.
pub enum Output<'a> {
    Datagram(&'a [u8]),
    ApplicationData(&'a [u8]),
    /// The handshake and the expected certificate pin have both been verified.
    Connected,
    /// Authenticated DTLS-SRTP exporter output. Never emitted by the datagram profile.
    KeyingMaterial(KeyingMaterial, SrtpProfile),
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
            Self::KeyingMaterial(_, profile) => {
                f.debug_tuple("KeyingMaterial").field(profile).finish()
            }
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

/// One mutually pinned association, with an explicit datagram or SRTP profile.
///
/// A protocol error, pin mismatch or premature inbound application data destroys
/// the engine. Local readiness/time errors do not destroy it. Dropping
/// or closing an association does not guarantee notification of the peer.
pub struct Association {
    inner: Option<Dtls>,
    expected_peer: Fingerprint,
    peer_verified: bool,
    handshake_complete: bool,
    profile: Profile,
    single_record: Option<SingleRecordState>,
    wan_application_wire_bytes: Option<usize>,
    srtp_material: Option<(KeyingMaterial, SrtpProfile)>,
    srtp_profile: Option<SrtpProfile>,
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
fn config(
    mtu: usize,
    key_limit: Option<u64>,
    profile: Profile,
) -> Result<std::sync::Arc<Config>, dimpl::Error> {
    let mut builder = Config::builder()
        .with_crypto_provider(dimpl::crypto::aws_lc_rs::default_provider())
        .require_client_certificate(true)
        .mtu(mtu)
        .max_queue_rx(32)
        .max_queue_tx(32);
    match profile {
        Profile::Datagrams => builder = builder.dtls12_cipher_suites(&[]),
        Profile::SingleRecordDatagrams | Profile::FabricWan => {
            builder = builder
                .dtls12_cipher_suites(&[])
                .dtls13_cipher_suites(&[dimpl::crypto::Dtls13CipherSuite::AES_128_GCM_SHA256]);
        }
        Profile::DtlsSrtp(_) => {
            use dimpl::crypto::Dtls13CipherSuite as Suite;
            builder = builder.use_server_cookie(false).dtls13_cipher_suites(&[
                Suite::AES_256_GCM_SHA384,
                Suite::AES_128_GCM_SHA256,
                Suite::CHACHA20_POLY1305_SHA256,
            ]);
        }
    }
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
        Self::with_profile(
            role,
            local,
            expected_peer,
            mtu,
            now,
            None,
            Profile::Datagrams,
        )
    }

    /// Explicitly provisioned DTLS 1.3 profile for one application record per
    /// protected datagram. Unlike `new`, this enforces the application budget.
    pub fn single_record_datagrams(
        role: Role,
        local: &LocalIdentity,
        expected_peer: Fingerprint,
        mtu: usize,
        now: Instant,
    ) -> Result<Self, Error> {
        Self::with_profile(
            role,
            local,
            expected_peer,
            mtu,
            now,
            None,
            Profile::SingleRecordDatagrams,
        )
    }

    /// WebRTC-compatible DTLS-SRTP with a purpose-specific authenticated exporter.
    pub fn dtls_srtp(
        role: Role,
        local: &LocalIdentity,
        expected_peer: Fingerprint,
        mtu: usize,
        now: Instant,
        version: SrtpVersion,
    ) -> Result<Self, Error> {
        Self::with_profile(
            role,
            local,
            expected_peer,
            mtu,
            now,
            None,
            Profile::DtlsSrtp(version),
        )
    }

    /// Provision the dedicated Fabric WAN profile. Ordinary DTLS output stays
    /// within the bootstrap MTU; single application records can carry probes
    /// up to the WAN envelope. Route and probe authority are caller-owned.
    pub fn fabric_wan(
        role: Role,
        local: &LocalIdentity,
        expected_peer: Fingerprint,
        bootstrap_mtu: usize,
        now: Instant,
    ) -> Result<Self, Error> {
        if bootstrap_mtu > FABRIC_WAN_BOOTSTRAP_BYTES {
            return Err(Error::DatagramBudget);
        }
        Self::with_profile(
            role,
            local,
            expected_peer,
            bootstrap_mtu,
            now,
            None,
            Profile::FabricWan,
        )
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
        Self::with_profile(
            role,
            local,
            expected_peer,
            mtu,
            now,
            Some(limit),
            Profile::Datagrams,
        )
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn single_record_with_test_key_limit(
        role: Role,
        local: &LocalIdentity,
        expected_peer: Fingerprint,
        mtu: usize,
        now: Instant,
        limit: u64,
    ) -> Result<Self, Error> {
        Self::with_profile(
            role,
            local,
            expected_peer,
            mtu,
            now,
            Some(limit),
            Profile::SingleRecordDatagrams,
        )
    }

    fn with_profile(
        role: Role,
        local: &LocalIdentity,
        expected_peer: Fingerprint,
        mtu: usize,
        now: Instant,
        key_limit: Option<u64>,
        profile: Profile,
    ) -> Result<Self, Error> {
        let wan = matches!(profile, Profile::FabricWan);
        let single_record = if wan || matches!(profile, Profile::SingleRecordDatagrams) {
            if mtu <= SINGLE_RECORD_OVERHEAD || mtu > usize::from(u16::MAX) {
                return Err(Error::DatagramBudget);
            }
            Some(SingleRecordState {
                mtu,
                drained: false,
                expected: None,
            })
        } else {
            None
        };
        // Only static certificate credentials are copied, never live traffic state.
        let config = config(mtu, key_limit, profile)?;
        let mut inner = match profile {
            Profile::Datagrams
            | Profile::SingleRecordDatagrams
            | Profile::FabricWan
            | Profile::DtlsSrtp(SrtpVersion::Dtls13) => Dtls::new_13(config, local.0.clone(), now),
            Profile::DtlsSrtp(SrtpVersion::Dtls12) => Dtls::new_12(config, local.0.clone(), now),
            Profile::DtlsSrtp(SrtpVersion::Auto) => Dtls::new_auto(config, local.0.clone(), now),
        };
        inner.set_active(role == Role::Client);
        Ok(Self {
            inner: Some(inner),
            expected_peer,
            peer_verified: false,
            handshake_complete: false,
            profile,
            single_record,
            wan_application_wire_bytes: None,
            srtp_material: None,
            srtp_profile: None,
            now,
        })
    }

    pub fn is_connected(&self) -> bool {
        self.inner.is_some()
            && self.peer_verified
            && self.handshake_complete
            && (!self.profile.exports_srtp() || self.srtp_profile.is_some())
    }

    pub fn protocol_version(&self) -> Option<ProtocolVersion> {
        self.inner.as_ref().and_then(Dtls::protocol_version)
    }

    pub fn negotiated_srtp_profile(&self) -> Option<SrtpProfile> {
        self.is_connected().then_some(self.srtp_profile).flatten()
    }

    /// Queue a graceful close-notify, then drain output. `close` remains immediate.
    pub fn shutdown(&mut self) -> Result<(), Error> {
        self.note_input();
        let result = self.inner.as_mut().ok_or(Error::Closed)?.close();
        self.protocol_result(result)
    }

    /// Feed a UDP payload from the selected peer after advancing time and draining.
    pub fn handle_packet(&mut self, datagram: &[u8]) -> Result<(), Error> {
        if let Some(state) = &self.single_record {
            let wan = matches!(self.profile, Profile::FabricWan);
            if wan && !state.drained {
                return Err(Error::DrainRequired);
            }
            if datagram.len() > state.mtu
                && (!wan
                    || datagram.len() > FABRIC_WAN_DATAGRAM_BYTES
                    || Dtls::single_unified_record_len(datagram).is_none())
            {
                return Err(Error::DatagramBudget);
            }
        }
        self.note_input();
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
        self.note_input();
        let result = self
            .inner
            .as_mut()
            .ok_or(Error::Closed)?
            .handle_timeout(now);
        self.now = now;
        self.protocol_result(result)
    }

    fn note_input(&mut self) {
        if let Some(state) = &mut self.single_record {
            state.drained = false;
        }
        self.wan_application_wire_bytes = None;
    }

    /// Queue one application datagram without splitting. The single-record
    /// profile also enforces its protected byte limit before queueing.
    pub fn send_application_data(&mut self, data: &[u8]) -> Result<(), Error> {
        self.send_application_record(data, false)
    }

    /// Queue the dedicated full-envelope probe, not ordinary control data.
    pub fn send_fabric_wan_probe(&mut self, data: &[u8]) -> Result<(), Error> {
        if self.inner.is_none() {
            return Err(Error::Closed);
        }
        if !matches!(self.profile, Profile::FabricWan)
            || data.len() != FABRIC_WAN_DATAGRAM_BYTES - SINGLE_RECORD_OVERHEAD
        {
            return Err(Error::DatagramBudget);
        }
        self.send_application_record(data, true)
    }

    fn send_application_record(&mut self, data: &[u8], full_probe: bool) -> Result<(), Error> {
        if self.inner.is_none() {
            return Err(Error::Closed);
        }
        if !self.is_connected() {
            return Err(Error::NotConnected);
        }
        if let Some(state) = &mut self.single_record {
            let wire = data.len().saturating_add(SINGLE_RECORD_OVERHEAD);
            let limit = if full_probe {
                FABRIC_WAN_DATAGRAM_BYTES
            } else {
                state.mtu
            };
            if wire > limit {
                return Err(Error::DatagramBudget);
            }
            if !state.drained {
                return Err(Error::DrainRequired);
            }
            state.drained = false;
            state.expected = Some(wire);
        }
        let result = self
            .inner
            .as_mut()
            .ok_or(Error::Closed)?
            .send_application_data(data)
            .map(|_| ());
        self.protocol_result(result)
    }

    /// The exact DTLS datagram length for the immediately preceding Fabric WAN
    /// application output. A later poll clears this observation.
    pub fn last_wan_application_wire_bytes(&self) -> Option<usize> {
        self.wan_application_wire_bytes
    }

    pub fn poll_output<'a>(&mut self, buffer: &'a mut [u8]) -> Result<Output<'a>, Error> {
        self.wan_application_wire_bytes = None;
        if self.is_connected()
            && let Some((material, profile)) = self.srtp_material.take()
        {
            return Ok(Output::KeyingMaterial(material, profile));
        }
        let was_connected = self.is_connected();
        let Some(inner) = self.inner.as_mut() else {
            return Ok(Output::Closed);
        };
        match inner.poll_output(buffer) {
            dimpl::Output::Packet(bytes) => {
                if let Some(state) = &mut self.single_record {
                    let records = inner.last_output_application_records();
                    let wan_application =
                        matches!(self.profile, Profile::FabricWan) && records != 0;
                    let limit = if wan_application {
                        FABRIC_WAN_DATAGRAM_BYTES
                    } else {
                        state.mtu
                    };
                    if bytes.len() > limit
                        || (wan_application && Dtls::single_unified_record_len(bytes).is_none())
                        || (records != 0
                            && (records != 1 || state.expected.take() != Some(bytes.len())))
                    {
                        self.close();
                        return Err(Error::DatagramShape);
                    }
                }
                return Ok(Output::Datagram(bytes));
            }
            dimpl::Output::ApplicationData(bytes) => {
                let record = inner.last_input_application_datagram_len();
                if !self.is_connected() {
                    self.close();
                    return Err(Error::NotConnected);
                }
                if matches!(self.profile, Profile::FabricWan) {
                    let expected = bytes.len().saturating_add(SINGLE_RECORD_OVERHEAD);
                    if record != Some(expected) {
                        self.close();
                        return Err(Error::DatagramShape);
                    }
                    self.wan_application_wire_bytes = record;
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
            dimpl::Output::Timeout(at) => {
                if let Some(state) = &mut self.single_record {
                    if state.expected.is_some() {
                        self.close();
                        return Err(Error::DatagramShape);
                    }
                    state.drained = true;
                }
                return Ok(Output::Timeout(at));
            }
            dimpl::Output::BufferTooSmall { needed } => {
                return Ok(Output::BufferTooSmall { needed });
            }
            dimpl::Output::KeyingMaterial(material, profile) => {
                if self.profile.exports_srtp() {
                    if matches!(self.profile, Profile::FabricWan)
                        && profile != SrtpProfile::AEAD_AES_256_GCM
                    {
                        self.close();
                        return Err(Error::WanSrtpProfile);
                    }
                    if self.srtp_profile.is_some() {
                        self.close();
                        return Err(Error::UnsupportedOutput);
                    }
                    self.srtp_profile = Some(profile);
                    self.srtp_material = Some((material, profile));
                }
            }
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
        self.single_record = None;
        self.wan_application_wire_bytes = None;
        self.peer_verified = false;
        self.handshake_complete = false;
        self.srtp_material = None;
        self.srtp_profile = None;
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
