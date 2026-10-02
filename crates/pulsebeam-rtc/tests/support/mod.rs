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
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use bytes::Bytes;
use pulsebeam_rtc::{
    Command, CommandError, Connection, ConnectionConfig, ConnectionEntropy, ConnectionLimits,
    DataChannelEvent, DataChannelId, DataMessage, Event as ConnectionEvent, ForwardedMedia,
    FrameBoundary, FrameDependencies, FrameId, FrameMetadata, GlobalMediaTime,
    LocalCandidate, MediaPacket, MediaPayloadBitrate, NetworkInput, Output as ConnectionOutput,
    SdpOffer, SenderId, TimePoint, TransmitTarget,
};
use pulsebeam_rtc_simulation::*;

pub struct PeerFixture {
    pub connection: Connection,
    pub sender: SenderId,
    pub senders: Vec<SenderId>,
    pub sender_mid: String,
    peer: PeerConnection,
    audio_source: Option<EncodedAudioSource>,
    video_source: Option<EncodedVideoSource>,
    audio_sink: Option<EncodedAudioSink>,
    video_sink: Option<EncodedVideoSink>,
    peer_channel: Option<DataChannel>,
    remote_channels: Vec<DataChannel>,
    network_adapter: ControlledSimulatedNetwork,
    socket: SimulatedUdpSocket,
    _factory: PeerConnectionFactory,
    _client: NetworkEndpoint,
    _server: NetworkEndpoint,
    now: Instant,
    start: Instant,
    source_timestamp: u32,
    headers: VecDeque<FixtureRtpHeader>,
    connection_idle: bool,
    peer_connected: bool,
    connection_connected: bool,
    twcc_sent: usize,
    feedback_enabled: bool,
    network: DeterministicNetwork,
    native_target: Option<fn(&Connection) -> Duration>,
    world: std::rc::Rc<SimulationWorld>,
}

#[test]
fn scenario_clock_binding_survives_later_peer_initialization() {
    let world = SimulationWorld::acquire();
    let start = Instant::now();
    world.bind_clock(start);
    let elapsed = Duration::from_millis(37);
    world.advance_clock_to(start + elapsed);
    world.bind_clock(start + Duration::from_secs(99));
    assert_eq!(world.monotonic_now(), start + elapsed);
}

#[test]
fn fixture_advance_stops_at_requested_deadline() {
    let mut fixture = PeerFixture::connected();
    fixture.configure_time_quantum(Duration::from_millis(5));
    let before = fixture.at().monotonic;
    fixture.drive_for(Duration::from_millis(1));
    assert_eq!(fixture.at().monotonic, before + Duration::from_millis(1));
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NetworkPolicy {
    pub delay: Duration,
    pub drop_every: Option<u64>,
    pub duplicate_every: Option<u64>,
    pub reorder_every: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub enum RtpImpairment {
    Random {
        per_mille: u64,
    },
    Burst {
        every: u64,
        length: u64,
    },
    Policer {
        bits_per_second: u64,
        burst_bytes: u64,
    },
}

#[derive(Clone, Debug)]
enum PendingPacket {
    Connection {
        due: Instant,
        input: NetworkInput,
    },
    Peer {
        due: Instant,
        destination: SocketAddr,
        payload: Vec<u8>,
        rtp_bytes: u64,
        emitted_at: Instant,
        queue_sample: Option<(Duration, Duration)>,
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
    impairment: Option<RtpImpairment>,
    impaired_packets: u64,
    impaired_drops: u64,
    policer_tokens: u128,
    policer_at: Option<Instant>,
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

    fn impair(&mut self, packet: &PendingPacket) -> bool {
        let PendingPacket::Peer { rtp_bytes, due, .. } = packet else {
            return false;
        };
        if *rtp_bytes == 0 {
            return false;
        }
        self.impaired_packets += 1;
        let drop = match self.impairment {
            None => false,
            Some(RtpImpairment::Random { per_mille }) => {
                let mut value = self
                    .impaired_packets
                    .wrapping_add(self.seed)
                    .wrapping_add(0x9e3779b97f4a7c15);
                value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
                value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
                (value ^ (value >> 31)) % 1_000 < per_mille
            }
            Some(RtpImpairment::Burst { every, length }) => self.impaired_packets % every < length,
            Some(RtpImpairment::Policer {
                bits_per_second,
                burst_bytes,
            }) => {
                // Downstream policer after shared-link service and propagation.
                // Drops consume upstream service; tokens retain fractional bytes.
                let capacity = u128::from(burst_bytes) * 8_000_000_000;
                self.policer_tokens = self.policer_at.map_or(capacity, |previous| {
                    (self.policer_tokens
                        + due.saturating_duration_since(previous).as_nanos()
                            * u128::from(bits_per_second))
                    .min(capacity)
                });
                self.policer_at = Some(*due);
                let cost = u128::from(*rtp_bytes) * 8_000_000_000;
                if cost <= self.policer_tokens {
                    self.policer_tokens -= cost;
                    false
                } else {
                    true
                }
            }
        };
        if drop {
            self.impaired_drops += 1;
        }
        drop
    }

    fn enqueue(&mut self, mut packet: PendingPacket) {
        self.packets = self.packets.saturating_add(1);
        let ordinal = self.packets.saturating_add(self.seed);
        if self.pending.len() >= Self::MAX_PENDING {
            self.dropped = self.dropped.saturating_add(1);
            self.record("queue-full");
            return;
        }
        if self.impair(&packet)
            || self
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
    fn default_start() -> Instant {
        // Construction time must not age media forwarded between virtual peers.
        static START: OnceLock<Instant> = OnceLock::new();
        *START.get_or_init(Instant::now)
    }

    pub fn connected() -> Self {
        Self::connected_with(FixtureTransport::Udp, None, false)
    }

    pub fn connected_at(start: Instant) -> Self {
        Self::connect_media_at(
            FixtureTransport::Udp,
            None,
            false,
            1,
            MediaKind::Audio,
            start,
        )
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
        Self::new_with(FixtureTransport::Udp, None, false, 1, MediaKind::Audio)
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
        Self::connect_media(
            transport,
            limits,
            datachannels,
            sender_count,
            MediaKind::Audio,
        )
    }

    pub fn connected_video() -> Self {
        Self::connect_media(FixtureTransport::Udp, None, false, 1, MediaKind::Video)
    }

    fn connect_media(
        transport: FixtureTransport,
        limits: Option<ConnectionLimits>,
        datachannels: bool,
        sender_count: usize,
        media: MediaKind,
    ) -> Self {
        Self::connect_media_at(
            transport,
            limits,
            datachannels,
            sender_count,
            media,
            Self::default_start(),
        )
    }

    fn connect_media_at(
        transport: FixtureTransport,
        limits: Option<ConnectionLimits>,
        datachannels: bool,
        sender_count: usize,
        media: MediaKind,
        start: Instant,
    ) -> Self {
        let mut fixture =
            Self::new_with_start(transport, limits, datachannels, sender_count, media, start);
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
        media: MediaKind,
    ) -> Self {
        Self::new_with_start(
            transport,
            limits,
            datachannels,
            sender_count,
            media,
            Self::default_start(),
        )
    }

    fn new_with_start(
        transport: FixtureTransport,
        limits: Option<ConnectionLimits>,
        datachannels: bool,
        sender_count: usize,
        media: MediaKind,
        start: Instant,
    ) -> Self {
        assert!(matches!(transport, FixtureTransport::Udp));
        assert!((1..=128).contains(&sender_count));
        let world = SimulationWorld::acquire();
        world.bind_clock(start);
        let network = world.controlled.create_network().expect("controlled network");
        let client = network.register_endpoint("192.0.2.1".parse().unwrap()).unwrap();
        let server = network.register_endpoint("192.0.2.2".parse().unwrap()).unwrap();
        let socket = server.bind_udp(41000).unwrap();
        let video_input = EncodedVideoInput::new().unwrap();
        let factory = world.controlled.peer_factory_builder().unwrap()
            .network_manager(client.network_manager().unwrap())
            .packet_socket_factory(client.packet_socket_factory().unwrap())
            .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
            .audio_decoder_factory(AudioDecoderFactory::builtin_opus().unwrap())
            .video_encoder_factory(video_input.encoder_factory())
            .video_decoder_factory(video_input.encoded_receive_factory().unwrap())
            .controlled_media().build().unwrap();
        let peer = factory.create_peer_connection(PeerConfiguration {
            always_negotiate_data_channels: datachannels,
            ..PeerConfiguration::default()
        }).unwrap();
        if matches!(media, MediaKind::Video) {
            // Sustain VBR source bursts before PulseBeam admission and pacing.
            peer.set_bitrate(Some(10_000_000), Some(10_000_000), Some(10_000_000)).unwrap();
        }
        let mut audio_source = None;
        let mut video_source = None;
        let mut transceivers = Vec::new();
        for index in 0..sender_count {
            let transceiver = match media {
                MediaKind::Audio => {
                    let source = factory.create_encoded_audio_source(1).unwrap();
                    let track = factory.create_encoded_audio_track(&format!("audio-{index}"), &source).unwrap();
                    let transceiver = peer.add_audio_transceiver(&track, RtpTransceiverDirection::SendReceive).unwrap();
                    let mono = peer.audio_sender_capabilities().unwrap().into_iter()
                        .find(|codec| codec.name().eq_ignore_ascii_case("opus")
                            && codec.parameters().iter().any(|p| p.key == "stereo" && p.value == "0"))
                        .expect("native mono Opus");
                    transceiver.set_audio_codec_preferences(&[mono]).unwrap();
                    if index == 0 { audio_source = Some(source); }
                    transceiver
                }
                MediaKind::Video => {
                    let source = video_input.create_source(&factory).unwrap();
                    let track = source.create_track(&factory, &format!("video-{index}")).unwrap();
                    let transceiver = peer.add_video_transceiver(&track, RtpTransceiverDirection::SendReceive).unwrap();
                    if index == 0 { video_source = Some(source); }
                    transceiver
                }
            };
            transceivers.push(transceiver);
        }
        let peer_channel = datachannels.then(|| peer.create_data_channel("peer-opened", DataChannelConfiguration::default()).unwrap());
        let offer = complete_operation(&world, &peer, peer.create_offer().unwrap()).unwrap();
        complete_operation(&world, &peer, peer.set_local_description(offer).unwrap());
        let mut gathered = false;
        for _ in 0..2_000 {
            world.pump();
            while let Some(event) = peer.try_next_event() {
                if matches!(event, PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)) {
                    gathered = true;
                }
            }
            if gathered { break; }
            world.controlled.advance(Duration::from_millis(1)).unwrap();
        }
        assert!(gathered, "native ICE gathering stalled");
        let offer = peer.descriptions().unwrap().pending_local.unwrap();
        let connection_addr = SocketAddr::new(socket.local_address().ip(), socket.local_address().port());
        let mut config = ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(connection_addr)],
            ..ConnectionConfig::default()
        };
        config.default_audio_policy.desired_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
        if let Some(limits) = limits { config.limits = limits; }
        let now = world.monotonic_now();
        let accepted = Connection::accept(config, SdpOffer::new(offer.sdp), TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(1_000_000 + now.duration_since(start).as_micros() as u64),
        }, ConnectionEntropy::new([11; 32])).expect("RTC accepts actual native offer");
        let senders = accepted.session.senders.iter().map(|s| s.id).collect::<Vec<_>>();
        let sender_mid = accepted.session.senders[0].mid.to_string();
        complete_operation(&world, &peer, peer.set_remote_description(SessionDescription {
            kind: SessionDescriptionType::Answer,
            sdp: accepted.answer.as_str().to_owned(),
        }).unwrap());
        let audio_sink = audio_source.as_ref().map(|_| transceivers[0].receiver().attach_encoded_audio_sink().unwrap());
        let video_sink = video_source.as_ref().map(|_| transceivers[0].receiver().attach_encoded_sink().unwrap());
        let now = world.monotonic_now();
        Self {
            connection: accepted.connection,
            sender: senders[0], senders, sender_mid,
            peer, audio_source, video_source, audio_sink, video_sink,
            peer_channel, remote_channels: Vec::new(), network_adapter: network, socket,
            _factory: factory, _client: client, _server: server,
            world, source_timestamp: 0,
            headers: VecDeque::new(),
            now, start,
            connection_idle: false, peer_connected: false, connection_connected: false,
            twcc_sent: 0, feedback_enabled: true,
            network: DeterministicNetwork::default(), native_target: None,
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

    pub fn configure_impairment(&mut self, impairment: RtpImpairment) {
        match impairment {
            RtpImpairment::Random { per_mille } => assert!(per_mille <= 1_000),
            RtpImpairment::Burst { every, length } => assert!(every > 0 && length <= every),
            RtpImpairment::Policer {
                bits_per_second,
                burst_bytes,
            } => assert!(bits_per_second > 0 && burst_bytes > 0),
        }
        self.network.impairment = Some(impairment);
        self.network.impaired_packets = 0;
        self.network.impaired_drops = 0;
        self.network.policer_at = None;
    }

    pub fn impairment_counts(&self) -> (u64, u64) {
        (self.network.impaired_packets, self.network.impaired_drops)
    }

    pub fn inject_cross_traffic(&mut self, bytes: u64) {
        let rate = self.network.bottleneck_bps.expect("configured bottleneck");
        let departure = self
            .network
            .next_departure
            .unwrap_or(self.now)
            .max(self.now);
        let nanos = (u128::from(bytes) * 8_000_000_000).div_ceil(u128::from(rate));
        self.network.next_departure = Some(
            departure + Duration::from_nanos(u64::try_from(nanos).expect("cross traffic service")),
        );
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
        self.now = self.now.max(self.world.monotonic_now());
        self.source_timestamp = self.source_timestamp.wrapping_add(960).max(
            u32::try_from(self.now.duration_since(self.start).as_micros() * 48_000 / 1_000_000)
                .expect("short source clock") + 960,
        );
        if let Some(source) = &self.audio_source {
            source.push_opus_at(&OpusInputFrame {
                data: opus_payload(payload),
                rtp_timestamp: self.source_timestamp,
                samples_per_channel: 960,
            }, self.world.controlled.now()).expect("native controlled Opus source");
        } else {
            // A caller can drain prior RTP without advancing its next capture tick.
            // Each encoded access unit still needs a distinct controlled capture instant.
            self.world.advance_to(self.world.controlled.now() + Duration::from_micros(1));
            let key_frame = payload[0] & 0x1f == 5;
            let mut data = if key_frame {
                // SPS/PPS describe 16x16 constrained-baseline input. Only the clear
                // slice prefix is codec data; the tail remains opaque test demand.
                vec![0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x0a, 0xd9, 0x1e, 0x84,
                    0, 0, 3, 0, 4, 0, 0, 3, 0, 0xf0, 0x3c, 0x48, 0x99, 0x20,
                    0, 0, 0, 1, 0x68, 0xcb, 0x80, 0xc4, 0xb2]
            } else {
                Vec::new()
            };
            data.extend_from_slice(&[0, 0, 0, 1, payload[0], 0x88, 0x84]);
            data.extend_from_slice(&payload[1..]);
            self.video_source.as_ref().unwrap().push_encoded(EncodedVideoAccessUnit {
                data, width: 16, height: 16,
                timestamp_us: self.world.controlled.now().as_micros() as i64,
                key_frame, qp: None,
                metadata: EncodedVideoMetadata {
                    codec: EncodedVideoCodec::H264 { base_layer_sync: false },
                    simulcast_index: None, spatial_index: None, temporal_index: None, end_of_picture: true,
                },
            }).expect("native controlled H264 source");
        }
        for _ in 0..2_000 {
            if let Some(PeerEvent::Inbound(packet)) = self.step() { return packet; }
        }
        panic!("RTC did not authenticate native source RTP");
    }

    pub fn receive_egress(&mut self) -> (FixtureRtpHeader, Vec<u8>) {
        self.connection_idle = false;
        for _ in 0..2_000 {
            if let Some(PeerEvent::Outbound(header, payload)) = self.step() { return (header, payload); }
        }
        panic!("native peer did not authenticate RTC RTP");
    }

    pub fn at(&self) -> TimePoint {
        TimePoint {
            monotonic: self.now,
            global: GlobalMediaTime::from_micros(1_000_000 + self.now.duration_since(self.start).as_micros() as u64),
        }
    }

    pub fn drive_for(&mut self, duration: Duration) {
        let deadline = self.now + duration;
        let mut stalled_steps = 0;
        while self.now < deadline {
            let before = self.now;
            let _ = self.step_until(Some(deadline));
            stalled_steps = if self.now == before { stalled_steps + 1 } else { 0 };
            assert!(stalled_steps < 100_000, "simulation failed to advance protocol time");
        }
    }

    pub fn twcc_sent(&self) -> usize { self.twcc_sent }
    pub fn set_feedback_enabled(&mut self, enabled: bool) { self.feedback_enabled = enabled; }
    pub fn command(&mut self, command: Command) { self.try_command(command).expect("connection command"); }
    pub fn try_command(&mut self, command: Command) -> Result<(), CommandError> {
        let result = self.connection.command(self.at(), command);
        if result.is_ok() { self.connection_idle = false; }
        result
    }
    pub fn peer_send(&mut self, binary: bool, payload: &[u8]) {
        assert_eq!(self.peer_channel.as_ref().expect("native channel").send(DataChannelMessage {
            kind: if binary { DataChannelMessageKind::Binary } else { DataChannelMessageKind::Text },
            bytes: payload.to_vec(),
        }), DataChannelSendResult::Sent);
    }
    pub fn next_data_event(&mut self) -> FixtureDataEvent {
        for _ in 0..4_000 {
            if let Some(event) = self.step().and_then(PeerEvent::into_data) { return event; }
        }
        panic!("DataChannel event stalled");
    }
    pub fn next_coexistence_event(&mut self) -> FixtureCoexistenceEvent {
        for _ in 0..8_000 {
            match self.step() {
                Some(PeerEvent::Outbound(_, _)) => return FixtureCoexistenceEvent::Rtp,
                Some(PeerEvent::PeerMessage { .. }) => return FixtureCoexistenceEvent::Data,
                _ => {}
            }
        }
        panic!("coexistence traffic stalled");
    }

    fn step(&mut self) -> Option<PeerEvent> {
        self.step_until(None)
    }

    fn step_until(&mut self, deadline: Option<Instant>) -> Option<PeerEvent> {
        self.world.pump();
        if let Some(index) = self.network.pending.iter().position(|packet| packet.due() <= self.now) {
            match self.network.pending.remove(index).unwrap() {
                PendingPacket::Connection { input, .. } => {
                    self.connection.receive(self.at(), input).expect("RTC native datagram receive");
                    self.connection_idle = false;
                }
                PendingPacket::Peer { destination, payload, rtp_bytes, emitted_at, queue_sample, .. } => {
                    if self.network.bottleneck_bps.is_some() {
                        self.network.delivered_bytes = self.network.delivered_bytes.saturating_add(rtp_bytes);
                        if rtp_bytes > 0 {
                            self.network.delivered_rtp.push((emitted_at, rtp_bytes));
                            if let Some((sojourn, target)) = queue_sample {
                                self.network.rtp_queue_samples.push((emitted_at, sojourn, target));
                            }
                        }
                    }
                    if let Some(header) = fixture_rtp_header(&payload) {
                        if self.headers.len() == 256 { self.headers.pop_front(); }
                        self.headers.push_back(header);
                    }
                    // BUNDLE retires non-primary native candidate sockets after answering.
                    let stun = payload.get(4..8) == Some(&[0x21, 0x12, 0xa4, 0x42][..]);
                    match self.socket.send_to(NetworkAddress::new(destination.ip(), destination.port()).unwrap(), payload) {
                        Ok(_) => {}
                        Err(NetworkError::DestinationUnavailable) if stun => {}
                        Err(error) => panic!("native virtual send: {error:?}"),
                    }
                }
            }
            return None;
        }
        if let Some(packet) = self.network_adapter.next_packet() {
            assert_eq!(packet.kind, OutboundKind::Udp, "UDP-only simulated ICE profile");
            match self.network_adapter.deliver(packet.id) {
                Ok(()) => {}
                Err(NetworkError::DestinationUnavailable)
                    if packet.source == self.socket.local_address()
                        && packet.payload.get(4..8) == Some(&[0x21, 0x12, 0xa4, 0x42][..]) => {}
                Err(error) => panic!("native virtual delivery: {error:?}"),
            }
            return None;
        }
        if let Some(packet) = self.socket.try_receive() {
            let bytes = &packet.payload;
            let rtcp = bytes.first().is_some_and(|byte| byte >> 6 == 2)
                && bytes.get(1).is_some_and(|byte| (192..=223).contains(byte));
            if rtcp && !self.feedback_enabled { return None; }
            if rtcp && bytes[0] & 31 == 15 && bytes[1] == 205 { self.twcc_sent += 1; }
            self.network.enqueue(PendingPacket::Connection {
                due: self.now + self.network.policy.delay,
                input: NetworkInput::Udp {
                    local: SocketAddr::new(self.socket.local_address().ip(), self.socket.local_address().port()),
                    remote: SocketAddr::new(packet.source.ip(), packet.source.port()),
                    ecn: None, payload: Bytes::from(packet.payload),
                },
            });
            return None;
        }
        for channel in self.peer_channel.iter().chain(self.remote_channels.iter()) {
            if let Some(event) = channel.try_next_event() {
                return match event {
                    pulsebeam_rtc_simulation::DataChannelEvent::StateChanged(DataChannelState::Open) => Some(PeerEvent::PeerOpened),
                    pulsebeam_rtc_simulation::DataChannelEvent::StateChanged(DataChannelState::Closed) => Some(PeerEvent::PeerClosed),
                    pulsebeam_rtc_simulation::DataChannelEvent::Message(message) => Some(PeerEvent::PeerMessage {
                        binary: message.kind == DataChannelMessageKind::Binary, payload: message.bytes,
                    }),
                    _ => None,
                };
            }
        }
        if let Some(sink) = &self.audio_sink {
            if let Some(frame) = sink.try_next_frame() {
                let index = self.headers.iter().position(|h| h.ssrc == frame.ssrc && Some(h.sequence_number) == frame.sequence_number)
                    .expect("authenticated native frame matches delivered RTP header");
                let header = self.headers.remove(index).unwrap();
                assert_eq!(header.timestamp, frame.rtp_timestamp);
                return Some(PeerEvent::Outbound(header, unpack_opus_payload(&frame.data)));
            }
        }
        if let Some(event) = self.peer.try_next_event() {
            return match event {
                PeerConnectionEvent::ConnectionStateChanged(pulsebeam_rtc_simulation::ConnectionState::Connected) => {
                    self.peer_connected = true;
                    Some(PeerEvent::Connected)
                }
                PeerConnectionEvent::DataChannel(channel) => {
                    self.remote_channels.push(channel);
                    None
                }
                _ => None,
            };
        }
        if !self.connection_idle {
            match self.connection.poll(self.at()) {
                ConnectionOutput::Transmit(transmit) => {
                    let (destination, payload) = match transmit.target {
                        TransmitTarget::Udp { remote, .. } => (remote, transmit.payload.as_ref()),
                        TransmitTarget::IceTcp { .. } => panic!("unexpected ICE-TCP output in UDP simulation"),
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
                    }
                    if padding != 0 {
                        self.network.emitted_rtp.push((self.now, padding, true));
                    }
                    let rtp_bytes = rtp_bytes.saturating_add(padding);
                    let queue_sample = if rtp_bytes != 0
                        && let (Some(reader), Some(rate)) =
                            (self.native_target, self.network.bottleneck_bps)
                    {
                        let service = Duration::from_nanos(
                            u64::try_from(
                                (u128::from(rtp_bytes) * 8_000_000_000).div_ceil(u128::from(rate)),
                            )
                            .unwrap_or(u64::MAX),
                        );
                        let departure = self
                            .network
                            .shared_departure
                            .as_ref()
                            .map_or(self.network.next_departure, |shared| *shared.borrow())
                            .unwrap_or(self.now);
                        Some((
                            departure.saturating_duration_since(self.now).max(service),
                            reader(&self.connection),
                        ))
                    } else {
                        None
                    };
                    self.network.enqueue(PendingPacket::Peer {
                        due,
                        destination,
                        payload: payload.to_vec(),
                        rtp_bytes,
                        emitted_at: self.now,
                        queue_sample,
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
        self.now += self.network.pending.iter().min_by_key(|packet| packet.due())
            .map_or(Duration::from_millis(10), |packet| packet.due().saturating_duration_since(self.now).max(Duration::from_millis(1)))
            .min(self.network.time_quantum.unwrap_or(Duration::MAX))
            .min(deadline.map_or(Duration::MAX, |at| at.duration_since(self.now)));
        self.world.advance_clock_to(self.now);
        self.connection_idle = false;
        None
    }
}

pub fn second_negotiated_sender() -> SenderId {
    PeerFixture::new_with(FixtureTransport::Udp, None, false, 2, MediaKind::Audio).senders[1]
}

#[derive(Clone, Copy)]
enum MediaKind { Audio, Video }

#[derive(Clone, Debug)]
pub struct FixtureRtpHeader {
    pub ssrc: u32,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ext_vals: FixtureExtensions,
}
#[derive(Clone, Debug, Default)]
pub struct FixtureExtensions { pub mid: Option<String>, pub transport_cc: Option<u16> }

fn fixture_rtp_header(bytes: &[u8]) -> Option<FixtureRtpHeader> {
    if bytes.len() < 12 || bytes[0] >> 6 != 2 || (192..=223).contains(&bytes[1]) { return None; }
    let mut ext_vals = FixtureExtensions::default();
    let offset = 12 + usize::from(bytes[0] & 15) * 4;
    if bytes[0] & 16 != 0 && bytes.get(offset..offset + 2) == Some(&[0xbe, 0xde]) {
        let words = u16::from_be_bytes(bytes.get(offset + 2..offset + 4)?.try_into().ok()?);
        let extensions = bytes.get(offset + 4..offset + 4 + usize::from(words) * 4)?;
        let mut index = 0;
        while index < extensions.len() {
            let tag = extensions[index]; index += 1;
            if tag == 0 { continue; }
            let len = usize::from(tag & 15) + 1;
            let value = extensions.get(index..index + len)?;
            match tag >> 4 {
                4 => ext_vals.mid = Some(String::from_utf8(value.to_vec()).ok()?),
                3 if len == 2 => ext_vals.transport_cc = Some(u16::from_be_bytes(value.try_into().ok()?)),
                _ => {}
            }
            index += len;
        }
    }
    Some(FixtureRtpHeader {
        ssrc: u32::from_be_bytes(bytes[8..12].try_into().ok()?),
        sequence_number: u16::from_be_bytes(bytes[2..4].try_into().ok()?),
        timestamp: u32::from_be_bytes(bytes[4..8].try_into().ok()?), ext_vals,
    })
}

// RFC 6716 padding carries test labels behind a valid mono 20 ms silence frame.
fn opus_payload(payload: &[u8]) -> Vec<u8> {
    if payload == [0xf8] { return payload.to_vec(); }
    let mut packet = vec![0xfb, 0x41];
    let mut remaining = payload.len();
    while remaining >= 254 { packet.push(255); remaining -= 254; }
    packet.push(remaining as u8);
    packet.extend_from_slice(&[0xff, 0xfe]);
    packet.extend_from_slice(payload);
    assert!(packet.len() <= 1_200);
    packet
}
fn unpack_opus_payload(packet: &[u8]) -> Vec<u8> {
    if packet.first() != Some(&0xfb) { return packet.to_vec(); }
    let mut padding = 0;
    for byte in &packet[2..] {
        padding += if *byte == 255 { 254 } else { usize::from(*byte) };
        if *byte != 255 { break; }
    }
    packet[packet.len() - padding..].to_vec()
}

#[allow(
    clippy::print_stderr,
    reason = "report actual production probe overhead separately from the allowance"
)]
pub fn assert_probe_windows(emitted: &[(Instant, u64, bool)]) {
    assert!(emitted.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    let mut checkpoints = Vec::with_capacity(emitted.len() * 2);
    for (at, _, _) in emitted {
        checkpoints.push(*at);
        checkpoints.push(*at + Duration::from_secs(5));
    }
    checkpoints.sort_unstable();
    checkpoints.dedup();
    let (mut next, mut expired) = (0, 0);
    let (mut nonprobe, mut probes) = (0_u64, 0_u64);
    let (mut peak_ratio, mut peak_allowance) = (0_u64, 0_u64);
    let mut check = |at: Instant, nonprobe: u64, probes: u64| {
        let total = probes + nonprobe;
        peak_ratio = peak_ratio.max(if total == 0 { 0 } else { probes * 100 / total });
        peak_allowance = peak_allowance.max(probes.saturating_sub(nonprobe / 19));
        assert!(
            probes <= nonprobe / 19 + 3_000,
            "emitted probe budget at {at:?}: probes={probes} nonprobe={nonprobe}"
        );
    };
    for at in checkpoints {
        while expired < next && at.duration_since(emitted[expired].0) >= Duration::from_secs(5) {
            let (_, bytes, probe) = emitted[expired];
            if probe {
                probes -= bytes;
            } else {
                nonprobe -= bytes;
            }
            expired += 1;
        }
        check(at, nonprobe, probes);
        while next < emitted.len() && emitted[next].0 == at {
            let (_, bytes, probe) = emitted[next];
            if probe {
                probes += bytes;
            } else {
                nonprobe += bytes;
            }
            next += 1;
            // Later media at this same clock instant cannot fund an earlier probe.
            check(at, nonprobe, probes);
        }
    }
    let total: u64 = emitted.iter().map(|(_, bytes, _)| *bytes).sum();
    let padding: u64 = emitted
        .iter()
        .filter(|(_, _, probe)| *probe)
        .map(|(_, bytes, _)| *bytes)
        .sum();
    eprintln!(
        "production probe windows events={} total_bytes={total} probe_bytes={padding} peak_raw_ratio={peak_ratio}% peak_allowance_used={peak_allowance}",
        emitted.len()
    );
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
    Outbound(FixtureRtpHeader, Vec<u8>),
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
}

