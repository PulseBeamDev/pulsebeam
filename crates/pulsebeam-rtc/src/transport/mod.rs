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

use crate::{NetworkInput, TransmitTarget};

use srtp::{RtpMetadata, SrtpError, outbound_rtp_metadata, outbound_twcc_sequence};

const MAX_EVENTS: usize = 256;
const MAX_TRANSMISSIONS: usize = 256;
const MAX_CANDIDATE_PAIRS: usize = 128;
const MAX_PENDING_DTLS: usize = 64;

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
    Sctp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct PathEpoch(u64);

impl PathEpoch {
    pub(crate) const fn from_value(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn value(self) -> u64 {
        self.0
    }

    fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreparedRtpIdentity {
    pub(crate) ssrc: u32,
    pub(crate) sequence: u16,
    pub(crate) index: u64,
    pub(crate) twcc_sequence: Option<u16>,
    pub(crate) service: RtpService,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RtpService {
    Original,
    Repair,
    Padding,
}

impl crate::TransmitKind {
    pub(crate) const fn rtp_service(self) -> Option<RtpService> {
        match self {
            Self::Original { .. } => Some(RtpService::Original),
            Self::Repair { .. } => Some(RtpService::Repair),
            Self::Padding { .. } => Some(RtpService::Padding),
            Self::Control => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TransportEvent {
    StateChanged(TransportState),
    IceStateChanged(IceConnectionState),
    AuthenticatedPath(crate::AuthenticatedPath),
    Rtp {
        arrival: Instant,
        path_epoch: PathEpoch,
        bytes: Vec<u8>,
        metadata: RtpMetadata,
        ecn: Option<crate::EcnCodepoint>,
    },
    Rtcp {
        arrival: Instant,
        path_epoch: PathEpoch,
        bytes: Vec<u8>,
    },
    SelectedPathChanged {
        previous: Option<NetworkEnvelope>,
        current: Option<NetworkEnvelope>,
        epoch: PathEpoch,
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
    pub(crate) classification: crate::TransmitKind,
    pub(crate) bytes: Vec<u8>,
    pub(crate) kind: DatagramKind,
    pub(crate) wire_len: usize,
    pub(crate) path_epoch: Option<PathEpoch>,
    pub(crate) rtp: Option<PreparedRtpIdentity>,
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
    inbound_payload_types: Box<[u8]>,
    outbound_payload_types: Box<[u8]>,
    twcc_extension_id: Option<u8>,
    twcc_payload_map: Box<[(u8, u8)]>,
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
            inbound_payload_types: (0..=127)
                .filter(|payload| !(64..=95).contains(payload))
                .collect(),
            outbound_payload_types: Box::new([]),
            twcc_extension_id: None,
            twcc_payload_map: Box::new([]),
        }
    }

    pub(crate) fn with_ice_role(mut self, controlling: bool, tie_breaker: u64) -> Self {
        self.ice_controlling = controlling;
        self.ice_tie_breaker = tie_breaker;
        self
    }

    pub(crate) fn with_rtp_payload_types(
        mut self,
        inbound: Box<[u8]>,
        outbound: Box<[u8]>,
    ) -> Self {
        self.inbound_payload_types = inbound;
        self.outbound_payload_types = outbound;
        self
    }

    fn with_twcc_extension_id(mut self, twcc_extension_id: Option<u8>) -> Self {
        self.twcc_extension_id = twcc_extension_id;
        self
    }

    fn with_twcc_payload_map(mut self, twcc_payload_map: Box<[(u8, u8)]>) -> Self {
        self.twcc_payload_map = twcc_payload_map;
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
            || self.ice_controlling && self.remote_candidates.is_empty()
            || self.remote_candidates.len() > self.max_candidate_pairs
            || self.local_candidates.is_empty()
            || self.remote_fingerprint.hash_func != "sha-256"
            || self.remote_fingerprint.bytes.len() != 32
            || Self::validate_rtp_payload_types(&self.inbound_payload_types).is_err()
            || Self::validate_rtp_payload_types(&self.outbound_payload_types).is_err()
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
    next_path_epoch: PathEpoch,
    state: TransportState,
    last_now: Option<Instant>,
    ice: ice::IceLayer,
    dtls: Option<dtls::DtlsLayer>,
    srtp: Option<srtp::SrtpLayer>,
    allowed_inbound_ssrcs: Option<Box<[u32]>>,
    certificate: Option<DtlsCert>,
    local_fingerprint: Fingerprint,
    remote_fingerprint: Fingerprint,
    dtls_role: DtlsRole,
    inbound_payload_types: Box<[u8]>,
    outbound_payload_types: Box<[u8]>,
    twcc_extension_id: Option<u8>,
    twcc_payload_map: Box<[(u8, u8)]>,
    events: VecDeque<TransportEvent>,
    transmissions: VecDeque<PreparedTransmit>,
    pending_dtls: VecDeque<(SelectedPath, Vec<u8>)>,
    next_deadline: Option<Instant>,
    max_events: usize,
    max_transmissions: usize,
    dropped_inputs: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NetworkEnvelope {
    Udp {
        local: SocketAddr,
        remote: SocketAddr,
    },
    IceTcp {
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
            Self::IceTcp { local, remote } => TransmitTarget::IceTcp {
                local: *local,
                remote: *remote,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedPath {
    envelope: NetworkEnvelope,
    epoch: Option<PathEpoch>,
}

impl SelectedPath {
    fn unselected(envelope: NetworkEnvelope) -> Self {
        Self {
            envelope,
            epoch: None,
        }
    }
    fn source(&self) -> SocketAddr {
        self.envelope.source()
    }
    fn destination(&self) -> SocketAddr {
        self.envelope.destination()
    }
    fn protocol(&self) -> is::Protocol {
        self.envelope.protocol()
    }
    fn target(&self) -> TransmitTarget {
        self.envelope.target()
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
        )
        .with_rtp_payload_types(
            facts.inbound_payload_types(),
            facts.outbound_payload_types(),
        )
        .with_twcc_payload_map(facts.outbound_twcc_payload_map())
        .with_twcc_extension_id(facts.outbound_twcc_extension_id());
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
            next_path_epoch: PathEpoch(0),
            state: TransportState::Checking,
            last_now: Some(now),
            ice,
            dtls: None,
            srtp: None,
            certificate: Some(config.certificate),
            local_fingerprint,
            remote_fingerprint,
            dtls_role: config.dtls_role,
            allowed_inbound_ssrcs: None,
            inbound_payload_types: config.inbound_payload_types,
            outbound_payload_types: config.outbound_payload_types,
            twcc_extension_id: config.twcc_extension_id,
            twcc_payload_map: config.twcc_payload_map,
            events: VecDeque::new(),
            transmissions: VecDeque::new(),
            pending_dtls: VecDeque::new(),
            next_deadline: None,
            max_events: config.max_events,
            max_transmissions: config.max_transmissions,
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

    #[cfg(test)]
    fn negotiated_security(
        &self,
    ) -> Option<(
        str0m::crypto::dtls::ProtocolVersion,
        str0m::crypto::dtls::SrtpProfile,
    )> {
        let dtls = self.dtls.as_ref()?;
        Some((dtls.protocol_version()?, dtls.negotiated_profile()?))
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if matches!(self.state, TransportState::Closed | TransportState::Failed) {
            None
        } else {
            self.next_deadline
        }
    }

    pub(crate) fn has_pending_output(&self) -> bool {
        !self.events.is_empty() || !self.transmissions.is_empty()
    }

    pub(crate) fn poll_event(&mut self) -> Option<TransportEvent> {
        if let Some(index) = self
            .events
            .iter()
            .rposition(|event| matches!(event, TransportEvent::SelectedPathChanged { .. }))
        {
            let latest = self.events.remove(index);
            // The next commit uses the current path, never an intermediate queued epoch.
            self.events
                .retain(|event| !matches!(event, TransportEvent::SelectedPathChanged { .. }));
            return latest;
        }
        self.events.pop_front()
    }

    #[cfg(test)]
    pub(crate) fn reselect_path_for_test(&mut self) -> Result<(), TransportError> {
        let selected = self.selected.clone().ok_or(TransportError::Protocol)?;
        self.select_path(SelectedPath::unselected(selected.envelope))
    }

    pub(crate) fn can_send_user_data(&self) -> bool {
        self.state == TransportState::Connected && self.selected.is_some()
    }

    pub(crate) fn smoothed_rtt(&self) -> Option<Duration> {
        self.ice.smoothed_rtt()
    }

    pub(crate) const fn dropped_inputs(&self) -> u64 {
        self.dropped_inputs
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

    #[cfg(test)]
    pub(crate) fn handle_datagram(
        &mut self,
        now: Instant,
        source: SocketAddr,
        destination: SocketAddr,
        bytes: Vec<u8>,
    ) -> Result<(), TransportError> {
        self.handle_envelope(
            now,
            now,
            SelectedPath::unselected(NetworkEnvelope::Udp {
                local: destination,
                remote: source,
            }),
            bytes,
            None,
        )
    }

    fn handle_envelope(
        &mut self,
        now: Instant,
        arrival: Instant,
        path: SelectedPath,
        bytes: Vec<u8>,
        ecn: Option<crate::EcnCodepoint>,
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
                    Ok(authenticated) => {
                        if authenticated {
                            self.push_event(TransportEvent::AuthenticatedPath(
                                crate::AuthenticatedPath {
                                    protocol: match path.envelope {
                                        NetworkEnvelope::Udp { .. } => crate::NetworkProtocol::Udp,
                                        NetworkEnvelope::IceTcp { .. } => {
                                            crate::NetworkProtocol::IceTcp
                                        }
                                    },
                                    local: destination,
                                    remote: source,
                                    arrival,
                                },
                            ))?;
                        }
                        let provider = str0m::crypto::from_feature_flags();
                        self.drain_ice(now, &provider)?;
                        // ICE nomination is tuple-based and may not repeat on a new stream.
                        if authenticated
                            && matches!(path.envelope, NetworkEnvelope::IceTcp { .. })
                            && self.selected.is_none()
                            && self.ice.accepts_tuple(source, destination, proto)
                        {
                            self.select_path(path)?;
                        }
                    }
                    Err(_) => self.drop_input(),
                }
            }
            DatagramKind::Dtls => {
                let provider = str0m::crypto::from_feature_flags();
                self.drain_ice(now, &provider)?;
                if !self.accepts_path(&path) {
                    if self.state == TransportState::Connecting && self.can_queue_path(&path) {
                        self.select_path(path.clone())?;
                    } else if self.can_queue_path(&path)
                        && self.pending_dtls.len() < MAX_PENDING_DTLS
                    {
                        self.pending_dtls.push_back((path, bytes));
                        return Ok(());
                    } else {
                        self.drop_input();
                        return Ok(());
                    }
                }
                self.handle_dtls_packet(now, path, bytes, &provider)?;
            }
            DatagramKind::Rtp | DatagramKind::Rtcp => {
                if self.state != TransportState::Connected || !self.accepts_path(&path) {
                    self.drop_input();
                    return Ok(());
                }
                if kind == DatagramKind::Rtp
                    && self.allowed_inbound_ssrcs.as_ref().is_some_and(|allowed| {
                        bytes
                            .get(8..12)
                            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
                            .map(u32::from_be_bytes)
                            .is_none_or(|ssrc| !allowed.contains(&ssrc))
                    })
                {
                    self.drop_input();
                    return Ok(());
                }
                let Some(srtp) = self.srtp.as_mut() else {
                    return Ok(());
                };
                match kind {
                    DatagramKind::Rtp => match srtp.unprotect_rtp(&bytes) {
                        Ok((bytes, metadata)) => {
                            if self.inbound_payload_types.contains(&metadata.payload_type) {
                                let path_epoch = self
                                    .selected
                                    .as_ref()
                                    .and_then(|selected| selected.epoch)
                                    .ok_or(TransportError::Protocol)?;
                                self.push_event(TransportEvent::Rtp {
                                    arrival,
                                    path_epoch,
                                    bytes,
                                    metadata,
                                    ecn,
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
                        Ok(bytes) => {
                            let path_epoch = self
                                .selected
                                .as_ref()
                                .and_then(|selected| selected.epoch)
                                .ok_or(TransportError::Protocol)?;
                            self.push_event(TransportEvent::Rtcp {
                                arrival,
                                path_epoch,
                                bytes,
                            })?;
                        }
                        Err(SrtpError::Replay | SrtpError::InvalidPacket) => self.drop_input(),
                        Err(SrtpError::Crypto) => self.fail(TransportError::Crypto)?,
                        Err(SrtpError::OutputFull | SrtpError::UnsupportedProfile) => {
                            self.fail(TransportError::Protocol)?;
                        }
                    },
                    _ => {}
                }
            }
            DatagramKind::Sctp => self.drop_input(),
        }
        Ok(())
    }

    pub(crate) fn receive(
        &mut self,
        now: Instant,
        arrival: Instant,
        input: NetworkInput,
    ) -> Result<(), TransportError> {
        let (envelope, bytes, ecn) = match input {
            NetworkInput::Udp {
                local,
                remote,
                payload,
                ecn,
            } => (
                NetworkEnvelope::Udp { local, remote },
                payload.to_vec(),
                ecn,
            ),
            NetworkInput::IceTcp {
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
                (
                    NetworkEnvelope::IceTcp { local, remote },
                    payload.to_vec(),
                    None,
                )
            }
        };
        self.handle_envelope(now, arrival, SelectedPath::unselected(envelope), bytes, ecn)
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
        self.drain_dtls(now, DatagramKind::Dtls)
    }

    pub(crate) fn send_rtp_with_info(
        &mut self,
        packet: &[u8],
        index: u64,
        classification: crate::TransmitKind,
    ) -> Result<(), TransportError> {
        self.send_secure(packet, Some(index), DatagramKind::Rtp, classification)
    }

    pub(crate) fn send_rtcp(&mut self, packet: &[u8]) -> Result<(), TransportError> {
        self.send_secure(
            packet,
            None,
            DatagramKind::Rtcp,
            crate::TransmitKind::Control,
        )
    }

    pub(crate) fn send_sctp(&mut self, packet: &[u8], now: Instant) -> Result<(), TransportError> {
        if !self.can_send_user_data() {
            return Err(TransportError::Protocol);
        }
        let dtls = self.dtls.as_mut().ok_or(TransportError::InvalidInput)?;
        dtls.send_application_data(packet, now)
            .map_err(error_to_transport)?;
        self.drain_dtls(now, DatagramKind::Sctp)
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
            self.drain_dtls(now, DatagramKind::Dtls)?;
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

    pub(crate) fn abort(&mut self) {
        self.state = TransportState::Closed;
        self.next_deadline = None;
        self.transmissions.clear();
        self.events.clear();
        self.pending_dtls.clear();
        self.dtls = None;
        self.srtp = None;
        self.certificate = None;
    }

    fn send_secure(
        &mut self,
        packet: &[u8],
        index: Option<u64>,
        kind: DatagramKind,
        classification: crate::TransmitKind,
    ) -> Result<(), TransportError> {
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
        let rtp = if kind == DatagramKind::Rtp {
            let metadata =
                outbound_rtp_metadata(packet).map_err(|_| TransportError::InvalidInput)?;
            let twcc_sequence = self
                .twcc_payload_map
                .iter()
                .filter(|(payload_type, _)| *payload_type == metadata.payload_type)
                .find_map(|(_, extension_id)| {
                    outbound_twcc_sequence(packet, Some(*extension_id))
                        .ok()
                        .flatten()
                })
                .or_else(|| {
                    outbound_twcc_sequence(packet, self.twcc_extension_id)
                        .ok()
                        .flatten()
                });
            Some(PreparedRtpIdentity {
                ssrc: metadata.ssrc,
                sequence: metadata.sequence,
                index: index.ok_or(TransportError::InvalidInput)?,
                twcc_sequence,
                service: classification
                    .rtp_service()
                    .ok_or(TransportError::InvalidInput)?,
            })
        } else {
            None
        };
        let Some(srtp) = self.srtp.as_mut() else {
            return Err(TransportError::Protocol);
        };
        let bytes = match kind {
            DatagramKind::Rtp => {
                srtp.protect_rtp(packet, index.ok_or(TransportError::InvalidInput)?)
            }
            DatagramKind::Rtcp => srtp.protect_rtcp(packet),
            _ => Err(SrtpError::InvalidPacket),
        }
        .map_err(|_| TransportError::Crypto)?;
        self.prepare_transmit(path, bytes, kind, rtp, classification)
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
            self.prepare_transmit(
                path,
                bytes,
                DatagramKind::Stun,
                None,
                crate::TransmitKind::Control,
            )?;
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
                    self.select_path(path)?;
                    if self.srtp.is_none() {
                        self.state = TransportState::Connecting;
                        self.push_event(TransportEvent::StateChanged(TransportState::Connecting))?;
                        self.start_dtls(now, provider)?;
                    }
                }
                ice::IceEvent::Restart => {
                    if self.dtls.is_some() {
                        self.fail(TransportError::Protocol)?;
                    }
                    self.clear_selected_path()?;
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
        self.drain_dtls(now, DatagramKind::Dtls)
    }

    fn drain_dtls(
        &mut self,
        _now: Instant,
        packet_kind: DatagramKind,
    ) -> Result<(), TransportError> {
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
        // Records produced without a live path are discarded, never retargeted later.
        if let Some(path) = self.selected.clone() {
            for bytes in packets {
                self.prepare_transmit(
                    path.clone(),
                    bytes,
                    packet_kind,
                    None,
                    crate::TransmitKind::Control,
                )?;
            }
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
            self.select_path(path)?;
            self.state = TransportState::Connecting;
            self.start_dtls(now, provider)?;
        }
        let Some(dtls) = self.dtls.as_mut() else {
            return Ok(());
        };
        if let Err(error) = dtls.handle_packet(&bytes, now) {
            self.fail(error_to_transport(error))?;
        }
        self.drain_dtls(now, DatagramKind::Dtls)
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
            if path.envelope == selected.envelope
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
        rtp: Option<PreparedRtpIdentity>,
        classification: crate::TransmitKind,
    ) -> Result<(), TransportError> {
        if matches!(path.envelope, NetworkEnvelope::IceTcp { .. }) {
            let length = u16::try_from(bytes.len()).map_err(|_| TransportError::Protocol)?;
            let mut frame = length.to_be_bytes().to_vec();
            frame.extend_from_slice(&bytes);
            bytes = frame;
        }
        if classification.rtp_service() != rtp.map(|identity| identity.service)
            || (kind == DatagramKind::Rtp) != rtp.is_some()
            || (kind == DatagramKind::Rtp && path.epoch.is_none())
        {
            return Err(TransportError::Protocol);
        }
        let transmission = PreparedTransmit {
            target: path.target(),
            classification,
            wire_len: bytes.len(),
            bytes,
            kind,
            path_epoch: path.epoch,
            rtp,
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
        if !self.local_candidates_contains(local, proto) {
            return None;
        }
        let envelope = match proto {
            is::Protocol::Udp => NetworkEnvelope::Udp { local, remote },
            is::Protocol::Tcp => NetworkEnvelope::IceTcp { local, remote },
            _ => return None,
        };
        Some(SelectedPath::unselected(envelope))
    }

    fn accepts_path(&self, path: &SelectedPath) -> bool {
        let selected_after_validated_dtls = self.state == TransportState::Connected
            && self
                .selected
                .as_ref()
                .is_some_and(|selected| selected.envelope == path.envelope);
        if !selected_after_validated_dtls
            && !self
                .ice
                .accepts_tuple(path.source(), path.destination(), path.protocol())
        {
            return false;
        }
        true
    }

    fn select_path(&mut self, mut path: SelectedPath) -> Result<(), TransportError> {
        let previous = self
            .selected
            .as_ref()
            .map(|selected| selected.envelope.clone());
        let epoch = self.advance_path_epoch()?;
        path.epoch = Some(epoch);
        self.selected = Some(path);
        self.transmissions.retain(|prepared| {
            prepared.kind != DatagramKind::Rtp || prepared.path_epoch == Some(epoch)
        });
        self.push_event(TransportEvent::SelectedPathChanged {
            previous,
            current: self
                .selected
                .as_ref()
                .map(|selected| selected.envelope.clone()),
            epoch,
        })
    }

    fn clear_selected_path(&mut self) -> Result<(), TransportError> {
        let Some(previous) = self.selected.take() else {
            return Ok(());
        };
        let epoch = self.advance_path_epoch()?;
        self.transmissions
            .retain(|prepared| prepared.kind != DatagramKind::Rtp);
        self.push_event(TransportEvent::SelectedPathChanged {
            previous: Some(previous.envelope),
            current: None,
            epoch,
        })
    }

    fn advance_path_epoch(&mut self) -> Result<PathEpoch, TransportError> {
        let Some(epoch) = self.next_path_epoch.next() else {
            return self.fail(TransportError::Protocol).map(|()| PathEpoch(0));
        };
        self.next_path_epoch = epoch;
        Ok(epoch)
    }

    fn can_queue_path(&self, path: &SelectedPath) -> bool {
        self.ice
            .can_queue_tuple(path.source(), path.destination(), path.protocol())
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

    pub(crate) fn can_admit_ssrcs(&self, identities: &[u32]) -> bool {
        identities.len() <= 512
            && self
                .srtp
                .as_ref()
                .is_none_or(|srtp| srtp.can_admit_ssrcs(identities))
    }

    pub(crate) fn configure_rtp_mappings(
        &mut self,
        inbound: Box<[u8]>,
        outbound: Box<[u8]>,
        twcc: Box<[(u8, u8)]>,
        identities: Box<[u32]>,
    ) {
        self.inbound_payload_types = inbound;
        self.outbound_payload_types = outbound;
        self.twcc_extension_id = None;
        self.twcc_payload_map = twcc;
        self.allowed_inbound_ssrcs = Some(identities);
    }

    fn accepts_payload_type(&self, payload_type: u8) -> bool {
        self.outbound_payload_types.is_empty()
            || self.outbound_payload_types.contains(&payload_type)
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
    fn secure_transport_preserves_caller_indices_arrivals_and_crypto_boundaries() {
        let left_certificate = certificate();
        let right_certificate = certificate();
        let mut now = Instant::now();
        let mut left = Transport::new(
            config(
                address(4000),
                address(4001),
                left_certificate.clone(),
                &right_certificate,
                true,
            ),
            now,
        )
        .unwrap();
        let mut right = Transport::new(
            config(
                address(4001),
                address(4000),
                right_certificate,
                &left_certificate,
                false,
            ),
            now,
        )
        .unwrap();
        connect(&mut left, &mut right, &mut now);
        assert_eq!(left.negotiated_security(), right.negotiated_security());
        assert!(left.negotiated_security().is_some());
        let mut replay = None;
        for index in [65_534_u64, 65_536, 65_535, 65_538, 65_537] {
            let mut packet = vec![0xa0, 96];
            packet.extend_from_slice(&u16::try_from(index & 0xffff).unwrap().to_be_bytes());
            packet.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 7, 9, 8, 7, 0, 0, 3]);
            left.send_rtp_with_info(&packet, index, crate::TransmitKind::Original { ssrc: 7 })
                .expect("caller-indexed SRTP");
            let prepared = left.poll_prepared().expect("protected packet");
            assert_eq!(prepared.wire_len, prepared.bytes.len());
            assert_eq!(prepared.rtp.unwrap().index, index);
            if replay.is_none() {
                replay = Some(prepared.bytes.clone());
            }
            let TransmitTarget::Udp { local, remote, .. } = prepared.target else {
                panic!("UDP fixture");
            };
            right
                .receive(
                    now,
                    now,
                    NetworkInput::Udp {
                        local: remote,
                        remote: local,
                        ecn: None,
                        payload: prepared.bytes.into(),
                    },
                )
                .unwrap();
            let event = std::iter::from_fn(|| right.poll_event())
                .find(|event| matches!(event, TransportEvent::Rtp { .. }))
                .expect("authenticated late/wrapped primary");
            assert!(
                matches!(event, TransportEvent::Rtp { bytes, arrival, .. } if bytes == packet && arrival == now)
            );
        }
        right
            .handle_datagram(now, address(4000), address(4001), replay.unwrap())
            .expect("ciphertext replay is dropped");
        assert!(
            !std::iter::from_fn(|| right.poll_event())
                .any(|event| matches!(event, TransportEvent::Rtp { .. }))
        );

        let rtcp = vec![0x80, 201, 0, 1, 0, 0, 0, 7];
        left.send_rtcp(&rtcp).expect("SRTCP");
        let packet = left.poll_transmit().unwrap();
        let arrival = now;
        now += Duration::from_millis(50);
        right
            .receive(
                now,
                arrival,
                NetworkInput::Udp {
                    local: packet.destination,
                    remote: packet.source,
                    ecn: None,
                    payload: packet.bytes.into(),
                },
            )
            .unwrap();
        assert_eq!(right.last_now, Some(now));
        assert!(std::iter::from_fn(|| right.poll_event()).any(|event| matches!(event, TransportEvent::Rtcp { bytes, arrival: at, .. } if bytes == rtcp && at == arrival)));

        let packet = [0x80, 96, 0, 3, 0, 0, 0, 1, 0, 0, 0, 8, 1];
        left.send_rtp_with_info(&packet, 3, crate::TransmitKind::Original { ssrc: 8 })
            .unwrap();
        let mut encrypted = left.poll_transmit().unwrap();
        *encrypted.bytes.last_mut().unwrap() ^= 1;
        assert_eq!(
            right.handle_datagram(
                now,
                encrypted.source,
                encrypted.destination,
                encrypted.bytes
            ),
            Err(TransportError::Crypto)
        );
        assert_eq!(right.state(), TransportState::Failed);
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
        .with_rtp_payload_types(Box::new([]), vec![63, 96].into_boxed_slice());
        assert!(valid_config.validate().is_ok());

        let invalid_config = config(
            address(20000),
            address(20001),
            certificate(),
            &right_certificate,
            true,
        )
        .with_rtp_payload_types(Box::new([]), vec![64].into_boxed_slice());
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
        .with_rtp_payload_types(vec![95].into_boxed_slice(), Box::new([]));
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
        .with_rtp_payload_types(vec![96].into_boxed_slice(), vec![96].into_boxed_slice());
        assert!(valid_config.validate().is_ok());
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
