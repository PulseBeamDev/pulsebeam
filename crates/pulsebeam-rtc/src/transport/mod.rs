#![allow(
    dead_code,
    reason = "the private transport preparation seam is consumed by later scheduler and poll milestones"
)]

mod dtls;
mod ice;
mod srtp;

use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use is::{Candidate, IceConnectionState, IceCreds};
use str0m::crypto::Fingerprint;
use str0m::crypto::dtls::DtlsCert;

use crate::negotiation::{DtlsRole, NegotiatedSessionFacts};
use crate::{GlobalMediaTime, IceTcpFlowId, NetworkInput, TimePoint, TransmitTarget};

use srtp::{RtpMetadata, SrtpError};

const MAX_EVENTS: usize = 256;
const MAX_TRANSMISSIONS: usize = 256;
const MAX_CANDIDATE_PAIRS: usize = 128;
const MAX_PENDING_DTLS: usize = 64;
const MAX_TCP_FLOWS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportState {
    Checking,
    Connecting,
    Connected,
    Draining,
    Closed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DatagramKind {
    Stun,
    Dtls,
    Rtp,
    Rtcp,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TransportEvent {
    StateChanged(TransportState),
    IceStateChanged(IceConnectionState),
    Rtp {
        arrival: TimePoint,
        bytes: Vec<u8>,
        metadata: RtpMetadata,
    },
    Rtcp {
        arrival: TimePoint,
        bytes: Vec<u8>,
    },
    Data(Vec<u8>),
    Closed,
}

#[derive(Debug, PartialEq, Eq)]
#[cfg(test)]
pub(crate) struct TransportTransmit {
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub bytes: Vec<u8>,
    pub kind: DatagramKind,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PreparedTransmit {
    pub(crate) target: TransmitTarget,
    pub(crate) bytes: Vec<u8>,
    pub(crate) kind: DatagramKind,
    pub(crate) wire_len: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TransportError {
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

pub(crate) struct TransportConfig {
    pub local_ice: IceCreds,
    pub local_candidates: Box<[Candidate]>,
    pub remote_ice: IceCreds,
    pub remote_candidates: Box<[Candidate]>,
    pub certificate: DtlsCert,
    pub remote_fingerprint: Fingerprint,
    pub dtls_role: DtlsRole,
    pub ice_controlling: bool,
    pub ice_tie_breaker: u64,
    pub max_candidate_pairs: usize,
    pub max_events: usize,
    pub max_transmissions: usize,
    rtp_payload_types: Box<[u8]>,
}

impl TransportConfig {
    pub(crate) fn new(
        local_ice: IceCreds,
        local_candidate: Candidate,
        remote_ice: IceCreds,
        remote_candidates: Box<[Candidate]>,
        certificate: DtlsCert,
        remote_fingerprint: Fingerprint,
        dtls_role: DtlsRole,
    ) -> Self {
        Self::from_candidates(
            local_ice,
            vec![local_candidate].into_boxed_slice(),
            remote_ice,
            remote_candidates,
            certificate,
            remote_fingerprint,
            dtls_role,
        )
    }

    pub(crate) fn from_candidates(
        local_ice: IceCreds,
        local_candidates: Box<[Candidate]>,
        remote_ice: IceCreds,
        remote_candidates: Box<[Candidate]>,
        certificate: DtlsCert,
        remote_fingerprint: Fingerprint,
        dtls_role: DtlsRole,
    ) -> Self {
        Self {
            local_ice,
            local_candidates,
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

    pub(crate) fn with_ice_role(mut self, controlling: bool, tie_breaker: u64) -> Self {
        self.ice_controlling = controlling;
        self.ice_tie_breaker = tie_breaker;
        self
    }

    pub(crate) fn with_rtp_payload_types(mut self, payload_types: Box<[u8]>) -> Self {
        self.rtp_payload_types = payload_types;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), TransportError> {
        if self.max_candidate_pairs == 0
            || self.max_events == 0
            || self.max_transmissions == 0
            || self.max_candidate_pairs > MAX_CANDIDATE_PAIRS
            || self.max_events > MAX_EVENTS
            || self.max_transmissions > MAX_TRANSMISSIONS
            || self.ice_tie_breaker == 0
            || self.remote_candidates.is_empty()
            || self.remote_candidates.len() > self.max_candidate_pairs
            || self.local_candidates.is_empty()
            || self.remote_fingerprint.hash_func != "sha-256"
            || self.remote_fingerprint.bytes.len() != 32
            || Self::validate_rtp_payload_types(&self.rtp_payload_types).is_err()
        {
            return Err(TransportError::Configuration);
        }
        Ok(())
    }

    pub(crate) fn validate_rtp_payload_types(payload_types: &[u8]) -> Result<(), TransportError> {
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

pub(crate) struct Transport {
    selected: Option<SelectedPath>,
    state: TransportState,
    last_now: Option<Instant>,
    ice: ice::IceLayer,
    dtls: Option<dtls::DtlsLayer>,
    srtp: Option<srtp::SrtpLayer>,
    certificate: Option<DtlsCert>,
    local_fingerprint: Fingerprint,
    remote_fingerprint: Fingerprint,
    dtls_role: DtlsRole,
    rtp_payload_types: Box<[u8]>,
    events: VecDeque<TransportEvent>,
    transmissions: VecDeque<PreparedTransmit>,
    pending_dtls: VecDeque<(SelectedPath, Vec<u8>)>,
    next_deadline: Option<Instant>,
    max_events: usize,
    max_transmissions: usize,
    tcp_flows: Vec<(IceTcpFlowId, SocketAddr, SocketAddr)>,
    dropped_inputs: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NetworkEnvelope {
    Udp {
        local: SocketAddr,
        remote: SocketAddr,
    },
    IceTcp {
        flow: IceTcpFlowId,
        local: SocketAddr,
        remote: SocketAddr,
    },
}

impl NetworkEnvelope {
    fn source(&self) -> SocketAddr {
        match self {
            Self::Udp { remote, .. } | Self::IceTcp { remote, .. } => *remote,
        }
    }

    fn destination(&self) -> SocketAddr {
        match self {
            Self::Udp { local, .. } | Self::IceTcp { local, .. } => *local,
        }
    }

    fn protocol(&self) -> is::Protocol {
        match self {
            Self::Udp { .. } => is::Protocol::Udp,
            Self::IceTcp { .. } => is::Protocol::Tcp,
        }
    }
    fn target(&self) -> TransmitTarget {
        match self {
            Self::Udp { local, remote } => TransmitTarget::Udp {
                local: *local,
                remote: *remote,
                ecn: None,
            },
            Self::IceTcp { flow, .. } => TransmitTarget::IceTcp { flow: *flow },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedPath(NetworkEnvelope);

impl SelectedPath {
    fn source(&self) -> SocketAddr {
        self.0.source()
    }
    fn destination(&self) -> SocketAddr {
        self.0.destination()
    }
    fn protocol(&self) -> is::Protocol {
        self.0.protocol()
    }
    fn target(&self) -> TransmitTarget {
        self.0.target()
    }
}

impl Transport {
    pub(crate) fn from_session(
        facts: &NegotiatedSessionFacts,
        now: Instant,
    ) -> Result<Self, TransportError> {
        let remote_candidates = facts
            .remote_candidates
            .iter()
            .map(|candidate| {
                Candidate::from_sdp_string(candidate).map_err(|_| TransportError::Configuration)
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_boxed_slice();
        let local_ice = IceCreds {
            ufrag: facts.local_ice.ufrag.clone(),
            pass: facts.local_ice.password.clone(),
        };
        let remote_ice = IceCreds {
            ufrag: facts.remote_ice.ufrag.clone(),
            pass: facts.remote_ice.password.clone(),
        };
        let config = TransportConfig::from_candidates(
            local_ice,
            facts.local_candidates.clone(),
            remote_ice,
            remote_candidates,
            facts.dtls_identity.clone(),
            Fingerprint {
                hash_func: facts.remote_fingerprint.algorithm.to_owned(),
                bytes: facts.remote_fingerprint.value.to_vec(),
            },
            facts.local_dtls_role,
        );
        Self::new(config, now)
    }

    pub fn new(config: TransportConfig, now: Instant) -> Result<Self, TransportError> {
        config.validate()?;
        let provider = str0m::crypto::from_feature_flags();
        let remote_fingerprint = config.remote_fingerprint;
        let local_fingerprint = Fingerprint {
            hash_func: "sha-256".to_owned(),
            bytes: provider
                .sha256_provider
                .sha256(&config.certificate.certificate)
                .to_vec(),
        };
        let ice = ice::IceLayer::new(
            config.local_ice,
            &config.local_candidates,
            config.remote_ice,
            &config.remote_candidates,
            config.ice_controlling,
            config.max_candidate_pairs,
            config.ice_tie_breaker,
        );
        let mut transport = Self {
            selected: None,
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
            tcp_flows: Vec::new(),
            dropped_inputs: 0,
        };
        transport.push_event(TransportEvent::StateChanged(TransportState::Checking))?;
        transport
            .ice
            .handle_timeout(now)
            .map_err(|_| TransportError::Protocol)?;
        transport.drain_ice(now, &provider)?;
        Ok(transport)
    }

    pub(crate) fn state(&self) -> TransportState {
        self.state
    }

    pub(crate) fn local_fingerprint(&self) -> &Fingerprint {
        &self.local_fingerprint
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if matches!(
            self.state,
            TransportState::Connected | TransportState::Closed | TransportState::Failed
        ) {
            None
        } else {
            self.next_deadline
        }
    }

    pub(crate) fn poll_event(&mut self) -> Option<TransportEvent> {
        self.events.pop_front()
    }

    pub(crate) fn smoothed_rtt(&self) -> Option<Duration> {
        self.ice.smoothed_rtt()
    }

    #[cfg(test)]
    pub(crate) fn poll_transmit(&mut self) -> Option<TransportTransmit> {
        let item = self.poll_prepared()?;
        let TransmitTarget::Udp { local, remote, .. } = item.target else {
            return None;
        };
        debug_assert!(self.transmissions.len() <= self.max_transmissions);
        Some(TransportTransmit {
            source: local,
            destination: remote,
            bytes: item.bytes,
            kind: item.kind,
        })
    }

    pub(crate) fn poll_prepared(&mut self) -> Option<PreparedTransmit> {
        self.transmissions.pop_front()
    }

    pub(crate) fn classify(bytes: &[u8]) -> Option<DatagramKind> {
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

    pub(crate) fn handle_datagram(
        &mut self,
        now: Instant,
        source: SocketAddr,
        destination: SocketAddr,
        bytes: Vec<u8>,
    ) -> Result<(), TransportError> {
        self.handle_envelope(
            now,
            TimePoint {
                monotonic: now,
                global: GlobalMediaTime::from_micros(0),
            },
            SelectedPath(NetworkEnvelope::Udp {
                local: destination,
                remote: source,
            }),
            bytes,
        )
    }

    fn handle_envelope(
        &mut self,
        now: Instant,
        arrival: TimePoint,
        path: SelectedPath,
        bytes: Vec<u8>,
    ) -> Result<(), TransportError> {
        self.observe(now)?;
        if bytes.is_empty() {
            self.drop_input();
            return Ok(());
        }
        let Some(kind) = Self::classify(&bytes) else {
            self.drop_input();
            return Ok(());
        };
        let source = path.source();
        let destination = path.destination();
        let proto = path.protocol();
        match kind {
            DatagramKind::Stun => {
                if !self.local_candidates_contains(destination, proto) {
                    self.drop_input();
                    return Ok(());
                }
                match self
                    .ice
                    .handle_packet(now, source, destination, proto, &bytes)
                {
                    Ok(()) => {
                        self.bind_current_tcp_flow(path);
                        let provider = str0m::crypto::from_feature_flags();
                        self.drain_ice(now, &provider)?;
                    }
                    Err(_) => self.drop_input(),
                }
            }
            DatagramKind::Dtls => {
                let provider = str0m::crypto::from_feature_flags();
                self.drain_ice(now, &provider)?;
                if !self.accepts_path(&path) {
                    if self.can_queue_path(&path) && self.pending_dtls.len() < MAX_PENDING_DTLS {
                        self.pending_dtls.push_back((path, bytes));
                    } else {
                        self.drop_input();
                    }
                    return Ok(());
                }
                self.handle_dtls_packet(now, path, bytes, &provider)?;
            }
            DatagramKind::Rtp | DatagramKind::Rtcp => {
                if self.state != TransportState::Connected || !self.accepts_path(&path) {
                    self.drop_input();
                    return Ok(());
                }
                let Some(srtp) = self.srtp.as_mut() else {
                    return Ok(());
                };
                match kind {
                    DatagramKind::Rtp => match srtp.unprotect_rtp(&bytes) {
                        Ok((bytes, metadata)) => {
                            if self.accepts_payload_type(metadata.payload_type) {
                                self.push_event(TransportEvent::Rtp {
                                    arrival,
                                    bytes,
                                    metadata,
                                })?;
                            }
                        }
                        Err(SrtpError::Replay | SrtpError::InvalidPacket) => self.drop_input(),
                        Err(SrtpError::Crypto) => self.fail(TransportError::Crypto)?,
                        Err(SrtpError::OutputFull | SrtpError::UnsupportedProfile) => {
                            self.fail(TransportError::Protocol)?;
                        }
                    },
                    DatagramKind::Rtcp => match srtp.unprotect_rtcp(&bytes) {
                        Ok(bytes) => self.push_event(TransportEvent::Rtcp { arrival, bytes })?,
                        Err(SrtpError::Replay | SrtpError::InvalidPacket) => self.drop_input(),
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

    pub(crate) fn receive(
        &mut self,
        now: Instant,
        input: NetworkInput,
    ) -> Result<(), TransportError> {
        self.receive_at(
            TimePoint {
                monotonic: now,
                global: GlobalMediaTime::from_micros(0),
            },
            input,
        )
    }

    pub(crate) fn receive_at(
        &mut self,
        arrival: TimePoint,
        input: NetworkInput,
    ) -> Result<(), TransportError> {
        let (envelope, bytes) = match input {
            NetworkInput::Udp {
                local,
                remote,
                payload,
                ..
            } => (NetworkEnvelope::Udp { local, remote }, payload.to_vec()),
            NetworkInput::IceTcp {
                flow,
                local,
                remote,
                frame,
            } => {
                let Ok(payload) = validate_rfc4571(&frame) else {
                    self.drop_input();
                    return Ok(());
                };
                if !self.local_candidates_contains(local, is::Protocol::Tcp) {
                    self.drop_input();
                    return Ok(());
                }
                match self.tcp_flows.iter().find(|(id, _, _)| *id == flow) {
                    Some((_, bound_local, bound_remote))
                        if *bound_local != local || *bound_remote != remote =>
                    {
                        self.drop_input();
                        return Ok(());
                    }
                    None if self.tcp_flows.len() >= MAX_TCP_FLOWS => {
                        self.drop_input();
                        return Ok(());
                    }
                    Some(_) | None => {}
                }
                (
                    NetworkEnvelope::IceTcp {
                        flow,
                        local,
                        remote,
                    },
                    payload.to_vec(),
                )
            }
        };
        self.handle_envelope(arrival.monotonic, arrival, SelectedPath(envelope), bytes)
    }

    fn local_candidates_contains(&self, address: SocketAddr, proto: is::Protocol) -> bool {
        self.ice.has_local_candidate(address, proto)
    }

    pub(crate) fn handle_timeout(&mut self, now: Instant) -> Result<(), TransportError> {
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

    pub(crate) fn send_rtp(&mut self, packet: &[u8]) -> Result<(), TransportError> {
        self.send_secure(packet, DatagramKind::Rtp)
    }

    pub(crate) fn send_rtcp(&mut self, packet: &[u8]) -> Result<(), TransportError> {
        self.send_secure(packet, DatagramKind::Rtcp)
    }

    pub(crate) fn close(&mut self, now: Instant) -> Result<(), TransportError> {
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
        let Some(path) = self.selected.clone() else {
            return Err(TransportError::Protocol);
        };
        if kind == DatagramKind::Rtp {
            let payload_type = packet.get(1).ok_or(TransportError::InvalidInput)? & 0x7f;
            if !self.accepts_payload_type(payload_type) {
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
        self.prepare_transmit(path, bytes, kind)
    }

    fn drain_ice(
        &mut self,
        now: Instant,
        provider: &str0m::crypto::CryptoProvider,
    ) -> Result<(), TransportError> {
        while let Some((proto, source, destination, bytes)) = self.ice.poll_transmit() {
            let Some(path) = self.path_for_tuple(proto, source, destination) else {
                self.drop_input();
                continue;
            };
            self.prepare_transmit(path, bytes, DatagramKind::Stun)?;
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
                    proto,
                    source,
                    destination,
                } => {
                    let Some(path) = self.path_for_tuple(proto, source, destination) else {
                        self.drop_input();
                        continue;
                    };
                    self.selected = Some(path);
                    self.state = TransportState::Connecting;
                    self.push_event(TransportEvent::StateChanged(TransportState::Connecting))?;
                    self.start_dtls(now, provider)?;
                }
                ice::IceEvent::Restart => {
                    if self.dtls.is_some() {
                        self.fail(TransportError::Protocol)?;
                    }
                    self.selected = None;
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
        let path = self.selected.clone().ok_or(TransportError::Protocol)?;
        for bytes in packets {
            self.prepare_transmit(path.clone(), bytes, DatagramKind::Dtls)?;
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
        path: SelectedPath,
        bytes: Vec<u8>,
        provider: &str0m::crypto::CryptoProvider,
    ) -> Result<(), TransportError> {
        if self.dtls.is_none() {
            self.selected = Some(path);
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
        let Some(selected) = self.selected.clone() else {
            return Ok(());
        };
        let mut pending = std::mem::take(&mut self.pending_dtls);
        while let Some((path, bytes)) = pending.pop_front() {
            if path == selected
                && self
                    .ice
                    .accepts_tuple(path.source(), path.destination(), path.protocol())
            {
                self.handle_dtls_packet(now, path, bytes, provider)?;
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

    fn prepare_transmit(
        &mut self,
        path: SelectedPath,
        mut bytes: Vec<u8>,
        kind: DatagramKind,
    ) -> Result<(), TransportError> {
        if matches!(path.0, NetworkEnvelope::IceTcp { .. }) {
            let length = u16::try_from(bytes.len()).map_err(|_| TransportError::Protocol)?;
            let mut frame = length.to_be_bytes().to_vec();
            frame.extend_from_slice(&bytes);
            bytes = frame;
        }
        let transmission = PreparedTransmit {
            target: path.target(),
            wire_len: bytes.len(),
            bytes,
            kind,
        };
        if transmission.bytes.is_empty() || self.transmissions.len() >= self.max_transmissions {
            let _ = self.fail(TransportError::QueueFull);
            return Err(TransportError::QueueFull);
        }
        self.transmissions.push_back(transmission);
        debug_assert!(self.transmissions.len() <= self.max_transmissions);
        Ok(())
    }

    fn path_for_tuple(
        &self,
        proto: is::Protocol,
        local: SocketAddr,
        remote: SocketAddr,
    ) -> Option<SelectedPath> {
        match proto {
            is::Protocol::Udp => self
                .local_candidates_contains(local, proto)
                .then_some(SelectedPath(NetworkEnvelope::Udp { local, remote })),
            is::Protocol::Tcp => self
                .tcp_flows
                .iter()
                .find(|(_, bound_local, bound_remote)| {
                    *bound_local == local && *bound_remote == remote
                })
                .map(|(flow, _, _)| {
                    SelectedPath(NetworkEnvelope::IceTcp {
                        flow: *flow,
                        local,
                        remote,
                    })
                }),
            _ => None,
        }
    }

    fn accepts_path(&self, path: &SelectedPath) -> bool {
        if !self
            .ice
            .accepts_tuple(path.source(), path.destination(), path.protocol())
        {
            return false;
        }
        match path.0 {
            NetworkEnvelope::Udp { .. } => true,
            NetworkEnvelope::IceTcp {
                flow,
                local,
                remote,
            } => self
                .tcp_flows
                .iter()
                .any(|(bound_flow, bound_local, bound_remote)| {
                    *bound_flow == flow && *bound_local == local && *bound_remote == remote
                }),
        }
    }

    fn can_queue_path(&self, path: &SelectedPath) -> bool {
        self.ice
            .can_queue_tuple(path.source(), path.destination(), path.protocol())
            && match path.0 {
                NetworkEnvelope::Udp { .. } => true,
                NetworkEnvelope::IceTcp {
                    flow,
                    local,
                    remote,
                } => self
                    .tcp_flows
                    .iter()
                    .any(|(bound_flow, bound_local, bound_remote)| {
                        *bound_flow == flow && *bound_local == local && *bound_remote == remote
                    }),
            }
    }

    fn bind_current_tcp_flow(&mut self, path: SelectedPath) {
        let NetworkEnvelope::IceTcp {
            flow,
            local,
            remote,
        } = path.0
        else {
            return;
        };
        if self.tcp_flows.iter().any(|(id, _, _)| *id == flow) {
            return;
        }
        if self.tcp_flows.len() < MAX_TCP_FLOWS {
            self.tcp_flows.push((flow, local, remote));
        }
    }

    fn drop_input(&mut self) {
        self.dropped_inputs = self.dropped_inputs.saturating_add(1);
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

fn validate_rfc4571(frame: &[u8]) -> Result<&[u8], TransportError> {
    let length: [u8; 2] = frame
        .get(..2)
        .ok_or(TransportError::InvalidInput)?
        .try_into()
        .map_err(|_| TransportError::InvalidInput)?;
    let declared = usize::from(u16::from_be_bytes(length));
    if declared == 0
        || frame.len()
            != declared
                .checked_add(2)
                .ok_or(TransportError::InvalidInput)?
    {
        return Err(TransportError::InvalidInput);
    }
    frame.get(2..).ok_or(TransportError::InvalidInput)
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
    #[allow(
        clippy::disallowed_types,
        reason = "NetworkInput is intentionally Bytes-backed"
    )]
    use bytes::Bytes;
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

    fn fingerprint(certificate: &DtlsCert) -> Fingerprint {
        let provider = str0m::crypto::from_feature_flags();
        Fingerprint {
            hash_func: "sha-256".to_owned(),
            bytes: provider
                .sha256_provider
                .sha256(&certificate.certificate)
                .to_vec(),
        }
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

    #[allow(
        clippy::disallowed_types,
        reason = "NetworkInput is intentionally Bytes-backed"
    )]
    fn connect(left: &mut Transport, right: &mut Transport, now: &mut Instant) {
        for _ in 0..400 {
            let mut progress = false;
            while let Some(transmit) = left.poll_prepared() {
                progress = true;
                let TransmitTarget::Udp { local, remote, .. } = transmit.target else {
                    panic!("UDP fixture only routes UDP transmissions");
                };
                right
                    .receive(
                        *now,
                        NetworkInput::Udp {
                            local: remote,
                            remote: local,
                            ecn: None,
                            payload: Bytes::from(transmit.bytes),
                        },
                    )
                    .expect("right accepts deterministic datagram");
            }
            while let Some(transmit) = right.poll_prepared() {
                progress = true;
                let TransmitTarget::Udp { local, remote, .. } = transmit.target else {
                    panic!("UDP fixture only routes UDP transmissions");
                };
                left.receive(
                    *now,
                    NetworkInput::Udp {
                        local: remote,
                        remote: local,
                        ecn: None,
                        payload: Bytes::from(transmit.bytes),
                    },
                )
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
    #[allow(
        clippy::disallowed_types,
        reason = "NetworkInput is intentionally Bytes-backed"
    )]
    fn ice_tcp_frames_are_complete_and_flow_bound() {
        let local_certificate = certificate();
        let remote_certificate = certificate();
        let local = address(6100);
        let remote = address(6101);
        let mut transport = Transport::new(
            TransportConfig::new(
                IceCreds {
                    ufrag: "local".to_owned(),
                    pass: "localpasswordabcdefghijklmnop".to_owned(),
                },
                Candidate::builder()
                    .tcp()
                    .host(local)
                    .tcptype(str0m::net::TcpType::Passive)
                    .build()
                    .expect("passive local"),
                IceCreds {
                    ufrag: "remote".to_owned(),
                    pass: "remotepasswordabcdefghijklmnop".to_owned(),
                },
                vec![
                    Candidate::builder()
                        .tcp()
                        .host(remote)
                        .tcptype(str0m::net::TcpType::Passive)
                        .build()
                        .expect("passive remote"),
                ]
                .into_boxed_slice(),
                local_certificate,
                fingerprint(&remote_certificate),
                DtlsRole::Active,
            ),
            Instant::now(),
        )
        .expect("mixed transport configuration");
        let now = Instant::now();
        assert_eq!(
            validate_rfc4571(&[0, 2, 1]),
            Err(TransportError::InvalidInput)
        );
        assert_eq!(validate_rfc4571(&[0, 0]), Err(TransportError::InvalidInput));
        transport
            .receive(
                now,
                NetworkInput::IceTcp {
                    flow: IceTcpFlowId::from_value(7),
                    local,
                    remote,
                    frame: bytes::Bytes::from_static(&[0, 1, 0]),
                },
            )
            .expect("complete unknown frame is dropped");
        transport
            .receive(
                now,
                NetworkInput::IceTcp {
                    flow: IceTcpFlowId::from_value(7),
                    local,
                    remote: address(6102),
                    frame: bytes::Bytes::from_static(&[0, 1, 0]),
                },
            )
            .expect("rebound flow is dropped");
        assert!(transport.tcp_flows.is_empty());
        assert!(transport.dropped_inputs >= 2);
    }

    #[test]
    fn prepared_ice_tcp_is_framed_and_flow_targeted() {
        let local_certificate = certificate();
        let remote_certificate = certificate();
        let local = address(6200);
        let remote = address(6201);
        let mut transport = Transport::new(
            TransportConfig::new(
                IceCreds {
                    ufrag: "local".to_owned(),
                    pass: "localpasswordabcdefghijklmnop".to_owned(),
                },
                Candidate::builder()
                    .tcp()
                    .host(local)
                    .tcptype(str0m::net::TcpType::Passive)
                    .build()
                    .expect("passive local"),
                IceCreds {
                    ufrag: "remote".to_owned(),
                    pass: "remotepasswordabcdefghijklmnop".to_owned(),
                },
                vec![
                    Candidate::builder()
                        .tcp()
                        .host(remote)
                        .tcptype(str0m::net::TcpType::Passive)
                        .build()
                        .expect("passive remote"),
                ]
                .into_boxed_slice(),
                local_certificate,
                fingerprint(&remote_certificate),
                DtlsRole::Active,
            ),
            Instant::now(),
        )
        .expect("transport");
        transport.transmissions.clear();
        transport
            .prepare_transmit(
                SelectedPath(NetworkEnvelope::IceTcp {
                    flow: IceTcpFlowId::from_value(9),
                    local,
                    remote,
                }),
                vec![1, 2, 3],
                DatagramKind::Dtls,
            )
            .expect("prepared frame");
        let prepared = transport.poll_prepared().expect("prepared output");
        assert_eq!(
            prepared.target,
            TransmitTarget::IceTcp {
                flow: IceTcpFlowId::from_value(9)
            }
        );
        assert_eq!(prepared.bytes, vec![0, 3, 1, 2, 3]);
        assert_eq!(prepared.wire_len, prepared.bytes.len());
    }

    #[test]
    #[allow(
        clippy::disallowed_types,
        reason = "NetworkInput is intentionally Bytes-backed"
    )]
    fn tcp_flow_bindings_are_bounded() {
        let local_certificate = certificate();
        let remote_certificate = certificate();
        let local = address(6300);
        let remote = address(6301);
        let mut transport = Transport::new(
            TransportConfig::new(
                IceCreds {
                    ufrag: "local".to_owned(),
                    pass: "localpasswordabcdefghijklmnop".to_owned(),
                },
                Candidate::builder()
                    .tcp()
                    .host(local)
                    .tcptype(str0m::net::TcpType::Passive)
                    .build()
                    .expect("passive local"),
                IceCreds {
                    ufrag: "remote".to_owned(),
                    pass: "remotepasswordabcdefghijklmnop".to_owned(),
                },
                vec![
                    Candidate::builder()
                        .tcp()
                        .host(remote)
                        .tcptype(str0m::net::TcpType::Passive)
                        .build()
                        .expect("passive remote"),
                ]
                .into_boxed_slice(),
                local_certificate,
                fingerprint(&remote_certificate),
                DtlsRole::Active,
            ),
            Instant::now(),
        )
        .expect("transport");
        transport.tcp_flows = (0..MAX_TCP_FLOWS)
            .map(|value| (IceTcpFlowId::from_value(value as u64), local, remote))
            .collect();
        transport.dropped_inputs = 0;
        transport
            .receive(
                Instant::now(),
                NetworkInput::IceTcp {
                    flow: IceTcpFlowId::from_value(999),
                    local,
                    remote,
                    frame: bytes::Bytes::from_static(&[0, 1, 0]),
                },
            )
            .expect("bounded drop");
        assert_eq!(transport.tcp_flows.len(), MAX_TCP_FLOWS);
        assert_eq!(transport.dropped_inputs, 1);
    }

    #[test]
    #[allow(
        clippy::disallowed_types,
        reason = "NetworkInput is intentionally Bytes-backed"
    )]
    fn rejected_stun_is_counted_once_without_progress() {
        let left_certificate = certificate();
        let right_certificate = certificate();
        let now = Instant::now();
        let mut transport = Transport::new(
            config(
                address(6400),
                address(6401),
                left_certificate,
                &right_certificate,
                true,
            ),
            now,
        )
        .expect("transport");
        let events = transport.events.len();
        let dropped = transport.dropped_inputs;
        let mut malformed_stun = vec![0; 20];
        malformed_stun[4..8].copy_from_slice(&[0x21, 0x12, 0xa4, 0x42]);

        transport
            .receive(
                now,
                NetworkInput::Udp {
                    local: address(6400),
                    remote: address(6401),
                    ecn: None,
                    payload: Bytes::from(malformed_stun),
                },
            )
            .expect("malformed STUN is dropped");

        assert_eq!(transport.dropped_inputs, dropped + 1);
        assert_eq!(transport.events.len(), events);
        assert_eq!(transport.state(), TransportState::Checking);
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
        if let TransportEvent::Rtp {
            bytes, metadata, ..
        } = event
        {
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
            |event| matches!(event, TransportEvent::Rtp { bytes, metadata, .. } if bytes == padded_rtp && metadata.ssrc == 8)
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
                .any(|event| matches!(event, TransportEvent::Rtcp { bytes, .. } if bytes == rtcp))
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
