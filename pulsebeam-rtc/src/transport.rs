use std::{
    collections::{HashMap, VecDeque},
    time::Instant,
};

use is::{Candidate, IceAgent, IceAgentEvent, IceConnectionState, IceCreds, Protocol};
use str0m::{
    crypto::dtls::{DtlsCert, DtlsOutput, DtlsVersion},
    crypto::{Fingerprint, from_feature_flags},
    dtls::Dtls,
    rtp_::{ExtensionMap, RtpHeader, SrtpContext},
};

use crate::{
    CongestionEstimate, ConnectionId, DataChannelAssociation, EgressCongestion, ForwardedRtp, Gcc,
    GccError, GccOutcome, IngressPacket, MediaSectionId, NegotiatedMediaSection, NegotiatedSession,
    PacketError, PacketProvenance, PacketView, SendId, TransportMetadata, TransportProtocol,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressDatagram {
    bytes: Vec<u8>,
    transport: TransportMetadata,
    send_id: Option<SendId>,
}

const EGRESS_CAPACITY: usize = 256;

impl EgressDatagram {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn transport(&self) -> TransportMetadata {
        self.transport
    }

    pub const fn send_id(&self) -> Option<SendId> {
        self.send_id
    }

    pub fn into_parts(self) -> (Vec<u8>, TransportMetadata, Option<SendId>) {
        (self.bytes, self.transport, self.send_id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportEvent {
    IceChecking,
    IceConnected,
    IceDisconnected,
    IceFailed,
    DtlsConnected,
    DtlsClosed,
}

pub struct LocalTransport {
    ice: crate::IceCredentials,
    fingerprint: crate::DtlsFingerprint,
    certificate: DtlsCert,
}

impl LocalTransport {
    pub fn generate(ice: crate::IceCredentials) -> Result<Self, LiveConnectionError> {
        let crypto = from_feature_flags();
        let certificate = crypto
            .dtls_provider
            .generate_certificate()
            .ok_or(LiveConnectionError::Certificate)?;
        let fingerprint = crate::DtlsFingerprint::new(
            "sha-256".to_owned(),
            crypto
                .sha256_provider
                .sha256(&certificate.certificate)
                .to_vec()
                .into_boxed_slice(),
        )
        .ok_or(LiveConnectionError::Certificate)?;
        Ok(Self {
            ice,
            fingerprint,
            certificate,
        })
    }

    pub fn ice(&self) -> &crate::IceCredentials {
        &self.ice
    }

    pub fn fingerprint(&self) -> &crate::DtlsFingerprint {
        &self.fingerprint
    }
}

#[derive(Clone, Debug)]
pub struct AuthenticatedPacket {
    bytes: Vec<u8>,
    provenance: PacketProvenance,
}

impl AuthenticatedPacket {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn provenance(&self) -> PacketProvenance {
        self.provenance
    }

    pub fn parse(&self) -> Result<PacketView<'_>, PacketError> {
        IngressPacket::new(&self.bytes, self.provenance).parse()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LiveConnectionError {
    #[error("invalid ICE candidate: {0}")]
    Candidate(String),
    #[error("no DTLS certificate is available from the configured crypto provider")]
    Certificate,
    #[error("local transport facts do not match the negotiated session")]
    LocalTransportMismatch,
    #[error("DTLS initialization failed: {0}")]
    Dtls(String),
    #[error("received an unsupported datagram")]
    UnsupportedDatagram,
    #[error("transport is not ready for protected media")]
    CryptoNotReady,
    #[error("egress datagram capacity is exhausted")]
    EgressFull,
    #[error("invalid RTP header")]
    RtpHeader,
    #[error("SRTP authentication failed")]
    SrtpAuthentication,
    #[error("SRTCP authentication failed")]
    SrtcpAuthentication,
    #[error("invalid datagram: {0}")]
    Datagram(#[from] PacketError),
    #[error("congestion control error: {0}")]
    Congestion(#[from] GccError),
}

pub struct LiveConnection {
    id: ConnectionId,
    session: NegotiatedSession,
    crypto: str0m::crypto::CryptoProvider,
    ice: IceAgent,
    dtls: Dtls,
    srtp_rx: Option<SrtpContext>,
    srtp_tx: Option<SrtpContext>,
    data: Option<DataChannelAssociation>,
    rtp_rx: HashMap<u32, RtpReceiveIndex>,
    events: VecDeque<TransportEvent>,
    egress: VecDeque<EgressDatagram>,
    authenticated: VecDeque<AuthenticatedPacket>,
    dtls_buf: Box<[u8]>,
    next_dtls_deadline: Option<Instant>,
    nominated: Option<TransportMetadata>,
    gcc: Gcc,
    congestion: VecDeque<GccOutcome>,
}

#[derive(Clone, Copy, Debug, Default)]
struct RtpReceiveIndex {
    rollover_counter: u32,
    highest_sequence: u16,
    initialized: bool,
}

impl RtpReceiveIndex {
    fn extended_sequence(self, sequence: u16) -> u64 {
        let rollover_counter = if self.initialized
            && sequence < self.highest_sequence
            && self.highest_sequence.wrapping_sub(sequence) > (u16::MAX / 2)
        {
            self.rollover_counter.saturating_add(1)
        } else if self.initialized
            && sequence > self.highest_sequence
            && sequence.wrapping_sub(self.highest_sequence) > (u16::MAX / 2)
        {
            self.rollover_counter.saturating_sub(1)
        } else {
            self.rollover_counter
        };
        (u64::from(rollover_counter) << 16) | u64::from(sequence)
    }

    fn accept(&mut self, extended_sequence: u64) {
        let rollover_counter = (extended_sequence >> 16) as u32;
        let sequence = extended_sequence as u16;
        if !self.initialized
            || rollover_counter > self.rollover_counter
            || (rollover_counter == self.rollover_counter && sequence > self.highest_sequence)
        {
            self.rollover_counter = rollover_counter;
            self.highest_sequence = sequence;
            self.initialized = true;
        }
    }
}

impl LiveConnection {
    pub fn new(
        id: ConnectionId,
        session: NegotiatedSession,
        local: LocalTransport,
        now: Instant,
    ) -> Result<Self, LiveConnectionError> {
        let crypto = from_feature_flags();
        if local.ice != *session.local_ice() || local.fingerprint != *session.local_fingerprint() {
            return Err(LiveConnectionError::LocalTransportMismatch);
        }
        let mut ice = IceAgent::with_hmac(
            IceCreds {
                ufrag: local.ice.ufrag().to_owned(),
                pass: local.ice.password().to_owned(),
            },
            crypto.sha1_hmac_provider,
        );
        ice.set_ice_lite(true);
        ice.set_controlling(false);
        ice.set_remote_credentials(IceCreds {
            ufrag: session.remote_ice().ufrag().to_owned(),
            pass: session.remote_ice().password().to_owned(),
        });
        for candidate in session.local_candidates() {
            let candidate = Candidate::from_sdp_string(candidate.as_sdp())
                .map_err(|error| LiveConnectionError::Candidate(error.to_string()))?;
            let _ = ice.add_local_candidate(candidate);
        }
        for candidate in session.remote_candidates() {
            let candidate = Candidate::from_sdp_string(candidate.as_sdp())
                .map_err(|error| LiveConnectionError::Candidate(error.to_string()))?;
            ice.add_remote_candidate(candidate);
        }
        let mut dtls = Dtls::new(
            &local.certificate,
            crypto.dtls_provider,
            crypto.sha256_provider,
            now,
            DtlsVersion::Auto,
            1200..=1500,
        )
        .map_err(|error| LiveConnectionError::Dtls(error.to_string()))?;
        dtls.set_active(false);
        let data = session
            .media_sections()
            .iter()
            .find_map(|section| section.data_channel())
            .map(|parameters| {
                DataChannelAssociation::new("pulsebeam", parameters.sctp_port(), now, 64, 64)
            });
        Ok(Self {
            id,
            session,
            crypto,
            ice,
            dtls,
            srtp_rx: None,
            srtp_tx: None,
            data,
            rtp_rx: HashMap::new(),
            events: VecDeque::new(),
            egress: VecDeque::with_capacity(EGRESS_CAPACITY),
            authenticated: VecDeque::new(),
            dtls_buf: vec![0; 2048].into_boxed_slice(),
            next_dtls_deadline: None,
            nominated: None,
            gcc: Gcc::new(EGRESS_CAPACITY.saturating_mul(4)),
            congestion: VecDeque::with_capacity(64),
        })
    }

    pub const fn id(&self) -> ConnectionId {
        self.id
    }

    pub fn session(&self) -> &NegotiatedSession {
        &self.session
    }

    pub fn media_section(&self, id: MediaSectionId) -> Option<&NegotiatedMediaSection> {
        self.session.media_section(id)
    }

    pub fn media_section_by_mid(&self, mid: &str) -> Option<&NegotiatedMediaSection> {
        self.session.media_section_by_mid(mid)
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        self.ice.handle_timeout(now);
        if self
            .next_dtls_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            let _ = self.dtls.handle_timeout(now);
        }
        self.drive_data(now);
    }

    pub fn handle_datagram(
        &mut self,
        now: Instant,
        packet: IngressPacket<'_>,
    ) -> Result<(), LiveConnectionError> {
        let bytes = packet.bytes();
        let first = *bytes.first().ok_or(PacketError::Empty)?;
        if first < 2 {
            let message = is::stun::StunMessage::parse(bytes)
                .map_err(|_| LiveConnectionError::UnsupportedDatagram)?;
            let transport = packet.provenance().transport();
            let proto = match transport.protocol() {
                TransportProtocol::Udp => Protocol::Udp,
                TransportProtocol::Tcp => Protocol::Tcp,
            };
            self.ice.handle_packet(
                now,
                is::stun::StunPacket {
                    proto,
                    source: transport.source(),
                    destination: transport.destination(),
                    message,
                },
            );
            self.drive_data(now);
            return Ok(());
        }

        if (20..64).contains(&first) {
            self.dtls
                .handle_receive(bytes)
                .map_err(|error| LiveConnectionError::Dtls(error.to_string()))?;
            self.drive_data(now);
            return Ok(());
        }

        if first >> 6 == 2 {
            if matches!(bytes.get(1), Some(192..=223)) {
                return self.receive_rtcp(packet);
            }
            return self.receive_rtp_auto(packet);
        }

        Err(LiveConnectionError::UnsupportedDatagram)
    }

    pub fn next_deadline(&mut self) -> Option<Instant> {
        let mut deadline = self.ice.poll_timeout();
        if let Some(dtls) = self.next_dtls_deadline {
            deadline = Some(deadline.map_or(dtls, |current| current.min(dtls)));
        }
        if let Some(data) = self
            .data
            .as_ref()
            .and_then(DataChannelAssociation::next_deadline)
        {
            deadline = Some(deadline.map_or(data, |current| current.min(data)));
        }
        deadline
    }

    pub fn poll_event(&mut self) -> Option<TransportEvent> {
        self.events.pop_front()
    }

    pub fn poll_egress(&mut self) -> Option<EgressDatagram> {
        self.egress.pop_front()
    }

    pub fn egress_ready(&self) -> bool {
        self.egress.len() < EGRESS_CAPACITY
    }

    pub fn poll_authenticated(&mut self) -> Option<AuthenticatedPacket> {
        self.authenticated.pop_front()
    }

    pub fn congestion_estimate(&self, now: Instant) -> CongestionEstimate {
        self.gcc.estimate(now)
    }

    pub fn poll_congestion(&mut self) -> Option<GccOutcome> {
        self.congestion.pop_front()
    }

    pub fn report_departure(
        &mut self,
        send_id: SendId,
        now: Instant,
    ) -> Result<(), LiveConnectionError> {
        self.gcc.record_departure(send_id, now)?;
        Ok(())
    }

    pub fn data_association(&mut self) -> Option<&mut DataChannelAssociation> {
        self.data.as_mut()
    }

    pub fn drive_data(&mut self, now: Instant) {
        if let Some(data) = self.data.as_mut() {
            while let Some(packet) = data.poll_egress() {
                if self.dtls.handle_input(&packet).is_err() {
                    self.events.push_back(TransportEvent::DtlsClosed);
                    return;
                }
            }
        }
        self.drain(now);
    }

    pub fn receive_rtp(
        &mut self,
        packet: IngressPacket<'_>,
        extended_sequence: u64,
    ) -> Result<(), LiveConnectionError> {
        let bytes = packet.bytes();
        let header = RtpHeader::parse(bytes, &ExtensionMap::empty())
            .ok_or(LiveConnectionError::RtpHeader)?;
        let plaintext = self
            .srtp_rx
            .as_mut()
            .ok_or(LiveConnectionError::CryptoNotReady)?
            .unprotect_rtp(bytes, &header, extended_sequence)
            .ok_or(LiveConnectionError::SrtpAuthentication)?;
        let mut decrypted = Vec::with_capacity(header.header_len.saturating_add(plaintext.len()));
        decrypted.extend_from_slice(&bytes[..header.header_len]);
        decrypted.extend_from_slice(plaintext);
        self.authenticated.push_back(AuthenticatedPacket {
            bytes: decrypted,
            provenance: packet.provenance(),
        });
        Ok(())
    }

    fn receive_rtp_auto(&mut self, packet: IngressPacket<'_>) -> Result<(), LiveConnectionError> {
        let header = RtpHeader::parse(packet.bytes(), &ExtensionMap::empty())
            .ok_or(LiveConnectionError::RtpHeader)?;
        let ssrc = *header.ssrc;
        let extended_sequence = self
            .rtp_rx
            .get(&ssrc)
            .copied()
            .unwrap_or_default()
            .extended_sequence(header.sequence_number);
        self.receive_rtp(packet, extended_sequence)?;
        self.rtp_rx
            .entry(ssrc)
            .or_default()
            .accept(extended_sequence);
        Ok(())
    }

    pub fn receive_rtcp(&mut self, packet: IngressPacket<'_>) -> Result<(), LiveConnectionError> {
        let plaintext = self
            .srtp_rx
            .as_mut()
            .ok_or(LiveConnectionError::CryptoNotReady)?
            .unprotect_rtcp(packet.bytes())
            .ok_or(LiveConnectionError::SrtcpAuthentication)?;
        let authenticated = AuthenticatedPacket {
            bytes: plaintext,
            provenance: packet.provenance(),
        };
        if let PacketView::Rtcp(rtcp) = authenticated.parse()? {
            for outcome in self
                .gcc
                .process_rtcp(packet.provenance().received_at(), &rtcp)?
            {
                if self.congestion.len() < self.congestion.capacity() {
                    self.congestion.push_back(outcome);
                }
            }
        }
        self.authenticated.push_back(authenticated);
        Ok(())
    }

    pub fn send_rtp(
        &mut self,
        bytes: &[u8],
        extended_sequence: u64,
    ) -> Result<(), LiveConnectionError> {
        let header = RtpHeader::parse(bytes, &ExtensionMap::empty())
            .ok_or(LiveConnectionError::RtpHeader)?;
        let encrypted = self
            .srtp_tx
            .as_mut()
            .ok_or(LiveConnectionError::CryptoNotReady)?
            .protect_rtp(bytes, &header, extended_sequence);
        self.push_protected_egress(encrypted, None)
    }

    pub fn send_rtp_with_congestion(
        &mut self,
        bytes: &[u8],
        extended_sequence: u64,
        send_id: SendId,
    ) -> Result<EgressCongestion, LiveConnectionError> {
        let congestion = self.gcc.assign(send_id, bytes.len())?;
        self.send_rtp_with_assigned_congestion(bytes, extended_sequence, send_id)?;
        Ok(congestion)
    }

    pub fn assign_congestion(
        &mut self,
        send_id: SendId,
        bytes: usize,
    ) -> Result<EgressCongestion, LiveConnectionError> {
        Ok(self.gcc.assign(send_id, bytes)?)
    }

    pub fn send_rtp_with_assigned_congestion(
        &mut self,
        bytes: &[u8],
        extended_sequence: u64,
        send_id: SendId,
    ) -> Result<(), LiveConnectionError> {
        let header = RtpHeader::parse(bytes, &ExtensionMap::empty())
            .ok_or(LiveConnectionError::RtpHeader)?;
        let encrypted = self
            .srtp_tx
            .as_mut()
            .ok_or(LiveConnectionError::CryptoNotReady)?
            .protect_rtp(bytes, &header, extended_sequence);
        self.push_protected_egress(encrypted, Some(send_id))?;
        Ok(())
    }

    pub fn send_forwarded_rtp(&mut self, packet: &ForwardedRtp) -> Result<(), LiveConnectionError> {
        self.send_rtp(packet.bytes(), packet.extended_sequence())
    }

    pub fn send_rtcp(&mut self, bytes: &[u8]) -> Result<(), LiveConnectionError> {
        let encrypted = self
            .srtp_tx
            .as_mut()
            .ok_or(LiveConnectionError::CryptoNotReady)?
            .protect_rtcp(bytes);
        self.push_protected_egress(encrypted, None)
    }

    fn push_protected_egress(
        &mut self,
        bytes: Vec<u8>,
        send_id: Option<SendId>,
    ) -> Result<(), LiveConnectionError> {
        let transport = self.nominated.ok_or(LiveConnectionError::CryptoNotReady)?;
        if !self.egress_ready() {
            return Err(LiveConnectionError::EgressFull);
        }
        self.egress.push_back(EgressDatagram {
            bytes,
            transport,
            send_id,
        });
        Ok(())
    }

    fn drain(&mut self, now: Instant) {
        while self.egress_ready() {
            let Some(transmit) = self.ice.poll_transmit() else {
                break;
            };
            let protocol = match transmit.proto {
                Protocol::Udp => TransportProtocol::Udp,
                _ => TransportProtocol::Tcp,
            };
            self.egress.push_back(EgressDatagram {
                bytes: transmit.contents.to_vec(),
                transport: TransportMetadata::new(protocol, transmit.source, transmit.destination),
                send_id: None,
            });
        }
        while let Some(event) = self.ice.poll_event() {
            let event = match event {
                IceAgentEvent::IceConnectionStateChange(state) => match state {
                    IceConnectionState::Checking => Some(TransportEvent::IceChecking),
                    IceConnectionState::Connected | IceConnectionState::Completed => {
                        Some(TransportEvent::IceConnected)
                    }
                    IceConnectionState::Disconnected => Some(TransportEvent::IceDisconnected),
                    IceConnectionState::New => None,
                },
                IceAgentEvent::NominatedSend {
                    proto,
                    source,
                    destination,
                } => {
                    let protocol = match proto {
                        Protocol::Udp => TransportProtocol::Udp,
                        _ => TransportProtocol::Tcp,
                    };
                    self.nominated = Some(TransportMetadata::new(protocol, source, destination));
                    None
                }
                _ => None,
            };
            if let Some(event) = event {
                self.events.push_back(event);
            }
        }
        loop {
            match self.dtls.poll_output(&mut self.dtls_buf) {
                DtlsOutput::Timeout(deadline) => {
                    self.next_dtls_deadline = Some(deadline);
                    break;
                }
                DtlsOutput::Connected => self.events.push_back(TransportEvent::DtlsConnected),
                DtlsOutput::CloseNotify => self.events.push_back(TransportEvent::DtlsClosed),
                DtlsOutput::PeerCert(peer) => {
                    let actual = Fingerprint {
                        hash_func: "sha-256".to_owned(),
                        bytes: self.crypto.sha256_provider.sha256(peer).to_vec(),
                    };
                    self.dtls.set_remote_fingerprint(actual.clone());
                    if actual.hash_func != self.session.remote_fingerprint().algorithm()
                        || actual.bytes != self.session.remote_fingerprint().value()
                    {
                        self.events.push_back(TransportEvent::DtlsClosed);
                        let _ = self.dtls.close();
                    }
                }
                DtlsOutput::KeyingMaterial(material, profile) => {
                    let active = self.dtls.is_active().unwrap_or(false);
                    self.srtp_rx =
                        Some(SrtpContext::new(&self.crypto, profile, &material, !active));
                    self.srtp_tx = Some(SrtpContext::new(&self.crypto, profile, &material, active));
                }
                DtlsOutput::ApplicationData(data) => {
                    if let Some(association) = self.data.as_mut() {
                        association.handle_input(now, data);
                    }
                }
                _ => {}
            }
            while self.egress_ready() {
                let Some(packet) = self.dtls.poll_packet() else {
                    break;
                };
                if let Some(transport) = self.nominated {
                    self.egress.push_back(EgressDatagram {
                        bytes: packet.to_vec(),
                        transport,
                        send_id: None,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Instant};

    use super::*;
    use crate::{IceCandidate, IceCredentials, ServerTransport, negotiate};

    fn local() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 9000))
    }

    fn remote() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 9001))
    }

    fn offer() -> String {
        "v=0\r\n\
         o=- 1 2 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         t=0 0\r\n\
         a=group:BUNDLE 0\r\n\
         a=ice-ufrag:remoteufrag\r\n\
         a=ice-pwd:remotepassword\r\n\
         a=fingerprint:sha-256 01:02:03:04\r\n\
         a=setup:actpass\r\n\
         a=candidate:2 1 UDP 2130706431 127.0.0.1 9001 typ host\r\n\
         m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
         c=IN IP4 0.0.0.0\r\n\
         a=mid:0\r\n\
         a=sendonly\r\n\
         a=rtcp-mux\r\n\
         a=rtpmap:111 opus/48000/2\r\n"
            .to_owned()
    }

    fn connection(now: Instant) -> LiveConnection {
        let local = LocalTransport::generate(
            IceCredentials::new("localufrag".to_owned(), "localpassword".to_owned())
                .expect("valid local credentials"),
        )
        .expect("local transport");
        let candidate =
            IceCandidate::new("candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host".to_owned())
                .expect("valid candidate");
        let server = ServerTransport::new(
            7,
            local.ice().clone(),
            local.fingerprint().clone(),
            Box::new([candidate]),
        );
        let session = negotiate(&offer(), &server)
            .expect("negotiated session")
            .session()
            .clone();
        LiveConnection::new(ConnectionId::new(7), session, local, now).expect("live connection")
    }

    fn provenance(now: Instant) -> PacketProvenance {
        PacketProvenance::new(
            now,
            TransportMetadata::new(TransportProtocol::Udp, remote(), local()),
            crate::PacketId::new(1),
        )
    }

    #[test]
    fn live_transport_rejects_invalid_datagrams_without_side_effects() {
        let now = Instant::now();
        let mut connection = connection(now);

        let error = connection
            .handle_datagram(now, IngressPacket::new(&[], provenance(now)))
            .expect_err("empty datagram is invalid");

        assert!(matches!(
            error,
            LiveConnectionError::Datagram(PacketError::Empty)
        ));
        assert!(connection.poll_egress().is_none());
        assert!(connection.poll_authenticated().is_none());
    }

    #[test]
    fn live_transport_reports_its_only_idle_work_as_a_deadline() {
        let now = Instant::now();
        let mut connection = connection(now);

        assert!(connection.next_deadline().is_none());
        connection.handle_timeout(now);
        let deadline = connection
            .next_deadline()
            .expect("ICE deadline after activation");
        debug_assert!(deadline >= now);
        assert_eq!(connection.poll_event(), Some(TransportEvent::IceChecking));
        assert!(connection.poll_egress().is_none());
        assert!(connection.poll_authenticated().is_none());
    }

    #[test]
    fn live_transport_progresses_a_valid_ice_request_synchronously() {
        let now = Instant::now();
        let mut connection = connection(now);
        let crypto = from_feature_flags();
        let mut peer = IceAgent::with_hmac(
            IceCreds {
                ufrag: "remoteufrag".to_owned(),
                pass: "remotepassword".to_owned(),
            },
            crypto.sha1_hmac_provider,
        );
        peer.set_controlling(true);
        peer.set_remote_credentials(IceCreds {
            ufrag: "localufrag".to_owned(),
            pass: "localpassword".to_owned(),
        });
        peer.add_local_candidate(
            Candidate::from_sdp_string("candidate:2 1 UDP 2130706431 127.0.0.1 9001 typ host")
                .expect("remote candidate"),
        );
        peer.add_remote_candidate(
            Candidate::from_sdp_string("candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host")
                .expect("local candidate"),
        );
        peer.handle_timeout(now);
        let request = peer.poll_transmit().expect("ICE request");

        connection
            .handle_datagram(now, IngressPacket::new(&request.contents, provenance(now)))
            .expect("valid ICE request");

        let response = connection.poll_egress().expect("ICE response");
        assert_eq!(response.transport().source(), local());
        assert_eq!(response.transport().destination(), remote());
        assert!(!response.bytes().is_empty());
    }
}
