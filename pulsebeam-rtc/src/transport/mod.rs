mod dtls;
mod ice;
mod srtp;

use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::time::Instant;

use is::{Candidate, IceConnectionState, IceCreds};
use str0m::crypto::Fingerprint;
use str0m::crypto::dtls::DtlsCert;

use crate::{DtlsFingerprint, DtlsRole};

pub use dtls::DtlsError;
pub use ice::IceError;
pub use srtp::{RtpMetadata, SrtpError};

const MAX_EVENTS: usize = 256;
const MAX_TRANSMISSIONS: usize = 256;
const MAX_CANDIDATE_PAIRS: usize = 128;
const MAX_PENDING_DTLS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportState {
    Checking,
    Connecting,
    Connected,
    Draining,
    Closed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatagramKind {
    Stun,
    Dtls,
    Rtp,
    Rtcp,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TransportEvent {
    StateChanged(TransportState),
    IceStateChanged(IceConnectionState),
    Rtp {
        bytes: Vec<u8>,
        metadata: RtpMetadata,
    },
    Rtcp(Vec<u8>),
    Data(Vec<u8>),
    Closed,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TransportTransmit {
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub bytes: Vec<u8>,
    pub kind: DatagramKind,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TransportError {
    Closed,
    InvalidInput,
    NotDue,
    QueueFull,
    Configuration,
    Protocol,
    Crypto,
    Timeout,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Closed => "transport is closed",
            Self::InvalidInput => "invalid transport input",
            Self::NotDue => "transport deadline is not due",
            Self::QueueFull => "transport queue is full",
            Self::Configuration => "invalid transport configuration",
            Self::Protocol => "transport protocol error",
            Self::Crypto => "transport cryptographic failure",
            Self::Timeout => "transport timed out",
        })
    }
}

impl std::error::Error for TransportError {}

pub struct TransportConfig {
    pub local_ice: IceCreds,
    pub local_candidate: Candidate,
    pub remote_ice: IceCreds,
    pub remote_candidates: Box<[Candidate]>,
    pub certificate: DtlsCert,
    pub remote_fingerprint: DtlsFingerprint,
    pub dtls_role: DtlsRole,
    pub ice_controlling: bool,
    pub ice_tie_breaker: u64,
    pub max_candidate_pairs: usize,
    pub max_events: usize,
    pub max_transmissions: usize,
    rtp_payload_types: Box<[u8]>,
}

impl TransportConfig {
    pub fn new(
        local_ice: IceCreds,
        local_candidate: Candidate,
        remote_ice: IceCreds,
        remote_candidates: Box<[Candidate]>,
        certificate: DtlsCert,
        remote_fingerprint: DtlsFingerprint,
        dtls_role: DtlsRole,
    ) -> Self {
        Self {
            local_ice,
            local_candidate,
            remote_ice,
            remote_candidates,
            certificate,
            remote_fingerprint,
            dtls_role,
            ice_controlling: false,
            ice_tie_breaker: 1,
            max_candidate_pairs: MAX_CANDIDATE_PAIRS,
            max_events: MAX_EVENTS,
            max_transmissions: MAX_TRANSMISSIONS,
            rtp_payload_types: Box::new([]),
        }
    }

    pub fn with_ice_role(mut self, controlling: bool, tie_breaker: u64) -> Self {
        self.ice_controlling = controlling;
        self.ice_tie_breaker = tie_breaker;
        self
    }

    pub fn with_rtp_payload_types(mut self, payload_types: Box<[u8]>) -> Self {
        self.rtp_payload_types = payload_types;
        self
    }

    pub fn validate(&self) -> Result<(), TransportError> {
        if self.max_candidate_pairs == 0
            || self.max_events == 0
            || self.max_transmissions == 0
            || self.max_candidate_pairs > MAX_CANDIDATE_PAIRS
            || self.max_events > MAX_EVENTS
            || self.max_transmissions > MAX_TRANSMISSIONS
            || self.ice_tie_breaker == 0
            || self.remote_candidates.is_empty()
            || self.remote_candidates.len() > self.max_candidate_pairs
            || self.local_candidate.proto() != is::Protocol::Udp
            || self
                .remote_candidates
                .iter()
                .any(|c| c.proto() != is::Protocol::Udp)
            || self.remote_fingerprint.algorithm() != "sha-256"
            || self.remote_fingerprint.value().len() != 32
            || Self::validate_rtp_payload_types(&self.rtp_payload_types).is_err()
        {
            return Err(TransportError::Configuration);
        }
        Ok(())
    }

    pub fn validate_rtp_payload_types(payload_types: &[u8]) -> Result<(), TransportError> {
        if payload_types.len() > 128
            || payload_types
                .iter()
                .any(|payload_type| *payload_type > 127 || (64..=95).contains(payload_type))
        {
            return Err(TransportError::Configuration);
        }
        Ok(())
    }
}

pub struct Transport {
    local: SocketAddr,
    remote: Option<SocketAddr>,
    state: TransportState,
    last_now: Option<Instant>,
    ice: ice::IceLayer,
    dtls: Option<dtls::DtlsLayer>,
    srtp: Option<srtp::SrtpLayer>,
    certificate: Option<DtlsCert>,
    local_fingerprint: DtlsFingerprint,
    remote_fingerprint: Fingerprint,
    dtls_role: DtlsRole,
    rtp_payload_types: Box<[u8]>,
    events: VecDeque<TransportEvent>,
    transmissions: VecDeque<TransportTransmit>,
    pending_dtls: VecDeque<(SocketAddr, SocketAddr, Vec<u8>)>,
    next_deadline: Option<Instant>,
    max_events: usize,
    max_transmissions: usize,
}

impl Transport {
    pub fn new(config: TransportConfig, now: Instant) -> Result<Self, TransportError> {
        config.validate()?;
        let provider = str0m::crypto::from_feature_flags();
        let remote_fingerprint = Fingerprint {
            hash_func: config.remote_fingerprint.algorithm().to_owned(),
            bytes: config.remote_fingerprint.value().to_vec(),
        };
        let local_fingerprint = DtlsFingerprint::new(
            "sha-256".to_owned(),
            provider
                .sha256_provider
                .sha256(&config.certificate.certificate)
                .to_vec()
                .into_boxed_slice(),
        )
        .ok_or(TransportError::Configuration)?;
        let local = config.local_candidate.addr();
        let ice = ice::IceLayer::new(
            config.local_ice,
            config.local_candidate,
            config.remote_ice,
            &config.remote_candidates,
            config.ice_controlling,
            config.max_candidate_pairs,
            config.ice_tie_breaker,
        );
        let mut transport = Self {
            local,
            remote: None,
            state: TransportState::Checking,
            last_now: Some(now),
            ice,
            dtls: None,
            srtp: None,
            certificate: Some(config.certificate),
            local_fingerprint,
            remote_fingerprint,
            dtls_role: config.dtls_role,
            rtp_payload_types: config.rtp_payload_types,
            events: VecDeque::new(),
            transmissions: VecDeque::new(),
            pending_dtls: VecDeque::new(),
            next_deadline: None,
            max_events: config.max_events,
            max_transmissions: config.max_transmissions,
        };
        transport.push_event(TransportEvent::StateChanged(TransportState::Checking))?;
        transport
            .ice
            .handle_timeout(now)
            .map_err(|_| TransportError::Protocol)?;
        transport.drain_ice(now, &provider)?;
        Ok(transport)
    }

    pub fn state(&self) -> TransportState {
        self.state
    }

    pub fn local_fingerprint(&self) -> &DtlsFingerprint {
        &self.local_fingerprint
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        if matches!(
            self.state,
            TransportState::Connected | TransportState::Closed | TransportState::Failed
        ) {
            None
        } else {
            self.next_deadline
        }
    }

    pub fn poll_event(&mut self) -> Option<TransportEvent> {
        self.events.pop_front()
    }

    pub fn poll_transmit(&mut self) -> Option<TransportTransmit> {
        let item = self.transmissions.pop_front();
        debug_assert!(self.transmissions.len() <= self.max_transmissions);
        item
    }

    pub fn classify(bytes: &[u8]) -> Option<DatagramKind> {
        let first = *bytes.first()?;
        if bytes.len() >= 20
            && first & 0xc0 == 0
            && bytes.get(4..8) == Some(&[0x21, 0x12, 0xa4, 0x42])
        {
            return Some(DatagramKind::Stun);
        }
        if (20..=64).contains(&first) {
            return Some(DatagramKind::Dtls);
        }
        if first & 0xc0 != 0x80 {
            return None;
        }
        if bytes.len() < 4 {
            return None;
        }
        let second = *bytes.get(1)?;
        if (192..=223).contains(&second) {
            let length = usize::from(u16::from_be_bytes([*bytes.get(2)?, *bytes.get(3)?]))
                .checked_add(1)?
                .checked_mul(4)?;
            if length >= 8 && length <= bytes.len() {
                return Some(DatagramKind::Rtcp);
            }
            return None;
        }
        (bytes.len() >= 12).then_some(DatagramKind::Rtp)
    }

    pub fn handle_datagram(
        &mut self,
        now: Instant,
        source: SocketAddr,
        destination: SocketAddr,
        bytes: Vec<u8>,
    ) -> Result<(), TransportError> {
        self.observe(now)?;
        if bytes.is_empty() {
            return Err(TransportError::InvalidInput);
        }
        let Some(kind) = Self::classify(&bytes) else {
            return Ok(());
        };
        match kind {
            DatagramKind::Stun => {
                if destination != self.local {
                    return Ok(());
                }
                if self
                    .ice
                    .handle_packet(now, source, destination, &bytes)
                    .is_ok()
                {
                    let provider = str0m::crypto::from_feature_flags();
                    self.drain_ice(now, &provider)?;
                }
            }
            DatagramKind::Dtls => {
                let provider = str0m::crypto::from_feature_flags();
                self.drain_ice(now, &provider)?;
                if !self.ice.accepts_tuple(source, destination) {
                    if self.ice.can_queue_tuple(source, destination)
                        && self.pending_dtls.len() < MAX_PENDING_DTLS
                    {
                        self.pending_dtls.push_back((source, destination, bytes));
                    }
                    return Ok(());
                }
                self.handle_dtls_packet(now, source, destination, bytes, &provider)?;
            }
            DatagramKind::Rtp | DatagramKind::Rtcp => {
                if self.state != TransportState::Connected
                    || !self.ice.accepts_tuple(source, destination)
                {
                    return Ok(());
                }
                let Some(srtp) = self.srtp.as_mut() else {
                    return Ok(());
                };
                match kind {
                    DatagramKind::Rtp => match srtp.unprotect_rtp(&bytes) {
                        Ok((bytes, metadata)) => {
                            if self.accepts_payload_type(metadata.payload_type) {
                                self.push_event(TransportEvent::Rtp { bytes, metadata })?;
                            }
                        }
                        Err(SrtpError::Replay | SrtpError::InvalidPacket) => {}
                        Err(SrtpError::Crypto) => self.fail(TransportError::Crypto)?,
                        Err(SrtpError::OutputFull | SrtpError::UnsupportedProfile) => {
                            self.fail(TransportError::Protocol)?;
                        }
                    },
                    DatagramKind::Rtcp => match srtp.unprotect_rtcp(&bytes) {
                        Ok(bytes) => self.push_event(TransportEvent::Rtcp(bytes))?,
                        Err(SrtpError::Replay | SrtpError::InvalidPacket) => {}
                        Err(SrtpError::Crypto) => self.fail(TransportError::Crypto)?,
                        Err(SrtpError::OutputFull | SrtpError::UnsupportedProfile) => {
                            self.fail(TransportError::Protocol)?;
                        }
                    },
                    _ => {}
                }
            }
        }
        Ok(())
    }

    pub fn handle_timeout(&mut self, now: Instant) -> Result<(), TransportError> {
        self.observe(now)?;
        let Some(deadline) = self.next_deadline else {
            return Err(TransportError::NotDue);
        };
        if now < deadline {
            return Err(TransportError::NotDue);
        }
        self.ice
            .handle_timeout(now)
            .map_err(|_| TransportError::Protocol)?;
        if let Some(dtls) = self.dtls.as_mut()
            && dtls.next_deadline().is_some_and(|value| now >= value)
            && let Err(error) = dtls.handle_timeout(now)
        {
            self.fail(error_to_transport(error))?;
        }
        let provider = str0m::crypto::from_feature_flags();
        self.drain_ice(now, &provider)?;
        self.drain_dtls(now)
    }

    pub fn send_rtp(&mut self, packet: &[u8]) -> Result<(), TransportError> {
        self.send_secure(packet, DatagramKind::Rtp)
    }

    pub fn send_rtcp(&mut self, packet: &[u8]) -> Result<(), TransportError> {
        self.send_secure(packet, DatagramKind::Rtcp)
    }

    pub fn close(&mut self, now: Instant) -> Result<(), TransportError> {
        if matches!(self.state, TransportState::Closed | TransportState::Failed) {
            return Err(TransportError::Closed);
        }
        self.transmissions.clear();
        self.events.clear();
        self.pending_dtls.clear();
        if let Some(dtls) = self.dtls.as_mut() {
            dtls.clear_pending();
            dtls.close(now).map_err(error_to_transport)?;
            self.drain_dtls(now)?;
        }
        self.dtls = None;
        self.srtp = None;
        self.next_deadline = None;
        if self.state != TransportState::Closed {
            self.state = TransportState::Closed;
            self.push_event(TransportEvent::Closed)?;
        }
        Ok(())
    }

    fn send_secure(&mut self, packet: &[u8], kind: DatagramKind) -> Result<(), TransportError> {
        if self.state != TransportState::Connected {
            return Err(
                if matches!(self.state, TransportState::Closed | TransportState::Failed) {
                    TransportError::Closed
                } else {
                    TransportError::Protocol
                },
            );
        }
        let Some(remote) = self.remote else {
            return Err(TransportError::Protocol);
        };
        if kind == DatagramKind::Rtp {
            let parsed = crate::packet::RtpPacket::parse(packet)
                .map_err(|_| TransportError::InvalidInput)?;
            if !self.accepts_payload_type(parsed.payload_type()) {
                return Err(TransportError::InvalidInput);
            }
        }
        let Some(srtp) = self.srtp.as_mut() else {
            return Err(TransportError::Protocol);
        };
        let bytes = match kind {
            DatagramKind::Rtp => srtp.protect_rtp(packet),
            DatagramKind::Rtcp => srtp.protect_rtcp(packet),
            _ => Err(SrtpError::InvalidPacket),
        }
        .map_err(|_| TransportError::Crypto)?;
        self.enqueue_transmit(TransportTransmit {
            source: self.local,
            destination: remote,
            bytes,
            kind,
        })
    }

    fn drain_ice(
        &mut self,
        now: Instant,
        provider: &str0m::crypto::CryptoProvider,
    ) -> Result<(), TransportError> {
        while let Some((source, destination, bytes)) = self.ice.poll_transmit() {
            self.enqueue_transmit(TransportTransmit {
                source,
                destination,
                bytes,
                kind: DatagramKind::Stun,
            })?;
        }
        while let Some(event) = self.ice.poll_event() {
            match event {
                ice::IceEvent::StateChanged(state) => {
                    self.push_event(TransportEvent::IceStateChanged(state))?;
                    if state == IceConnectionState::Disconnected {
                        self.fail(TransportError::Timeout)?;
                    }
                }
                ice::IceEvent::Nominated {
                    source: _,
                    destination,
                } => {
                    self.remote = Some(destination);
                    self.state = TransportState::Connecting;
                    self.push_event(TransportEvent::StateChanged(TransportState::Connecting))?;
                    self.start_dtls(now, provider)?;
                }
                ice::IceEvent::Restart => {
                    if self.dtls.is_some() {
                        self.fail(TransportError::Protocol)?;
                    }
                    self.remote = None;
                    self.srtp = None;
                    self.pending_dtls.clear();
                    self.state = TransportState::Checking;
                    self.push_event(TransportEvent::StateChanged(TransportState::Checking))?;
                }
            }
        }
        self.drain_pending_dtls(now, provider)?;
        let ice_deadline = self.ice.next_deadline();
        let dtls_deadline = self.dtls.as_ref().and_then(dtls::DtlsLayer::next_deadline);
        self.next_deadline = match (ice_deadline, dtls_deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, None) | (None, left) => left,
        };
        Ok(())
    }

    fn start_dtls(
        &mut self,
        now: Instant,
        provider: &str0m::crypto::CryptoProvider,
    ) -> Result<(), TransportError> {
        if self.dtls.is_some() {
            return Ok(());
        }
        let certificate = self
            .certificate
            .take()
            .ok_or(TransportError::Configuration)?;
        let dtls = dtls::DtlsLayer::new(
            certificate,
            self.remote_fingerprint.clone(),
            self.dtls_role == DtlsRole::Active,
            now,
            provider,
        )
        .map_err(error_to_transport)?;
        let ice_deadline = self.ice.next_deadline();
        self.next_deadline = match (ice_deadline, dtls.next_deadline()) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, None) | (None, left) => left,
        };
        self.dtls = Some(dtls);
        self.drain_dtls(now)
    }

    fn drain_dtls(&mut self, _now: Instant) -> Result<(), TransportError> {
        let Some(dtls) = self.dtls.as_mut() else {
            return Ok(());
        };
        let mut packets = Vec::new();
        while let Some(bytes) = dtls.poll_packet() {
            packets.push(bytes);
        }
        let mut events = Vec::new();
        while let Some(event) = dtls.poll_event() {
            events.push(event);
        }
        let connected = dtls.connected();
        let next_deadline = dtls.next_deadline();
        let _ = dtls;
        let destination = self.remote.ok_or(TransportError::Protocol)?;
        for bytes in packets {
            self.enqueue_transmit(TransportTransmit {
                source: self.local,
                destination,
                bytes,
                kind: DatagramKind::Dtls,
            })?;
        }
        for event in events {
            match event {
                dtls::DtlsEvent::Connected => {
                    self.push_event(TransportEvent::StateChanged(TransportState::Connected))?;
                }
                dtls::DtlsEvent::KeyingMaterial(material, profile) => {
                    let provider = str0m::crypto::from_feature_flags();
                    self.srtp = Some(
                        srtp::SrtpLayer::new(
                            material,
                            profile,
                            self.dtls_role == DtlsRole::Active,
                            &provider,
                        )
                        .map_err(|_| TransportError::Crypto)?,
                    );
                }
                dtls::DtlsEvent::ApplicationData(data) => {
                    self.push_event(TransportEvent::Data(data))?;
                }
                dtls::DtlsEvent::CloseNotify => {
                    self.state = TransportState::Closed;
                    self.push_event(TransportEvent::Closed)?;
                }
            }
        }
        if connected && self.srtp.is_some() && self.state == TransportState::Connecting {
            self.state = TransportState::Connected;
        }
        let ice_deadline = self.ice.next_deadline();
        self.next_deadline = match (ice_deadline, next_deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, None) | (None, left) => left,
        };
        Ok(())
    }

    fn handle_dtls_packet(
        &mut self,
        now: Instant,
        source: SocketAddr,
        _destination: SocketAddr,
        bytes: Vec<u8>,
        provider: &str0m::crypto::CryptoProvider,
    ) -> Result<(), TransportError> {
        if self.dtls.is_none() {
            self.remote = Some(source);
            self.state = TransportState::Connecting;
            self.start_dtls(now, provider)?;
        }
        let Some(dtls) = self.dtls.as_mut() else {
            return Ok(());
        };
        if let Err(error) = dtls.handle_packet(&bytes, now) {
            self.fail(error_to_transport(error))?;
        }
        self.drain_dtls(now)
    }

    fn drain_pending_dtls(
        &mut self,
        now: Instant,
        provider: &str0m::crypto::CryptoProvider,
    ) -> Result<(), TransportError> {
        let Some(remote) = self.remote else {
            return Ok(());
        };
        let local = self.local;
        let mut pending = std::mem::take(&mut self.pending_dtls);
        while let Some((source, destination, bytes)) = pending.pop_front() {
            if source == remote
                && destination == local
                && self.ice.accepts_tuple(source, destination)
            {
                self.handle_dtls_packet(now, source, destination, bytes, provider)?;
            }
        }
        Ok(())
    }

    fn observe(&mut self, now: Instant) -> Result<(), TransportError> {
        if matches!(self.state, TransportState::Closed | TransportState::Failed) {
            return Err(TransportError::Closed);
        }
        if self.last_now.is_some_and(|previous| now < previous) {
            return Err(TransportError::InvalidInput);
        }
        self.last_now = Some(now);
        Ok(())
    }

    fn enqueue_transmit(&mut self, transmission: TransportTransmit) -> Result<(), TransportError> {
        if transmission.bytes.is_empty() || self.transmissions.len() >= self.max_transmissions {
            let _ = self.fail(TransportError::QueueFull);
            return Err(TransportError::QueueFull);
        }
        self.transmissions.push_back(transmission);
        debug_assert!(self.transmissions.len() <= self.max_transmissions);
        Ok(())
    }

    fn push_event(&mut self, event: TransportEvent) -> Result<(), TransportError> {
        if self.events.len() >= self.max_events {
            let _ = self.fail(TransportError::QueueFull);
            return Err(TransportError::QueueFull);
        }
        self.events.push_back(event);
        debug_assert!(self.events.len() <= self.max_events);
        Ok(())
    }

    fn fail(&mut self, error: TransportError) -> Result<(), TransportError> {
        self.state = TransportState::Failed;
        self.next_deadline = None;
        self.transmissions.clear();
        self.events.clear();
        self.pending_dtls.clear();
        self.dtls = None;
        self.srtp = None;
        self.certificate = None;
        self.events
            .push_back(TransportEvent::StateChanged(TransportState::Failed));
        Err(error)
    }

    fn accepts_payload_type(&self, payload_type: u8) -> bool {
        self.rtp_payload_types.is_empty() || self.rtp_payload_types.contains(&payload_type)
    }
}

fn error_to_transport(error: dtls::DtlsError) -> TransportError {
    match error {
        dtls::DtlsError::FingerprintMismatch | dtls::DtlsError::Crypto => TransportError::Crypto,
        dtls::DtlsError::BufferTooSmall | dtls::DtlsError::OutputFull => TransportError::QueueFull,
        dtls::DtlsError::InvalidState => TransportError::Protocol,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::time::Duration;

    fn address(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    fn certificate() -> DtlsCert {
        let provider = str0m::crypto::from_feature_flags();
        provider
            .dtls_provider
            .generate_certificate()
            .expect("test crypto provider generates certificates")
    }

    fn fingerprint(certificate: &DtlsCert) -> DtlsFingerprint {
        let provider = str0m::crypto::from_feature_flags();
        DtlsFingerprint::new(
            "sha-256".to_owned(),
            provider
                .sha256_provider
                .sha256(&certificate.certificate)
                .to_vec()
                .into_boxed_slice(),
        )
        .expect("sha-256 fingerprint")
    }

    fn config(
        local: SocketAddr,
        remote: SocketAddr,
        local_certificate: DtlsCert,
        remote_certificate: &DtlsCert,
        active: bool,
    ) -> TransportConfig {
        TransportConfig::new(
            IceCreds {
                ufrag: if active { "left" } else { "right" }.to_owned(),
                pass: if active {
                    "leftpasswordabcdefghijklmnop"
                } else {
                    "rightpasswordabcdefghijklmnop"
                }
                .to_owned(),
            },
            Candidate::host(local, is::Protocol::Udp).expect("local candidate"),
            IceCreds {
                ufrag: if active { "right" } else { "left" }.to_owned(),
                pass: if active {
                    "rightpasswordabcdefghijklmnop"
                } else {
                    "leftpasswordabcdefghijklmnop"
                }
                .to_owned(),
            },
            vec![Candidate::host(remote, is::Protocol::Udp).expect("remote candidate")]
                .into_boxed_slice(),
            local_certificate,
            fingerprint(remote_certificate),
            if active {
                DtlsRole::Active
            } else {
                DtlsRole::Passive
            },
        )
        .with_ice_role(active, if active { 2 } else { 1 })
    }

    fn connect(left: &mut Transport, right: &mut Transport, now: &mut Instant) {
        for _ in 0..400 {
            let mut progress = false;
            while let Some(transmit) = left.poll_transmit() {
                progress = true;
                right
                    .handle_datagram(*now, transmit.source, transmit.destination, transmit.bytes)
                    .expect("right accepts deterministic datagram");
            }
            while let Some(transmit) = right.poll_transmit() {
                progress = true;
                left.handle_datagram(*now, transmit.source, transmit.destination, transmit.bytes)
                    .expect("left accepts deterministic datagram");
            }
            if left.state() == TransportState::Connected
                && right.state() == TransportState::Connected
            {
                return;
            }
            if !progress {
                *now = now
                    .checked_add(Duration::from_millis(50))
                    .expect("deterministic test clock does not overflow");
                if left
                    .next_deadline()
                    .is_some_and(|deadline| deadline <= *now)
                {
                    left.handle_timeout(*now).expect("left timeout is due");
                }
                if right
                    .next_deadline()
                    .is_some_and(|deadline| deadline <= *now)
                {
                    right.handle_timeout(*now).expect("right timeout is due");
                }
            }
        }
        panic!("deterministic peers did not connect");
    }

    #[test]
    fn classification_is_unambiguous() {
        assert_eq!(
            Transport::classify(&[0, 1, 0, 0, 0x21, 0x12, 0xa4, 0x42]),
            None
        );
        assert_eq!(Transport::classify(&[0x16, 0, 0]), Some(DatagramKind::Dtls));
        assert_eq!(
            Transport::classify(&[0x80, 192, 0, 1, 0, 0, 0, 0]),
            Some(DatagramKind::Rtcp)
        );
        assert_eq!(
            Transport::classify(&[0x80, 200, 0, 1, 0, 0, 0, 0]),
            Some(DatagramKind::Rtcp)
        );
        assert_eq!(
            Transport::classify(&[0x80, 201, 0, 1, 0, 0, 0, 0]),
            Some(DatagramKind::Rtcp)
        );
        assert_eq!(
            Transport::classify(&[0x80, 203, 0, 1, 0, 0, 0, 0]),
            Some(DatagramKind::Rtcp)
        );
        assert_eq!(
            Transport::classify(&[0x80, 223, 0, 1, 0, 0, 0, 0]),
            Some(DatagramKind::Rtcp)
        );
        assert_eq!(
            Transport::classify(&[0x80, 224, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]),
            Some(DatagramKind::Rtp)
        );
        assert_eq!(Transport::classify(&[0x80, 224, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(Transport::classify(&[0x80, 224, 0, 1, 0, 0, 0, 0]), None);
        assert_eq!(
            Transport::classify(&[0x80, 224, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]),
            Some(DatagramKind::Rtp)
        );
        assert_eq!(Transport::classify(&[0xff; 12]), None);
        assert_eq!(Transport::classify(&[0x80]), None);
        assert_eq!(Transport::classify(&[0x80, 200]), None);
    }

    #[test]
    fn ambiguous_rtp_payload_types_are_rejected() {
        let left_certificate = certificate();
        let right_certificate = certificate();
        let valid_config = config(
            address(20000),
            address(20001),
            left_certificate,
            &right_certificate,
            true,
        )
        .with_rtp_payload_types(vec![63, 96].into_boxed_slice());
        assert!(valid_config.validate().is_ok());

        let invalid_config = config(
            address(20000),
            address(20001),
            certificate(),
            &right_certificate,
            true,
        )
        .with_rtp_payload_types(vec![64].into_boxed_slice());
        assert_eq!(
            invalid_config.validate(),
            Err(TransportError::Configuration)
        );

        let invalid_config = config(
            address(20000),
            address(20001),
            certificate(),
            &right_certificate,
            true,
        )
        .with_rtp_payload_types(vec![95].into_boxed_slice());
        assert_eq!(
            invalid_config.validate(),
            Err(TransportError::Configuration)
        );
        let valid_config = config(
            address(20000),
            address(20001),
            certificate(),
            &right_certificate,
            true,
        )
        .with_rtp_payload_types(vec![96].into_boxed_slice());
        assert!(valid_config.validate().is_ok());
    }

    #[test]
    fn secure_transport_connects_and_exchanges_protected_media() {
        let left_certificate = certificate();
        let right_certificate = certificate();
        let mut left = Transport::new(
            config(
                address(4000),
                address(4001),
                left_certificate.clone(),
                &right_certificate,
                true,
            ),
            Instant::now(),
        )
        .expect("left transport");
        let mut right = Transport::new(
            config(
                address(4001),
                address(4000),
                right_certificate,
                &left_certificate,
                false,
            ),
            Instant::now(),
        )
        .expect("right transport");
        let mut now = Instant::now();
        connect(&mut left, &mut right, &mut now);
        assert_eq!(left.next_deadline(), None);
        assert_eq!(right.next_deadline(), None);

        right
            .handle_datagram(
                now,
                address(4000),
                address(4001),
                vec![0x90, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7],
            )
            .expect("truncated clear RTP extension is ignored");
        assert!(
            !std::iter::from_fn(|| right.poll_event())
                .any(|event| matches!(event, TransportEvent::Rtp { .. }))
        );

        let mut rtp = vec![0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7, 1, 2, 3];
        left.send_rtp(&rtp).expect("left protects RTP");
        let transmit = left.poll_transmit().expect("protected RTP transmit");
        assert_eq!(transmit.kind, DatagramKind::Rtp);
        right
            .handle_datagram(now, transmit.source, transmit.destination, transmit.bytes)
            .expect("right handles RTP");
        let event = std::iter::from_fn(|| right.poll_event())
            .find(|event| matches!(event, TransportEvent::Rtp { .. }))
            .expect("authenticated RTP event");
        if let TransportEvent::Rtp { bytes, metadata } = event {
            assert_eq!(bytes, rtp);
            assert_eq!(metadata.ssrc, 7);
        }

        let padded_rtp = vec![
            0xa0, 96, 0xff, 0xff, 0, 0, 0, 1, 0, 0, 0, 8, 9, 8, 7, 0, 0, 3,
        ];
        left.send_rtp(&padded_rtp)
            .expect("left protects padded RTP");
        let transmit = left.poll_transmit().expect("padded RTP transmit");
        right
            .handle_datagram(now, transmit.source, transmit.destination, transmit.bytes)
            .expect("right handles padded RTP");
        assert!(std::iter::from_fn(|| right.poll_event()).any(
            |event| matches!(event, TransportEvent::Rtp { bytes, metadata } if bytes == padded_rtp && metadata.ssrc == 8)
        ));

        let mut wrapped_rtp = padded_rtp;
        wrapped_rtp[2..4].copy_from_slice(&0_u16.to_be_bytes());
        left.send_rtp(&wrapped_rtp)
            .expect("left protects post-wrap RTP");
        let transmit = left.poll_transmit().expect("post-wrap RTP transmit");
        right
            .handle_datagram(now, transmit.source, transmit.destination, transmit.bytes)
            .expect("right handles post-wrap RTP");
        assert!(std::iter::from_fn(|| right.poll_event()).any(
            |event| matches!(event, TransportEvent::Rtp { metadata, .. } if metadata.sequence == 0)
        ));

        let rtcp = vec![0x80, 200, 0, 1, 0, 0, 0, 7];
        left.send_rtcp(&rtcp).expect("left protects RTCP");
        let transmit = left.poll_transmit().expect("protected RTCP transmit");
        right
            .handle_datagram(now, transmit.source, transmit.destination, transmit.bytes)
            .expect("right handles RTCP");
        assert!(
            std::iter::from_fn(|| right.poll_event())
                .any(|event| matches!(event, TransportEvent::Rtcp(value) if value == rtcp))
        );

        rtp[3] = 1;
        left.send_rtp(&rtp).expect("second RTP");
        let transmit = left.poll_transmit().expect("second RTP transmit");
        let duplicate = transmit.bytes.clone();
        right
            .handle_datagram(now, transmit.source, transmit.destination, transmit.bytes)
            .expect("first packet accepted");
        right
            .handle_datagram(now, transmit.source, transmit.destination, duplicate)
            .expect("replay dropped");
        assert!(!std::iter::from_fn(|| right.poll_event()).any(
            |event| matches!(event, TransportEvent::Rtp { metadata, .. } if metadata.sequence == 1)
        ));

        rtp[3] = 2;
        left.send_rtp(&rtp).expect("corruption source packet");
        let mut corrupt = left.poll_transmit().expect("corruption transmit");
        let last = corrupt.bytes.len().checked_sub(1).expect("auth tag exists");
        *corrupt
            .bytes
            .get_mut(last)
            .expect("last index was derived from packet length") ^= 1;
        assert_eq!(
            right.handle_datagram(now, corrupt.source, corrupt.destination, corrupt.bytes),
            Err(TransportError::Crypto)
        );
        assert_eq!(right.state(), TransportState::Failed);
    }

    #[test]
    fn wrong_tuple_and_bad_fingerprint_never_reach_media() {
        let left_certificate = certificate();
        let right_certificate = certificate();
        let mut left = Transport::new(
            config(
                address(4100),
                address(4101),
                left_certificate.clone(),
                &right_certificate,
                true,
            ),
            Instant::now(),
        )
        .expect("left transport");
        let mut right_config = config(
            address(4101),
            address(4100),
            right_certificate,
            &left_certificate,
            false,
        );
        right_config.remote_fingerprint = fingerprint(&certificate());
        let mut right = Transport::new(right_config, Instant::now()).expect("right transport");
        let mut now = Instant::now();
        for _ in 0..100 {
            while let Some(transmit) = left.poll_transmit() {
                let _ = right.handle_datagram(
                    now,
                    transmit.source,
                    transmit.destination,
                    transmit.bytes,
                );
            }
            while let Some(transmit) = right.poll_transmit() {
                left.handle_datagram(now, transmit.source, transmit.destination, transmit.bytes)
                    .expect("left remains safe");
            }
            if left.next_deadline().is_some_and(|deadline| deadline <= now) {
                let _ = left.handle_timeout(now);
            }
            if right
                .next_deadline()
                .is_some_and(|deadline| deadline <= now)
            {
                let _ = right.handle_timeout(now);
            }
            now = now
                .checked_add(Duration::from_millis(50))
                .expect("deterministic test clock does not overflow");
        }
        assert_ne!(right.state(), TransportState::Connected);
        let packet = vec![0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7, 1];
        let _ = right.handle_datagram(now, address(4999), address(4101), packet);
        assert!(
            !std::iter::from_fn(|| right.poll_event())
                .any(|event| matches!(event, TransportEvent::Rtp { .. }))
        );
    }

    #[test]
    fn close_drains_dtls_alert_without_a_timer() {
        let left_certificate = certificate();
        let right_certificate = certificate();
        let mut left = Transport::new(
            config(
                address(5000),
                address(5001),
                left_certificate.clone(),
                &right_certificate,
                true,
            ),
            Instant::now(),
        )
        .expect("left transport");
        let mut right = Transport::new(
            config(
                address(5001),
                address(5000),
                right_certificate,
                &left_certificate,
                false,
            ),
            Instant::now(),
        )
        .expect("right transport");
        let mut now = Instant::now();
        connect(&mut left, &mut right, &mut now);

        left.close(now).expect("close emits DTLS alert");
        assert_eq!(left.state(), TransportState::Closed);
        assert_eq!(left.next_deadline(), None);
        assert!(
            std::iter::from_fn(|| left.poll_transmit())
                .any(|transmit| transmit.kind == DatagramKind::Dtls)
        );
    }
}
