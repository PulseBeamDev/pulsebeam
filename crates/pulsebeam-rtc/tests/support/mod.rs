#![allow(
    dead_code,
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::large_enum_variant,
    clippy::panic,
    unreachable_patterns,
    reason = "deterministic test fixture failures should stop at their violated invariant"
)]

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use bytes::Bytes;
use pulsebeam_rtc::{
    Command, Connection, ConnectionConfig, ConnectionEntropy, DataChannelEvent, DataChannelId,
    DataMessage, Event as ConnectionEvent, GlobalMediaTime, IceTcpFlowId, LocalCandidate,
    MediaPacket, MediaPayloadBitrate, NetworkInput, Output as ConnectionOutput, SdpOffer, SenderId,
    TimePoint, TransmitTarget,
};
use str0m_reference::{
    Candidate, Event, Input, Output, Rtc,
    change::SdpAnswer,
    media::{Direction, MediaKind, Mid},
    net::{Protocol, Receive, TcpType},
    rtp::{RawPacket, RtpHeader, rtcp::Rtcp},
};

pub struct PeerFixture {
    pub connection: Connection,
    pub sender: SenderId,
    pub sender_mid: String,
    peer: Rtc,
    mid: Mid,
    now: Instant,
    start: Instant,
    connection_addr: SocketAddr,
    peer_addr: SocketAddr,
    transport: FixtureTransport,
    peer_drained: bool,
    connection_idle: bool,
    peer_connected: bool,
    connection_connected: bool,
    twcc_sent: usize,
    peer_channel: Option<str0m_reference::channel::ChannelId>,
}

impl PeerFixture {
    pub fn connected() -> Self {
        Self::connected_with(FixtureTransport::Udp, None, false)
    }

    pub fn connected_tcp() -> Self {
        Self::connected_with(FixtureTransport::Tcp, None, false)
    }

    pub fn connected_with_media_limit(max_queued_media_bytes: usize) -> Self {
        Self::connected_with(FixtureTransport::Udp, Some(max_queued_media_bytes), false)
    }

    pub fn connected_datachannels() -> Self {
        Self::connected_with(FixtureTransport::Udp, None, true)
    }

    pub fn unconnected() -> Self {
        Self::new_with(FixtureTransport::Udp, None, false)
    }

    fn connected_with(
        transport: FixtureTransport,
        media_limit: Option<usize>,
        datachannels: bool,
    ) -> Self {
        let mut fixture = Self::new_with(transport, media_limit, datachannels);
        for _ in 0..2_000 {
            let _ = fixture.step();
            if fixture.peer_connected && fixture.connection_connected {
                return fixture;
            }
        }
        panic!("standards peer did not connect");
    }

    fn new_with(
        transport: FixtureTransport,
        media_limit: Option<usize>,
        datachannels: bool,
    ) -> Self {
        let start = Instant::now();
        str0m_reference::crypto::from_feature_flags().install_process_default();
        let connection_addr = SocketAddr::from(([127, 0, 0, 1], 41000));
        let peer_addr = SocketAddr::from(([127, 0, 0, 1], 41001));
        let mut peer = Rtc::builder().enable_raw_packets(true).build(start);
        let candidate = match transport {
            FixtureTransport::Udp => Candidate::host(peer_addr, "udp").expect("peer candidate"),
            FixtureTransport::Tcp => Candidate::builder()
                .tcp()
                .host(peer_addr)
                .tcptype(TcpType::Active)
                .build()
                .expect("active peer candidate"),
        };
        peer.add_local_candidate(candidate);
        drain_peer(&mut peer);
        let mut change = peer.sdp_api();
        let mid = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let peer_channel = datachannels.then(|| change.add_channel("peer-opened".into()));
        let (offer, pending) = change.apply().expect("peer offer");
        let mut config = ConnectionConfig {
            local_candidates: vec![match transport {
                FixtureTransport::Udp => LocalCandidate::Udp(connection_addr),
                FixtureTransport::Tcp => LocalCandidate::TcpPassive(connection_addr),
            }],
            ..ConnectionConfig::default()
        };
        config.default_audio_policy.desired_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
        if let Some(limit) = media_limit {
            config.limits.max_queued_media_bytes = limit;
        }
        let accepted = Connection::accept(
            config,
            SdpOffer::new(offer.to_sdp_string()),
            TimePoint {
                monotonic: start,
                global: GlobalMediaTime::from_micros(1_000_000),
            },
            ConnectionEntropy::new([11; 32]),
        )
        .expect("PulseBeam accepts standards peer offer");
        let sender = accepted.session.senders[0].id;
        let sender_mid = accepted.session.senders[0].mid.to_string();
        let answer = SdpAnswer::from_sdp_string(accepted.answer.as_str())
            .expect("peer parses PulseBeam answer");
        peer.sdp_api()
            .accept_answer(pending, answer)
            .expect("peer accepts PulseBeam answer");
        peer.direct_api().enable_twcc_feedback();
        Self {
            connection: accepted.connection,
            sender,
            sender_mid,
            peer,
            mid,
            now: start,
            start,
            connection_addr,
            peer_addr,
            transport,
            peer_drained: false,
            connection_idle: false,
            peer_connected: false,
            connection_connected: false,
            twcc_sent: 0,
            peer_channel,
        }
    }

    pub fn expire_connection(&mut self) {
        for _ in 0..2_000 {
            match self.connection.poll(self.at()) {
                ConnectionOutput::Closed(_) => return,
                ConnectionOutput::Idle {
                    next_wakeup: Some(deadline),
                } => self.now = deadline,
                ConnectionOutput::Idle { next_wakeup: None } => {
                    self.now = self
                        .now
                        .checked_add(Duration::from_secs(1))
                        .expect("fixture clock");
                }
                _ => {}
            }
        }
        panic!("PulseBeam connection did not time out");
    }

    pub fn send_source(&mut self, payload: &[u8]) -> MediaPacket {
        let pt = self
            .peer
            .writer(self.mid)
            .expect("negotiated writer")
            .payload_params()
            .next()
            .expect("negotiated payload")
            .pt();
        self.peer
            .writer(self.mid)
            .expect("negotiated writer")
            .write(
                pt,
                self.now,
                self.now.duration_since(self.start).into(),
                payload,
            )
            .expect("peer writes source RTP");
        self.peer_drained = false;
        for _ in 0..2_000 {
            if let Some(PeerEvent::Inbound(packet)) = self.step() {
                return packet;
            }
        }
        panic!("PulseBeam did not authenticate peer RTP");
    }

    pub fn receive_egress(&mut self) -> (RtpHeader, Vec<u8>) {
        for _ in 0..2_000 {
            if let Some(PeerEvent::Outbound(header, payload)) = self.step() {
                return (header, payload);
            }
        }
        panic!("peer did not authenticate PulseBeam RTP");
    }

    pub fn at(&self) -> TimePoint {
        TimePoint {
            monotonic: self.now,
            global: GlobalMediaTime::from_micros(
                1_000_000
                    + u64::try_from(self.now.duration_since(self.start).as_micros())
                        .unwrap_or(u64::MAX),
            ),
        }
    }

    pub fn drive_for(&mut self, duration: Duration) {
        let deadline = self.now.checked_add(duration).expect("fixture deadline");
        while self.now < deadline {
            let _ = self.step();
        }
    }

    pub fn twcc_sent(&self) -> usize {
        self.twcc_sent
    }

    pub fn command(&mut self, command: Command) {
        self.connection
            .command(self.at(), command)
            .expect("connection command");
        self.connection_idle = false;
    }

    pub fn peer_send(&mut self, binary: bool, payload: &[u8]) {
        let id = self.peer_channel.expect("fixture peer channel");
        let accepted = self
            .peer
            .channel(id)
            .expect("peer channel open")
            .write(binary, payload)
            .expect("peer channel write");
        assert!(accepted, "peer accepts data channel payload");
        self.peer_drained = false;
    }

    pub fn next_data_event(&mut self) -> FixtureDataEvent {
        for _ in 0..4_000 {
            if let Some(event) = self.step().and_then(PeerEvent::into_data) {
                return event;
            }
        }
        panic!("data channel event was not produced");
    }

    pub fn next_coexistence_event(&mut self) -> FixtureCoexistenceEvent {
        for _ in 0..8_000 {
            match self.step() {
                Some(PeerEvent::Outbound(_, _)) => return FixtureCoexistenceEvent::Rtp,
                Some(PeerEvent::PeerMessage { .. }) => return FixtureCoexistenceEvent::Data,
                _ => {}
            }
        }
        panic!("coexistence traffic made no progress");
    }

    fn step(&mut self) -> Option<PeerEvent> {
        if !self.peer_drained {
            match self.peer.poll_output().expect("peer poll") {
                Output::Transmit(transmit) => {
                    let bytes: &[u8] = &transmit.contents;
                    let payload = match self.transport {
                        FixtureTransport::Udp => Bytes::copy_from_slice(bytes),
                        FixtureTransport::Tcp => {
                            let mut frame = u16::try_from(bytes.len())
                                .expect("test datagram fits RFC 4571")
                                .to_be_bytes()
                                .to_vec();
                            frame.extend_from_slice(bytes);
                            Bytes::from(frame)
                        }
                    };
                    self.connection
                        .receive(
                            self.at(),
                            match self.transport {
                                FixtureTransport::Udp => NetworkInput::Udp {
                                    local: transmit.destination,
                                    remote: transmit.source,
                                    ecn: None,
                                    payload,
                                },
                                FixtureTransport::Tcp => NetworkInput::IceTcp {
                                    flow: IceTcpFlowId::from_value(1),
                                    local: transmit.destination,
                                    remote: transmit.source,
                                    frame: payload,
                                },
                            },
                        )
                        .expect("PulseBeam receives peer datagram");
                    self.connection_idle = false;
                    return None;
                }
                Output::Event(Event::RawPacket(packet)) => {
                    match *packet {
                        RawPacket::RtpRx(header, payload) => {
                            return Some(PeerEvent::Outbound(header, payload));
                        }
                        RawPacket::RtcpTx(Rtcp::Twcc(_)) => {
                            self.twcc_sent = self.twcc_sent.saturating_add(1);
                        }
                        _ => {}
                    }
                    return None;
                }
                Output::Event(Event::Connected) => {
                    self.peer_connected = true;
                    return Some(PeerEvent::Connected);
                }
                Output::Event(Event::ChannelOpen(_, _)) => {
                    return Some(PeerEvent::PeerOpened);
                }
                Output::Event(Event::ChannelData(data)) => {
                    return Some(PeerEvent::PeerMessage {
                        binary: data.binary,
                        payload: data.data,
                    });
                }
                Output::Event(Event::ChannelClose(_)) => {
                    return Some(PeerEvent::PeerClosed);
                }
                Output::Event(_) => return None,
                Output::Timeout(_) => self.peer_drained = true,
            }
        }
        if !self.connection_idle {
            match self.connection.poll(self.at()) {
                ConnectionOutput::Transmit(transmit) => {
                    let (protocol, source, destination, payload) = match transmit.target {
                        TransmitTarget::Udp { local, remote, .. } => {
                            (Protocol::Udp, local, remote, transmit.payload.as_ref())
                        }
                        TransmitTarget::IceTcp { flow } => {
                            assert_eq!(flow, IceTcpFlowId::from_value(1));
                            (
                                Protocol::Tcp,
                                self.connection_addr,
                                self.peer_addr,
                                transmit.payload.get(2..).expect("complete RFC 4571 output"),
                            )
                        }
                    };
                    let receive = Receive::new(protocol, source, destination, payload)
                        .expect("peer classifies PulseBeam datagram");
                    self.peer
                        .handle_input(Input::Receive(self.now, receive))
                        .expect("peer receives PulseBeam datagram");
                    self.peer_drained = false;
                    return None;
                }
                ConnectionOutput::Event(ConnectionEvent::Connected) => {
                    self.connection_connected = true;
                    return Some(PeerEvent::Connected);
                }
                ConnectionOutput::Event(ConnectionEvent::Media { packet, .. }) => {
                    return Some(PeerEvent::Inbound(packet));
                }
                ConnectionOutput::Event(ConnectionEvent::DataChannel(event)) => {
                    return Some(PeerEvent::ConnectionData(event));
                }
                ConnectionOutput::Event(_) => return None,
                ConnectionOutput::Idle { .. } => self.connection_idle = true,
                ConnectionOutput::Closed(reason) => panic!("PulseBeam closed: {reason:?}"),
                _ => panic!("unexpected future PulseBeam output"),
            }
        }
        self.now = self
            .now
            .checked_add(Duration::from_millis(10))
            .expect("fixture clock");
        self.peer
            .handle_input(Input::Timeout(self.now))
            .expect("peer timeout");
        self.peer_drained = false;
        self.connection_idle = false;
        None
    }
}

pub fn second_negotiated_sender() -> SenderId {
    let start = Instant::now();
    let local = SocketAddr::from(([127, 0, 0, 1], 42000));
    let mut peer = Rtc::builder().build(start);
    peer.add_local_candidate(
        Candidate::host(SocketAddr::from(([127, 0, 0, 1], 42001)), "udp").expect("peer candidate"),
    );
    drain_peer(&mut peer);
    let mut change = peer.sdp_api();
    change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, _) = change.apply().expect("two-sender offer");
    Connection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(local)],
            ..ConnectionConfig::default()
        },
        SdpOffer::new(offer.to_sdp_string()),
        TimePoint {
            monotonic: start,
            global: GlobalMediaTime::from_micros(1),
        },
        ConnectionEntropy::new([19; 32]),
    )
    .expect("two-sender session")
    .session
    .senders[1]
        .id
}

enum PeerEvent {
    Connected,
    Inbound(MediaPacket),
    Outbound(RtpHeader, Vec<u8>),
    ConnectionData(DataChannelEvent),
    PeerOpened,
    PeerMessage { binary: bool, payload: Vec<u8> },
    PeerClosed,
}

impl PeerEvent {
    fn into_data(self) -> Option<FixtureDataEvent> {
        match self {
            Self::ConnectionData(DataChannelEvent::Opened { channel }) => {
                Some(FixtureDataEvent::ConnectionOpened(channel))
            }
            Self::ConnectionData(DataChannelEvent::Message { channel, message }) => {
                Some(FixtureDataEvent::ConnectionMessage { channel, message })
            }
            Self::ConnectionData(DataChannelEvent::Closed { channel }) => {
                Some(FixtureDataEvent::ConnectionClosed(channel))
            }
            Self::PeerOpened => Some(FixtureDataEvent::PeerOpened),
            Self::PeerMessage { binary, payload } => {
                Some(FixtureDataEvent::PeerMessage { binary, payload })
            }
            Self::PeerClosed => Some(FixtureDataEvent::PeerClosed),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum FixtureDataEvent {
    ConnectionOpened(DataChannelId),
    ConnectionMessage {
        channel: DataChannelId,
        message: DataMessage,
    },
    ConnectionClosed(DataChannelId),
    PeerOpened,
    PeerMessage {
        binary: bool,
        payload: Vec<u8>,
    },
    PeerClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixtureCoexistenceEvent {
    Rtp,
    Data,
}

#[derive(Clone, Copy)]
enum FixtureTransport {
    Udp,
    Tcp,
}

fn drain_peer(peer: &mut Rtc) {
    loop {
        if matches!(peer.poll_output().expect("peer drain"), Output::Timeout(_)) {
            return;
        }
    }
}
