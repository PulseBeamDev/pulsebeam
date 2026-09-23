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
    collections::VecDeque,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use pulsebeam_rtc::{
    Command, CommandError, Connection, ConnectionConfig, ConnectionEntropy, ConnectionLimits,
    DataChannelEvent, DataChannelId, DataMessage, Event as ConnectionEvent, ForwardedMedia,
    FrameBoundary, FrameDependencies, FrameId, FrameMetadata, GlobalMediaTime, IceTcpFlowId,
    LocalCandidate, MediaPacket, MediaPayloadBitrate, NetworkInput, Output as ConnectionOutput,
    SdpOffer, SenderId, TimePoint, TransmitTarget,
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
    pub senders: Vec<SenderId>,
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
    network: DeterministicNetwork,
    native_target: Option<fn(&Connection) -> Duration>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NetworkPolicy {
    pub delay: Duration,
    pub drop_every: Option<u64>,
    pub duplicate_every: Option<u64>,
    pub reorder_every: Option<u64>,
}

#[derive(Clone, Debug)]
enum PendingPacket {
    Connection {
        due: Instant,
        input: NetworkInput,
    },
    Peer {
        due: Instant,
        protocol: Protocol,
        source: SocketAddr,
        destination: SocketAddr,
        payload: Vec<u8>,
        rtp_bytes: u64,
        emitted_at: Instant,
    },
}

impl PendingPacket {
    fn due(&self) -> Instant {
        match self {
            Self::Connection { due, .. } | Self::Peer { due, .. } => *due,
        }
    }

    fn delay_by(&mut self, delay: Duration) {
        match self {
            Self::Connection { due, .. } | Self::Peer { due, .. } => *due += delay,
        }
    }
}

#[derive(Debug, Default)]
struct DeterministicNetwork {
    policy: NetworkPolicy,
    seed: u64,
    packets: u64,
    dropped: u64,
    duplicated: u64,
    reordered: u64,
    pending: VecDeque<PendingPacket>,
    trace: Vec<&'static str>,
    bottleneck_bps: Option<u64>,
    next_departure: Option<Instant>,
    queue_sojourn: Vec<Duration>,
    delivered_bytes: u64,
    last_committed_rtp_bytes: u64,
    last_committed_padding_bytes: u64,
    emitted_rtp: Vec<(Instant, u64, bool)>,
    delivered_rtp: Vec<(Instant, u64)>,
    time_quantum: Option<Duration>,
    shared_departure: Option<std::rc::Rc<std::cell::RefCell<Option<Instant>>>>,
    rtp_queue_samples: Vec<(Instant, Duration, Duration)>,
}

impl DeterministicNetwork {
    const MAX_PENDING: usize = 8_192;
    const MAX_TRACE: usize = 256;

    fn configure(&mut self, seed: u64, policy: NetworkPolicy) {
        self.policy = policy;
        self.seed = seed;
        self.packets = 0;
        self.dropped = 0;
        self.duplicated = 0;
        self.reordered = 0;
        self.trace.clear();
    }

    fn enqueue(&mut self, mut packet: PendingPacket) {
        self.packets = self.packets.saturating_add(1);
        let ordinal = self.packets.saturating_add(self.seed);
        if self.pending.len() >= Self::MAX_PENDING {
            self.dropped = self.dropped.saturating_add(1);
            self.record("queue-full");
            return;
        }
        if self
            .policy
            .drop_every
            .is_some_and(|period| period != 0 && ordinal.is_multiple_of(period))
        {
            self.dropped = self.dropped.saturating_add(1);
            self.record("drop");
            return;
        }
        let duplicate = self
            .policy
            .duplicate_every
            .is_some_and(|period| period != 0 && ordinal.is_multiple_of(period));
        let reorder = self
            .policy
            .reorder_every
            .is_some_and(|period| period != 0 && ordinal.is_multiple_of(period));
        let duplicate_packet = duplicate.then(|| packet.clone());
        if reorder {
            self.reordered = self.reordered.saturating_add(1);
            self.record("reorder");
            packet.delay_by(self.policy.delay.max(Duration::from_millis(1)));
        }
        self.pending.push_back(packet);
        if let Some(packet) = duplicate_packet {
            self.duplicated = self.duplicated.saturating_add(1);
            self.record("duplicate");
            if self.pending.len() < Self::MAX_PENDING {
                self.pending.push_back(packet);
            }
        }
    }

    fn record(&mut self, action: &'static str) {
        if self.trace.len() < Self::MAX_TRACE {
            self.trace.push(action);
        }
    }
}

impl PeerFixture {
    pub fn connected() -> Self {
        Self::connected_with(FixtureTransport::Udp, None, false)
    }

    pub fn connected_tcp() -> Self {
        Self::connected_with(FixtureTransport::Tcp, None, false)
    }

    pub fn connected_with_media_limit(max_queued_media_bytes: usize) -> Self {
        let limits = ConnectionLimits {
            max_queued_media_bytes,
            ..ConnectionLimits::default()
        };
        Self::connected_with(FixtureTransport::Udp, Some(limits), false)
    }

    pub fn connected_with_limits(limits: ConnectionLimits, datachannels: bool) -> Self {
        Self::connected_with(FixtureTransport::Udp, Some(limits), datachannels)
    }

    pub fn connected_with_senders(sender_count: usize) -> Self {
        Self::connect(FixtureTransport::Udp, None, false, sender_count)
    }

    pub fn connected_datachannels() -> Self {
        Self::connected_with(FixtureTransport::Udp, None, true)
    }

    pub fn unconnected() -> Self {
        Self::new_with(FixtureTransport::Udp, None, false, 1)
    }

    fn connected_with(
        transport: FixtureTransport,
        limits: Option<ConnectionLimits>,
        datachannels: bool,
    ) -> Self {
        Self::connect(transport, limits, datachannels, 1)
    }

    fn connect(
        transport: FixtureTransport,
        limits: Option<ConnectionLimits>,
        datachannels: bool,
        sender_count: usize,
    ) -> Self {
        let mut fixture = Self::new_with(transport, limits, datachannels, sender_count);
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
        limits: Option<ConnectionLimits>,
        datachannels: bool,
        sender_count: usize,
    ) -> Self {
        let start = Instant::now();
        str0m_reference::crypto::from_feature_flags().install_process_default();
        let connection_addr = SocketAddr::from(([127, 0, 0, 1], 41000));
        let peer_addr = SocketAddr::from(([127, 0, 0, 1], 41001));
        assert!((1..=128).contains(&sender_count), "fixture sender bound");
        let mut offer_attempts = 0;
        let (mut peer, mid, peer_channel, offer, pending) = loop {
            offer_attempts += 1;
            assert!(offer_attempts <= 16, "str0m generated duplicate MIDs");
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
            let mids = (0..sender_count)
                .map(|_| change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None))
                .collect::<Vec<_>>();
            if mids
                .iter()
                .enumerate()
                .any(|(index, mid)| mids[..index].contains(mid))
            {
                continue;
            }
            let mid = mids[0];
            let peer_channel = datachannels.then(|| change.add_channel("peer-opened".into()));
            let (offer, pending) = change.apply().expect("peer offer");
            break (peer, mid, peer_channel, offer, pending);
        };
        let mut config = ConnectionConfig {
            local_candidates: vec![match transport {
                FixtureTransport::Udp => LocalCandidate::Udp(connection_addr),
                FixtureTransport::Tcp => LocalCandidate::TcpPassive(connection_addr),
            }],
            ..ConnectionConfig::default()
        };
        config.default_audio_policy.desired_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
        if let Some(limits) = limits {
            config.limits = limits;
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
        let senders = accepted
            .session
            .senders
            .iter()
            .map(|sender| sender.id)
            .collect::<Vec<_>>();
        let sender = senders[0];
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
            senders,
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
            network: DeterministicNetwork::default(),
            native_target: None,
        }
    }

    pub fn configure_network(&mut self, seed: u64, policy: NetworkPolicy) {
        assert!(self.network.pending.is_empty(), "network must be drained");
        self.network.configure(seed, policy);
    }

    pub fn observe_native_target(&mut self, reader: fn(&Connection) -> Duration) {
        self.native_target = Some(reader);
    }

    pub fn rtp_queue_samples(&self) -> &[(Instant, Duration, Duration)] {
        &self.network.rtp_queue_samples
    }

    pub fn configure_time_quantum(&mut self, quantum: Duration) {
        assert!(!quantum.is_zero());
        self.network.time_quantum = Some(quantum);
    }

    pub fn share_bottleneck_departure(
        &mut self,
        departure: std::rc::Rc<std::cell::RefCell<Option<Instant>>>,
    ) {
        self.network.shared_departure = Some(departure);
    }

    pub fn set_bottleneck_rate(&mut self, bits_per_second: u64) {
        assert!(bits_per_second > 0);
        assert!(self.network.bottleneck_bps.is_some());
        self.network.bottleneck_bps = Some(bits_per_second);
    }

    pub fn configure_bottleneck(&mut self, bits_per_second: u64) {
        assert!(bits_per_second > 0);
        self.network.bottleneck_bps = Some(bits_per_second);
        self.network.next_departure = None;
        self.network.queue_sojourn.clear();
        self.network.delivered_bytes = 0;
        self.network.last_committed_rtp_bytes =
            self.connection.stats().connection.transmitted_rtp_bytes;
    }

    pub fn emitted_rtp(&self) -> &[(Instant, u64, bool)] {
        &self.network.emitted_rtp
    }

    pub fn delivered_rtp(&self) -> &[(Instant, u64)] {
        &self.network.delivered_rtp
    }

    pub fn bottleneck_samples(&self) -> (&[Duration], u64) {
        (&self.network.queue_sojourn, self.network.delivered_bytes)
    }

    pub fn network_counters(&self) -> (u64, u64, u64, u64) {
        (
            self.network.packets,
            self.network.dropped,
            self.network.duplicated,
            self.network.reordered,
        )
    }

    pub fn network_trace(&self) -> &[&'static str] {
        &self.network.trace
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
        self.try_command(command).expect("connection command");
    }

    pub fn try_command(&mut self, command: Command) -> Result<(), CommandError> {
        let result = self.connection.command(self.at(), command);
        if result.is_ok() {
            self.connection_idle = false;
        }
        result
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
        if let Some(index) = self
            .network
            .pending
            .iter()
            .position(|packet| packet.due() <= self.now)
        {
            match self.network.pending.remove(index).expect("due packet") {
                PendingPacket::Connection { input, .. } => {
                    self.connection
                        .receive(self.at(), input)
                        .expect("PulseBeam receives peer datagram");
                    self.connection_idle = false;
                }
                PendingPacket::Peer {
                    protocol,
                    source,
                    destination,
                    payload,
                    rtp_bytes,
                    emitted_at,
                    ..
                } => {
                    if self.network.bottleneck_bps.is_some() {
                        self.network.delivered_bytes =
                            self.network.delivered_bytes.saturating_add(rtp_bytes);
                        if rtp_bytes > 0 {
                            self.network.delivered_rtp.push((emitted_at, rtp_bytes));
                        }
                    }
                    let receive = Receive::new(protocol, source, destination, &payload)
                        .expect("peer classifies PulseBeam datagram");
                    self.peer
                        .handle_input(Input::Receive(self.now, receive))
                        .expect("peer receives PulseBeam datagram");
                    self.peer_drained = false;
                }
            }
            return None;
        }
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
                    let input = match self.transport {
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
                    };
                    self.network.enqueue(PendingPacket::Connection {
                        due: self.now + self.network.policy.delay,
                        input,
                    });
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
                    let due = if let Some(rate) = self.network.bottleneck_bps {
                        let departure = self
                            .network
                            .shared_departure
                            .as_ref()
                            .map_or(self.network.next_departure, |shared| *shared.borrow())
                            .unwrap_or(self.now)
                            .max(self.now);
                        let nanos = (payload.len() as u128)
                            .saturating_mul(8_000_000_000)
                            .div_ceil(u128::from(rate));
                        let service =
                            Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
                        let end = departure.checked_add(service).expect("bottleneck clock");
                        if let Some(shared) = &self.network.shared_departure {
                            *shared.borrow_mut() = Some(end);
                        } else {
                            self.network.next_departure = Some(end);
                        }
                        self.network
                            .queue_sojourn
                            .push(end.saturating_duration_since(self.now));
                        end + self.network.policy.delay
                    } else {
                        self.now + self.network.policy.delay
                    };
                    let stats = self.connection.stats().connection;
                    let committed_rtp = stats.transmitted_rtp_bytes;
                    let rtp_bytes =
                        committed_rtp.saturating_sub(self.network.last_committed_rtp_bytes);
                    self.network.last_committed_rtp_bytes = committed_rtp;
                    let padding = stats
                        .transmitted_padding_bytes
                        .saturating_sub(self.network.last_committed_padding_bytes);
                    self.network.last_committed_padding_bytes = stats.transmitted_padding_bytes;
                    if rtp_bytes != 0 {
                        self.network.emitted_rtp.push((self.now, rtp_bytes, false));
                        if let (Some(reader), Some(rate)) =
                            (self.native_target, self.network.bottleneck_bps)
                        {
                            let service = Duration::from_nanos(
                                u64::try_from(
                                    (u128::from(rtp_bytes) * 8_000_000_000)
                                        .div_ceil(u128::from(rate)),
                                )
                                .unwrap_or(u64::MAX),
                            );
                            let departure = self
                                .network
                                .shared_departure
                                .as_ref()
                                .map_or(self.network.next_departure, |shared| *shared.borrow())
                                .unwrap_or(self.now);
                            self.network.rtp_queue_samples.push((
                                self.now,
                                departure.saturating_duration_since(self.now).max(service),
                                reader(&self.connection),
                            ));
                        }
                    }
                    if padding != 0 {
                        self.network.emitted_rtp.push((self.now, padding, true));
                    }
                    self.network.enqueue(PendingPacket::Peer {
                        due,
                        protocol,
                        source,
                        destination,
                        payload: payload.to_vec(),
                        rtp_bytes,
                        emitted_at: self.now,
                    });
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
            .checked_add(
                self.network
                    .pending
                    .iter()
                    .min_by_key(|packet| packet.due())
                    .map_or(Duration::from_millis(10), |packet| {
                        packet
                            .due()
                            .saturating_duration_since(self.now)
                            .max(Duration::from_millis(1))
                    })
                    .min(self.network.time_quantum.unwrap_or(Duration::MAX)),
            )
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

pub fn forwarded(packet: MediaPacket, id: u64) -> ForwardedMedia {
    ForwardedMedia {
        packet,
        frame: FrameMetadata {
            id: FrameId::from_value(id),
            boundary: FrameBoundary::Complete,
            random_access: true,
            discardable: false,
            dependencies: FrameDependencies::Known(Arc::from([])),
        },
    }
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
