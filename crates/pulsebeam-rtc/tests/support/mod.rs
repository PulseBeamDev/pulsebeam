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
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use bytes::Bytes;
use pulsebeam_rtc::{
    ConnectionConfig, ConnectionEntropy, ConnectionLimits, DataChannelConfig, DataChannelEvent,
    DataChannelId, DataChannelPriority, DataMessage, DataReliability, Event as ConnectionEvent,
    LocalCandidate, MediaPayloadBitrate, NetworkInput, Output as ConnectionOutput, RejectedRtp,
    RtcConnection, RtpPacket, RtpWrite, SdpOffer, TransmitTarget, WriteError,
};
use pulsebeam_rtc_simulation::*;

pub struct MediaFixture {
    pub sender_mid: String,
}

#[derive(Clone, Copy)]
pub enum LoopbackMedia {
    Opus,
    OpusPublisher,
    H264,
    FilteredH264,
    H264Publisher,
    Av1,
    Av1Publisher,
}

#[derive(Clone, Copy)]
struct LoopbackEndpoint {
    server: SocketAddr,
    publish: bool,
}

type PacingProfileReader = fn(&RtcConnection) -> (u64, u64);

pub struct AuthenticatedVideoSample {
    pub sequence: u16,
    pub timestamp: u32,
    pub is_repair: bool,
    pub marker: bool,
    pub codec_bytes: usize,
    pub protected_udp_bytes: usize,
    pub descriptor: Option<Vec<u8>>,
    pub payload_type: u8,
    pub primary_payload_type: u8,
    pub repair_payload_type: Option<u8>,
    pub extension_profile: Option<u16>,
    pub allow_mixed_extensions: bool,
    pub extension_lengths: Vec<(u8, usize)>,
}

#[derive(Default)]
struct AuthenticatedVideoCapture {
    pending_wire_sizes: BTreeMap<(u32, u16), usize>,
    samples: Vec<AuthenticatedVideoSample>,
}

pub struct PeerFixture<M = MediaFixture> {
    media: M,
    pub connection: RtcConnection,
    pub senders: Vec<String>,
    peer: PeerConnection,
    audio_source: Option<EncodedAudioSource>,
    video_source: Option<EncodedVideoSource>,
    audio_sink: Option<EncodedAudioSink>,
    video_sink: Option<EncodedVideoSink>,
    peer_channel: Option<DataChannel>,
    remote_channels: Vec<DataChannel>,
    network_adapter: ControlledSimulatedNetwork,
    tcp_streams: Vec<FixtureTcpStream>,
    socket: SimulatedUdpSocket,
    _factory: PeerConnectionFactory,
    _client: NetworkEndpoint,
    _server: NetworkEndpoint,
    now: Instant,
    start: Instant,
    source_timestamp: u32,
    headers: VecDeque<FixtureRtpHeader>,
    udp_payload_peaks: [usize; 2],
    udp_srtcp_peaks: [usize; 2],
    bounded_udp: Option<BoundedUdpPath>,
    transmit_observations: Option<VecDeque<(Instant, pulsebeam_rtc::TransmitKind)>>,
    refuse_handoff: Option<u16>,
    lose_after_handoff: Option<u16>,
    idle_observations: Option<VecDeque<(Instant, Option<Instant>)>>,
    extension_ids: FixtureExtensionIds,
    last_video_wire_header: Option<(u32, u16)>,
    observe_egress: bool,
    connection_idle: bool,
    peer_connected: bool,
    connection_connected: bool,
    twcc_sent: usize,
    feedback_enabled: bool,
    drop_source_after: Option<usize>,
    lost_source: Option<(u32, u16, u8)>,
    pub source_repair_packets: usize,
    authenticated_video: Option<AuthenticatedVideoCapture>,
    pub receiver_report: Option<pulsebeam_rtc::ReceiverReport>,
    network: DeterministicNetwork,
    native_target: Option<fn(&RtcConnection) -> Duration>,
    pacing_profile: Option<PacingProfileReader>,
    world: std::rc::Rc<SimulationWorld>,
}

pub fn scenario_clock_binding_survives_later_peer_initialization() {
    let world = SimulationWorld::acquire();
    let start = Instant::now();
    world.bind_clock(start);
    let elapsed = Duration::from_millis(37);
    world.advance_clock_to(start + elapsed);
    world.bind_clock(start + Duration::from_secs(99));
    assert_eq!(world.monotonic_now(), start + elapsed);
}

pub fn application_only_fixture_has_no_media() {
    let fixture = PeerFixture::connected_application_only();
    assert!(fixture.senders.is_empty());
    assert_eq!(fixture.connection.stats().connection.feedback, None);
    assert_eq!(
        fixture.connection.stats().connection.transmitted_rtp_bytes,
        0
    );
}

pub fn fixture_advance_stops_at_requested_deadline() {
    let mut fixture = PeerFixture::connected();
    fixture.configure_time_quantum(Duration::from_millis(5));
    let before = fixture.at();
    fixture.drive_for(Duration::from_millis(1));
    assert_eq!(fixture.at(), before + Duration::from_millis(1));
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

struct FixtureTcpStream {
    local: NetworkAddress,
    remote: NetworkAddress,
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
    pacing_trace: Vec<(Instant, usize, u64, u64)>,
    delivered_rtp: Vec<(Instant, u64)>,
    time_quantum: Option<Duration>,
    rtc_wakeup_quantum: Option<Duration>,
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

impl<M> std::ops::Deref for PeerFixture<M> {
    type Target = M;

    fn deref(&self) -> &M {
        &self.media
    }
}

impl PeerFixture<()> {
    pub fn connected_application_only() -> Self {
        let mut fixture = Self::build(
            FixtureTransport::Udp,
            None,
            Some(DataChannelConfiguration {
                negotiated: true,
                id: Some(0),
                ..DataChannelConfiguration::default()
            }),
            0,
            MediaKind::Audio,
            PeerFixture::default_start(),
            |senders| assert!(senders.is_empty()),
        );
        for _ in 0..2_000 {
            let _ = fixture.step();
            if fixture.peer_connected && fixture.connection_connected {
                return fixture;
            }
        }
        panic!("application-only native peer did not connect");
    }
}

impl PeerFixture {
    #[allow(
        clippy::result_large_err,
        reason = "keep caller-owned input inline instead of allocating an error wrapper"
    )]
    pub fn write_rtp(&mut self, mut packet: RtpWrite) -> Result<(), RejectedRtp> {
        let deadline = self.at() + Duration::from_millis(150);
        let mid = self.sender_mid.clone();
        loop {
            match self.connection.write_rtp(self.at(), &mid, packet) {
                Ok(()) => {
                    self.connection_idle = false;
                    return Ok(());
                }
                Err(rejected)
                    if rejected.reason == WriteError::WouldBlock && self.at() < deadline =>
                {
                    packet = rejected.packet;
                    self.drive_for(Duration::from_millis(1));
                }
                Err(rejected) => return Err(rejected),
            }
        }
    }

    fn default_start() -> Instant {
        // Construction time must not age media forwarded between virtual peers.
        static START: OnceLock<Instant> = OnceLock::new();
        *START.get_or_init(Instant::now)
    }

    pub fn connected() -> Self {
        Self::connected_with(FixtureTransport::Udp, None, false)
    }

    pub fn connected_with_feedback(feedback: pulsebeam_rtc::PacketFeedbackKind) -> Self {
        Self::connect_media_with_feedback(feedback, MediaKind::Audio)
    }

    pub fn connected_video_with_rfc8888() -> Self {
        Self::connect_media_with_feedback(
            pulsebeam_rtc::PacketFeedbackKind::Rfc8888,
            MediaKind::Video,
        )
    }

    fn connect_media_with_feedback(
        feedback: pulsebeam_rtc::PacketFeedbackKind,
        media: MediaKind,
    ) -> Self {
        let trial = match feedback {
            pulsebeam_rtc::PacketFeedbackKind::TransportWide => "Disabled",
            pulsebeam_rtc::PacketFeedbackKind::Rfc8888 => "Enabled,offer:true",
        };
        let trials =
            FieldTrials::parse(&format!("WebRTC-RFC8888CongestionControlFeedback/{trial}/"))
                .expect("pinned native feedback trial");
        let mut fixture = Self::build_with_trials(
            FixtureTransport::Udp,
            None,
            None,
            1,
            media,
            Self::default_start(),
            Some(trials),
            None,
            None,
            default_accept,
            |senders| MediaFixture {
                sender_mid: senders[0].mid.to_string(),
            },
        );
        assert_eq!(
            fixture.connection.stats().connection.feedback,
            Some(feedback)
        );
        for _ in 0..2_000 {
            let _ = fixture.step();
            if fixture.peer_connected && fixture.connection_connected {
                // Connected can precede the final DTLS ACK. Commit and deliver
                // ready setup work before starting post-setup observations.
                let connected_at = fixture.at();
                fixture.record_idle_wakeups();
                for _ in 0..2_000 {
                    let _ = fixture.step_until(Some(connected_at));
                    assert_eq!(fixture.at(), connected_at, "setup must not advance time");
                    if fixture
                        .take_idle_observations()
                        .iter()
                        .any(|(_, next)| next.is_none_or(|deadline| deadline > connected_at))
                    {
                        fixture.idle_observations = None;
                        return fixture;
                    }
                }
                panic!("native feedback setup did not become idle");
            }
        }
        panic!("native feedback peer did not connect");
    }

    /// Keep bounded native interception and RTCP/TWCC, but omit Rust frame observation.
    pub fn without_egress_observation(mut self) -> Self {
        self.observe_egress = false;
        self.headers.clear();
        self
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

    pub fn connected_bounded_udp_datachannels(max_payload: usize) -> Self {
        Self::new_with(FixtureTransport::Udp, None, true, 1, MediaKind::Audio)
            .with_bounded_udp(max_payload)
            .await_connected()
    }

    pub fn connected_with_media_crypto(crypto: CryptoOptions) -> Self {
        Self::build_with_trials(
            FixtureTransport::Udp,
            None,
            None,
            1,
            MediaKind::Audio,
            Self::default_start(),
            None,
            Some(crypto),
            None,
            default_accept,
            |senders| MediaFixture {
                sender_mid: senders[0].mid.to_string(),
            },
        )
        .await_connected()
    }

    pub fn bounded_data_with_loopback(local: SocketAddr) -> Self {
        Self::build_with_trials(
            FixtureTransport::Udp,
            None,
            Some(DataChannelConfiguration::default()),
            0,
            MediaKind::Audio,
            Self::default_start(),
            None,
            None,
            Some(LoopbackEndpoint {
                server: local,
                publish: false,
            }),
            default_accept,
            |_| MediaFixture {
                sender_mid: String::new(),
            },
        )
        .with_bounded_udp(1_216)
        .await_connected()
    }

    pub fn into_native_data_endpoint(
        mut self,
    ) -> (RtcConnection, NativeMediaEndpoint, DataChannel) {
        let channel = self
            .peer_channel
            .take()
            .expect("opened Native data channel");
        let (connection, endpoint) = self.into_native_media_endpoint();
        (connection, endpoint, channel)
    }

    pub fn connected_tcp_datachannels() -> Self {
        Self::connected_with(FixtureTransport::Tcp, None, true)
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

    pub fn connected_bounded_video_dd(max_payload: usize) -> Self {
        Self::new_with(FixtureTransport::Udp, None, false, 1, MediaKind::VideoDd)
            .with_bounded_udp(max_payload)
            .await_connected()
    }

    pub fn bounded_media_with_loopback(
        local: SocketAddr,
        media: LoopbackMedia,
        accept: impl FnOnce(ConnectionConfig, SdpOffer, Instant) -> pulsebeam_rtc::AcceptedConnection,
    ) -> Self {
        let publish = matches!(
            media,
            LoopbackMedia::OpusPublisher
                | LoopbackMedia::H264Publisher
                | LoopbackMedia::Av1Publisher
        );
        let media = match media {
            LoopbackMedia::Opus | LoopbackMedia::OpusPublisher => MediaKind::Audio,
            LoopbackMedia::H264 | LoopbackMedia::H264Publisher => MediaKind::Video,
            LoopbackMedia::FilteredH264 => MediaKind::VideoDd,
            LoopbackMedia::Av1 | LoopbackMedia::Av1Publisher => MediaKind::Av1,
        };
        let trials = (!matches!(media, MediaKind::VideoDd)).then(|| {
            FieldTrials::parse("WebRTC-RFC8888CongestionControlFeedback/Enabled,offer:true/")
                .expect("pinned native feedback trial")
        });
        Self::build_with_trials(
            FixtureTransport::Udp,
            None,
            None,
            1,
            media,
            Instant::now(),
            trials,
            None,
            Some(LoopbackEndpoint {
                server: local,
                publish,
            }),
            accept,
            |senders| MediaFixture {
                sender_mid: senders
                    .first()
                    .map_or_else(String::new, |sender| sender.mid.to_string()),
            },
        )
        .with_bounded_udp(1_216)
        .await_connected()
    }

    pub fn into_native_media_endpoint(self) -> (RtcConnection, NativeMediaEndpoint) {
        let local = self.socket.local_address();
        let local = SocketAddr::new(local.ip(), local.port());
        let [a, b] = self
            .bounded_udp
            .expect("bounded UDP path")
            .pair
            .expect("selected pair");
        let remote = if a == local {
            b
        } else {
            assert_eq!(b, local);
            a
        };
        (
            self.connection,
            NativeMediaEndpoint {
                peer: self.peer,
                audio_source: self.audio_source,
                video_source: self.video_source,
                video_sink: self.video_sink,
                audio_sink: self.audio_sink,
                network: self.network_adapter,
                socket: self.socket,
                _factory: self._factory,
                _client: self._client,
                _server: self._server,
                world: self.world,
                remote,
                dd_id: self.extension_ids.dependency_descriptor,
            },
        )
    }

    fn with_bounded_udp(mut self, max_payload: usize) -> Self {
        self.bounded_udp = Some(BoundedUdpPath {
            max_payload,
            pair: None,
        });
        self
    }

    pub fn connected_video() -> Self {
        Self::connect_media(FixtureTransport::Udp, None, false, 1, MediaKind::Video)
    }

    pub fn connected_video_publisher_audio_subscriber() -> Self {
        Self::connect_media(
            FixtureTransport::Udp,
            None,
            false,
            2,
            MediaKind::VideoPublisherAudioSubscriber,
        )
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
        Self::new_with_start(transport, limits, datachannels, sender_count, media, start)
            .await_connected()
    }

    fn await_connected(mut self) -> Self {
        for _ in 0..2_000 {
            let _ = self.step();
            if self.peer_connected && self.connection_connected {
                return self;
            }
        }
        panic!(
            "standards peer did not connect: native={}, rtc={}, streams={}, stats={:?}, descriptions={:?}",
            self.peer_connected,
            self.connection_connected,
            self.tcp_streams.len(),
            self.connection.stats(),
            self.peer.descriptions(),
        );
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
        assert!((1..=128).contains(&sender_count));
        Self::build(
            transport,
            limits,
            datachannels.then(DataChannelConfiguration::default),
            sender_count,
            media,
            start,
            |senders| {
                let sender = senders.first().expect("negotiated media sender");
                MediaFixture {
                    sender_mid: sender.mid.to_string(),
                }
            },
        )
    }
}

impl<M> PeerFixture<M> {
    fn build(
        transport: FixtureTransport,
        limits: Option<ConnectionLimits>,
        datachannels: Option<DataChannelConfiguration>,
        sender_count: usize,
        media: MediaKind,
        start: Instant,
        media_state: impl FnOnce(&[pulsebeam_rtc::SenderInfo]) -> M,
    ) -> Self {
        Self::build_with_trials(
            transport,
            limits,
            datachannels,
            sender_count,
            media,
            start,
            None,
            None,
            None,
            default_accept,
            media_state,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the controlled peer constructor keeps per-peer trial and crypto choices explicit alongside fixture inputs"
    )]
    fn build_with_trials(
        transport: FixtureTransport,
        limits: Option<ConnectionLimits>,
        datachannels: Option<DataChannelConfiguration>,
        sender_count: usize,
        media: MediaKind,
        start: Instant,
        trials: Option<FieldTrials>,
        crypto: Option<CryptoOptions>,
        loopback: Option<LoopbackEndpoint>,
        accept: impl FnOnce(ConnectionConfig, SdpOffer, Instant) -> pulsebeam_rtc::AcceptedConnection,
        media_state: impl FnOnce(&[pulsebeam_rtc::SenderInfo]) -> M,
    ) -> Self {
        assert!(sender_count <= 128);
        assert!(sender_count != 0 || datachannels.is_some());
        let world = SimulationWorld::acquire();
        world.bind_clock(start);
        let network = world
            .controlled
            .create_network()
            .expect("controlled network");
        let (client_ip, server_addr) = if let Some(endpoint) = loopback {
            let host = if endpoint.publish { 3 } else { 2 };
            ([127, 0, 0, host].into(), endpoint.server)
        } else {
            (
                "192.0.2.1".parse().expect("fixture endpoint IP"),
                "192.0.2.2:41000".parse().expect("fixture server address"),
            )
        };
        let client = network
            .register_endpoint(client_ip)
            .expect("fixture endpoint IP");
        let server = network
            .register_endpoint(server_addr.ip())
            .expect("fixture endpoint IP");
        let socket = server
            .bind_udp(server_addr.port())
            .expect("controlled server UDP socket");
        let video_dd = matches!(media, MediaKind::VideoDd);
        let video_input = if matches!(media, MediaKind::Av1) {
            EncodedVideoInput::new_for_format(VideoCodecFormat::new("AV1"))
        } else if video_dd {
            EncodedVideoInput::new_l1t3([64, 128, 255])
        } else {
            EncodedVideoInput::new()
        }
        .expect("native encoded video input");
        let factory = world
            .controlled
            .peer_factory_builder()
            .expect("controlled native factory builder")
            .network_manager(
                client
                    .network_manager()
                    .expect("controlled network manager"),
            )
            .packet_socket_factory(
                client
                    .packet_socket_factory()
                    .expect("controlled packet socket factory"),
            )
            .audio_encoder_factory(
                AudioEncoderFactory::with_opus_frames().expect("native Opus encoder factory"),
            )
            .audio_decoder_factory(
                AudioDecoderFactory::builtin_opus().expect("native Opus decoder factory"),
            )
            .video_encoder_factory(video_input.encoder_factory())
            .video_decoder_factory(
                video_input
                    .encoded_receive_factory()
                    .expect("native encoded video receiver factory"),
            )
            .build()
            .expect("controlled native factory");
        let rfc8888 = trials
            .as_ref()
            .is_some_and(|trials| trials.is_enabled("WebRTC-RFC8888CongestionControlFeedback"));
        let mut dependencies = PeerConnectionDependencies::default();
        if let Some(trials) = trials {
            dependencies = dependencies.field_trials(trials);
        }
        let mut config: RtcConfiguration = PeerConfiguration {
            always_negotiate_data_channels: datachannels.is_some(),
            ..PeerConfiguration::default()
        }
        .into();
        if let Some(crypto) = crypto {
            config.options.crypto_options = crypto;
        } else if video_dd || (rfc8888 && (loopback.is_some() || matches!(media, MediaKind::Video)))
        {
            config.options.crypto_options = CryptoOptions::prefer_gcm();
        }
        let peer = factory
            .create_peer_connection_with_dependencies(config, dependencies)
            .expect("controlled native peer");
        if !matches!(media, MediaKind::Audio) {
            // Sustain VBR source bursts before PulseBeam admission and pacing.
            peer.set_bitrate(Some(10_000_000), Some(10_000_000), Some(10_000_000))
                .expect("native video source bitrate");
        }
        let mut audio_source = None;
        let mut video_source = None;
        let mut transceivers = Vec::new();
        for index in 0..sender_count {
            let mixed = matches!(media, MediaKind::VideoPublisherAudioSubscriber);
            let transceiver = match (media, index) {
                (MediaKind::Audio, _) | (MediaKind::VideoPublisherAudioSubscriber, 1..) => {
                    let source = factory
                        .create_encoded_audio_source(1)
                        .expect("native encoded audio source");
                    let track = factory
                        .create_encoded_audio_track(&format!("audio-{index}"), &source)
                        .expect("native encoded audio track");
                    let transceiver = peer
                        .add_audio_transceiver(
                            &track,
                            match loopback {
                                Some(endpoint) if endpoint.publish => {
                                    RtpTransceiverDirection::SendOnly
                                }
                                Some(_) => RtpTransceiverDirection::ReceiveOnly,
                                None if mixed => RtpTransceiverDirection::ReceiveOnly,
                                None => RtpTransceiverDirection::SendReceive,
                            },
                        )
                        .expect("native audio transceiver");
                    let mono = peer
                        .audio_sender_capabilities()
                        .expect("native audio capabilities")
                        .into_iter()
                        .find(|codec| {
                            codec.name().eq_ignore_ascii_case("opus")
                                && codec
                                    .parameters()
                                    .iter()
                                    .any(|p| p.key == "stereo" && p.value == "0")
                        })
                        .expect("native mono Opus");
                    transceiver
                        .set_audio_codec_preferences(&[mono])
                        .expect("native mono Opus preference");
                    if index == 0 {
                        audio_source = Some(source);
                    }
                    transceiver
                }
                (MediaKind::Video | MediaKind::VideoDd | MediaKind::Av1, _)
                | (MediaKind::VideoPublisherAudioSubscriber, 0) => {
                    let source = video_input
                        .create_source(&factory)
                        .expect("native encoded video source");
                    let track = source
                        .create_track(&factory, &format!("video-{index}"))
                        .expect("native encoded video track");
                    let transceiver = peer
                        .add_video_transceiver(
                            &track,
                            match loopback {
                                Some(endpoint) if endpoint.publish => {
                                    RtpTransceiverDirection::SendOnly
                                }
                                Some(_) => RtpTransceiverDirection::ReceiveOnly,
                                None if mixed => RtpTransceiverDirection::SendOnly,
                                None => RtpTransceiverDirection::SendReceive,
                            },
                        )
                        .expect("native video transceiver");
                    if video_dd {
                        let mut extensions = transceiver
                            .header_extensions_to_negotiate()
                            .expect("native extensions");
                        let dd = extensions
                            .iter_mut()
                            .find(|extension| extension.uri() == pulsebeam_core::dd::URI)
                            .expect("native DD capability");
                        dd.direction = RtpHeaderExtensionDirection::SendReceive;
                        transceiver
                            .set_header_extensions_to_negotiate(&extensions)
                            .expect("enable native DD");
                    }
                    if index == 0 {
                        video_source = Some(source);
                    }
                    transceiver
                }
            };
            if rfc8888 {
                let mut extensions = transceiver
                    .header_extensions_to_negotiate()
                    .expect("native extensions");
                for extension in &mut extensions {
                    if extension.uri()
                        == "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
                    {
                        extension.direction = RtpHeaderExtensionDirection::Stopped;
                    }
                }
                transceiver
                    .set_header_extensions_to_negotiate(&extensions)
                    .expect("offer CCFB without TWCC");
            }
            transceivers.push(transceiver);
        }
        let peer_channel = datachannels.as_ref().map(|config| {
            peer.create_data_channel("peer-opened", config.clone())
                .expect("native DataChannel")
        });
        let offer = complete_operation(
            &world,
            &peer,
            peer.create_offer().expect("native offer operation"),
        )
        .expect("native offer operation");
        complete_operation(
            &world,
            &peer,
            peer.set_local_description(offer)
                .expect("native local description"),
        );
        let mut gathered = false;
        for _ in 0..2_000 {
            world.pump();
            while let Some(event) = peer.try_next_event() {
                if matches!(
                    event,
                    PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
                ) {
                    gathered = true;
                }
            }
            if gathered {
                break;
            }
            world
                .controlled
                .advance(Duration::from_millis(1))
                .expect("forward native simulated time");
        }
        assert!(gathered, "native ICE gathering stalled");
        let offer = peer
            .descriptions()
            .expect("native descriptions")
            .pending_local
            .expect("gathered native offer");
        let connection_addr =
            SocketAddr::new(socket.local_address().ip(), socket.local_address().port());
        let mut config = ConnectionConfig {
            local_candidates: vec![match transport {
                FixtureTransport::Udp => LocalCandidate::Udp(connection_addr),
                FixtureTransport::Tcp => LocalCandidate::TcpPassive(connection_addr),
            }],
            ..ConnectionConfig::default()
        };
        if let Some(limits) = limits {
            config.limits = limits;
        }
        let now = world.monotonic_now();
        let mut accepted = accept(config, SdpOffer::new(offer.sdp), now);
        if let Some(config) = datachannels.filter(|config| config.negotiated) {
            assert_eq!(config.id, Some(0));
            assert!(config.ordered);
            assert_eq!(config.max_retransmits, None);
            assert_eq!(config.max_retransmit_time_ms, None);
            assert_eq!(accepted.connection.data_channel_capacity(), None);
            accepted
                .connection
                .open_data_channel(
                    now,
                    DataChannelConfig {
                        id: Some(DataChannelId::from_value(0)),
                        label: Arc::from("peer-opened"),
                        protocol: Arc::from(""),
                        ordered: true,
                        reliability: DataReliability::Reliable,
                        priority: DataChannelPriority::MEDIUM,
                        negotiated: true,
                    },
                )
                .expect("reserve negotiated channel zero before transport readiness");
        }
        let senders = accepted
            .session
            .senders
            .iter()
            .map(|s| s.mid.to_string())
            .collect::<Vec<_>>();
        let media_state = media_state(&accepted.session.senders);
        if sender_count == 0 {
            assert!(accepted.session.inbound_media.is_empty());
            assert_eq!(accepted.session.feedback, None);
        }
        complete_operation(
            &world,
            &peer,
            peer.set_remote_description(SessionDescription {
                kind: SessionDescriptionType::Answer,
                sdp: accepted.answer.as_str().to_owned(),
            })
            .expect("native remote description"),
        );
        if video_dd && loopback.is_none() {
            let sender = transceivers[0].sender();
            let mut parameters = sender.parameters().expect("native sender parameters");
            parameters.encodings[0].scalability_mode = Some("L1T3".into());
            sender
                .set_parameters(parameters)
                .expect("native L1T3 configuration");
        }
        let audio_sink = (audio_source.is_some()
            && loopback.is_none_or(|endpoint| !endpoint.publish))
        .then(|| {
            transceivers[0]
                .receiver()
                .attach_encoded_audio_sink()
                .expect("native encoded audio sink")
        });
        let video_sink = video_source.as_ref().map(|_| {
            transceivers[0]
                .receiver()
                .attach_encoded_sink()
                .expect("native encoded video sink")
        });
        let negotiated_extensions = transceivers
            .first()
            .map(|transceiver| {
                transceiver
                    .negotiated_header_extensions()
                    .expect("native negotiated extensions")
            })
            .unwrap_or_default();
        let extension_id = |uri| {
            negotiated_extensions
                .iter()
                .find(|extension| {
                    extension.uri() == uri
                        && matches!(
                            extension.direction,
                            RtpHeaderExtensionDirection::SendReceive
                                | RtpHeaderExtensionDirection::ReceiveOnly
                        )
                })
                .and_then(pulsebeam_rtc_simulation::RtpHeaderExtensionCapability::preferred_id)
                .map(|id| u8::try_from(id).expect("RTP extension ID"))
        };
        let extension_ids = FixtureExtensionIds {
            mid: extension_id("urn:ietf:params:rtp-hdrext:sdes:mid"),
            transport_cc: extension_id(
                "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
            ),
            playout: extension_id("http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"),
            dependency_descriptor: extension_id(pulsebeam_core::dd::URI),
        };
        let now = world.monotonic_now();
        Self {
            connection: accepted.connection,
            media: media_state,
            senders,
            peer,
            audio_source,
            video_source,
            audio_sink,
            video_sink,
            peer_channel,
            remote_channels: Vec::new(),
            network_adapter: network,
            tcp_streams: Vec::new(),
            socket,
            _factory: factory,
            _client: client,
            _server: server,
            world,
            source_timestamp: 0,
            headers: VecDeque::new(),
            udp_payload_peaks: [0; 2],
            udp_srtcp_peaks: [0; 2],
            bounded_udp: None,
            transmit_observations: None,
            refuse_handoff: None,
            lose_after_handoff: None,
            idle_observations: None,
            extension_ids,
            last_video_wire_header: None,
            observe_egress: true,
            now,
            start,
            connection_idle: false,
            peer_connected: false,
            connection_connected: false,
            twcc_sent: 0,
            feedback_enabled: true,
            drop_source_after: None,
            lost_source: None,
            source_repair_packets: 0,
            authenticated_video: None,
            receiver_report: None,
            network: DeterministicNetwork::default(),
            native_target: None,
            pacing_profile: None,
        }
    }

    pub fn configure_network(&mut self, seed: u64, policy: NetworkPolicy) {
        assert!(self.network.pending.is_empty(), "network must be drained");
        self.network.configure(seed, policy);
    }

    pub fn observe_native_target(&mut self, reader: fn(&RtcConnection) -> Duration) {
        self.native_target = Some(reader);
    }

    pub fn observe_pacing_profile(&mut self, reader: PacingProfileReader) {
        self.pacing_profile = Some(reader);
    }

    pub fn pacing_trace(&self) -> &[(Instant, usize, u64, u64)] {
        &self.network.pacing_trace
    }

    pub fn rtp_queue_samples(&self) -> &[(Instant, Duration, Duration)] {
        &self.network.rtp_queue_samples
    }

    pub fn configure_time_quantum(&mut self, quantum: Duration) {
        assert!(!quantum.is_zero());
        self.network.time_quantum = Some(quantum);
    }

    pub fn configure_rtc_wakeup_quantum(&mut self, quantum: Duration) {
        assert!(!quantum.is_zero());
        self.network.rtc_wakeup_quantum = Some(quantum);
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

    pub fn drop_source_packet_after(&mut self, packets: usize) {
        self.drop_source_after = Some(packets);
        self.lost_source = None;
        self.source_repair_packets = 0;
    }

    pub fn lost_source_packet(&self) -> (u32, u16) {
        let (ssrc, sequence, _) = self.lost_source.expect("source packet was dropped");
        (ssrc, sequence)
    }

    pub fn publisher_stats(&self) -> RtcStatsReport {
        publisher_stats(&self.peer, &self.world)
    }

    pub fn receive_source_sequence(&mut self, sequence: u16) -> RtpPacket {
        for _ in 0..2_000 {
            if let Some(PeerEvent::Inbound(packet)) = self.step()
                && packet.header.sequence == sequence
            {
                return packet;
            }
        }
        panic!(
            "source sequence {sequence} was not recovered: rtx={}, elapsed={:?}, stats={:?}",
            self.source_repair_packets,
            self.now.duration_since(self.start),
            self.connection.stats()
        );
    }

    pub fn send_source(&mut self, payload: &[u8]) -> RtpPacket {
        self.send_video_temporal_source(payload, None)
    }

    pub fn send_video_temporal_source(
        &mut self,
        payload: &[u8],
        temporal_index: Option<u8>,
    ) -> RtpPacket {
        self.emit_source(payload, temporal_index);
        for _ in 0..2_000 {
            if let Some(PeerEvent::Inbound(packet)) = self.step() {
                return packet;
            }
        }
        panic!(
            "RTC did not authenticate native source RTP: payload_prefix={:?}, stats={:?}",
            &payload[..payload.len().min(32)],
            self.connection.stats()
        );
    }

    fn emit_source(&mut self, payload: &[u8], temporal_index: Option<u8>) {
        self.now = self.now.max(self.world.monotonic_now());
        self.world.advance_clock_to(self.now);
        self.source_timestamp = self.source_timestamp.wrapping_add(960).max(
            u32::try_from(self.now.duration_since(self.start).as_micros() * 48_000 / 1_000_000)
                .expect("short source clock")
                + 960,
        );
        if let Some(source) = &self.audio_source {
            source
                .push_opus_at(
                    &OpusInputFrame {
                        data: opus_payload(payload),
                        rtp_timestamp: self.source_timestamp,
                        samples_per_channel: 960,
                    },
                    self.world.controlled.now(),
                )
                .expect("native controlled Opus source");
        } else {
            // A caller can drain prior RTP without advancing its next capture tick.
            // Each encoded access unit still needs a distinct controlled capture instant.
            self.world
                .advance_to(self.world.controlled.now() + Duration::from_micros(1));
            let key_frame = payload[0] & 0x1f == 5;
            let mut data = if key_frame {
                // SPS/PPS describe 16x16 constrained-baseline input. Only the clear
                // slice prefix is codec data; the tail remains opaque test demand.
                vec![
                    0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x0a, 0xd9, 0x1e, 0x84, 0, 0, 3, 0, 4, 0, 0, 3,
                    0, 0xf0, 0x3c, 0x48, 0x99, 0x20, 0, 0, 0, 1, 0x68, 0xcb, 0x80, 0xc4, 0xb2,
                ]
            } else {
                Vec::new()
            };
            data.extend_from_slice(&[0, 0, 0, 1, payload[0], 0x88, 0x84]);
            data.extend_from_slice(&payload[1..]);
            self.video_source
                .as_ref()
                .expect("configured native video source")
                .push_encoded(EncodedVideoAccessUnit {
                    data,
                    width: 16,
                    height: 16,
                    timestamp_us: i64::try_from(self.world.controlled.now().as_micros())
                        .expect("bounded native capture clock"),
                    key_frame,
                    qp: None,
                    metadata: EncodedVideoMetadata {
                        codec: EncodedVideoCodec::H264 {
                            base_layer_sync: false,
                        },
                        simulcast_index: None,
                        spatial_index: None,
                        temporal_index,
                        end_of_picture: true,
                    },
                })
                .expect("native controlled H264 source");
        }
    }

    pub fn observe_source(&mut self, payload: &[u8]) -> (Instant, NetworkInput) {
        self.emit_source(payload, None);
        for _ in 0..2_000 {
            let ready = self.network.pending.iter().position(|packet| {
                let PendingPacket::Connection { due, input } = packet else {
                    return false;
                };
                let bytes = match input {
                    NetworkInput::Udp { payload, .. } => payload.as_ref(),
                    NetworkInput::IceTcp { frame, .. } => frame.get(2..).expect("complete frame"),
                };
                *due <= self.at()
                    && bytes.first().is_some_and(|byte| byte >> 6 == 2)
                    && bytes.get(1).is_some_and(|byte| !(192..=223).contains(byte))
            });
            if let Some(index) = ready {
                let PendingPacket::Connection { input, .. } =
                    self.network.pending.remove(index).expect("observed input")
                else {
                    panic!("selected fixture packet is not connection input");
                };
                return (self.at(), input);
            }
            let _ = self.step();
        }
        panic!("native publisher did not emit RTP");
    }

    pub fn configure_handoff_losses(&mut self, refused: u16, lost: u16) {
        self.refuse_handoff = Some(refused);
        self.lose_next_handoff(lost);
    }

    pub fn lose_next_handoff(&mut self, sequence: u16) {
        self.lose_after_handoff = Some(sequence);
    }

    pub fn handoff_losses_consumed(&self) -> bool {
        self.refuse_handoff.is_none() && self.lose_after_handoff.is_none()
    }

    /// Returns and clears the native-to-edge and edge-to-native UDP payload peaks.
    pub fn take_udp_payload_peaks(&mut self) -> [usize; 2] {
        std::mem::take(&mut self.udp_payload_peaks)
    }

    /// Returns and clears the native-to-edge and edge-to-native SRTCP UDP peaks.
    pub fn take_udp_srtcp_peaks(&mut self) -> [usize; 2] {
        std::mem::take(&mut self.udp_srtcp_peaks)
    }

    pub fn native_sctp_dtls_parameters(&self) -> (Option<i32>, Option<i32>) {
        let info = self
            .peer
            .sctp_transport()
            .expect("native SCTP transport")
            .dtls_transport()
            .expect("native SCTP is bound to DTLS")
            .information();
        assert_eq!(info.state(), DtlsTransportState::Connected);
        assert!(
            info.remote_certificate_data()
                .is_some_and(|chain| !chain.is_empty())
        );
        (info.tls_version(), info.ssl_cipher_suite())
    }

    pub fn installed_answer_fid(&self, mid: &str) -> [u32; 2] {
        let answer = self
            .peer
            .descriptions()
            .expect("native descriptions")
            .current_remote
            .expect("native installed the RTC answer");
        answer_fid(&answer.sdp, mid)
    }

    pub fn native_media_srtp_profile(&self, mid: &str) -> Option<i32> {
        self.native_media_dtls_parameters(mid).1
    }

    pub fn native_media_dtls_parameters(&self, mid: &str) -> (Option<i32>, Option<i32>) {
        let info = self
            .peer
            .dtls_transport_by_mid(mid.as_bytes())
            .expect("native media transport bound to the negotiated MID")
            .information();
        assert_eq!(info.state(), DtlsTransportState::Connected);
        assert!(
            info.remote_certificate_data()
                .is_some_and(|chain| !chain.is_empty())
        );
        (info.tls_version(), info.srtp_cipher_suite())
    }

    pub fn record_transmits(&mut self) {
        self.transmit_observations = Some(VecDeque::new());
    }

    pub fn record_idle_wakeups(&mut self) {
        self.idle_observations = Some(VecDeque::new());
    }

    pub fn take_idle_observations(&mut self) -> Vec<(Instant, Option<Instant>)> {
        self.idle_observations
            .as_mut()
            .expect("recording")
            .drain(..)
            .collect()
    }

    pub fn take_transmit_observations(&mut self) -> Vec<(Instant, pulsebeam_rtc::TransmitKind)> {
        self.transmit_observations
            .as_mut()
            .expect("recording")
            .drain(..)
            .collect()
    }

    fn push_stock_h264_keyframe(&mut self, layered: bool) {
        push_stock_h264_keyframe(
            &self.world,
            self.video_source
                .as_ref()
                .expect("configured native video source"),
            layered,
        );
    }

    pub fn send_stock_h264_keyframe_source(&mut self) -> Vec<RtpPacket> {
        self.collect_stock_h264_keyframe(true)
    }

    pub fn send_stock_h264_keyframe_with_sizes(&mut self) -> Vec<(RtpPacket, usize)> {
        self.authenticated_video = Some(AuthenticatedVideoCapture::default());
        let packets = self.collect_stock_h264_keyframe(false);
        let samples = self
            .authenticated_video
            .take()
            .expect("wire capture")
            .samples;
        packets
            .into_iter()
            .map(|packet| {
                let sample = samples
                    .iter()
                    .find(|sample| {
                        !sample.is_repair
                            && sample.sequence == packet.header.sequence
                            && sample.timestamp == packet.header.timestamp
                    })
                    .expect("authenticated source datagram");
                assert!(sample.descriptor.is_none());
                (packet, sample.protected_udp_bytes)
            })
            .collect()
    }

    fn collect_stock_h264_keyframe(&mut self, layered: bool) -> Vec<RtpPacket> {
        self.push_stock_h264_keyframe(layered);
        let mut packets = Vec::<RtpPacket>::new();
        for _ in 0..2_000 {
            let Some(PeerEvent::Inbound(packet)) = self.step() else {
                continue;
            };
            if let Some(first) = packets.first() {
                assert_eq!(packet.header.timestamp, first.header.timestamp);
            }
            let last = packet.header.marker;
            packets.push(packet);
            if last {
                assert!(
                    packets.len() >= 3,
                    "stock keyframe includes a middle fragment"
                );
                return packets;
            }
        }
        panic!("stock keyframe did not complete");
    }

    pub fn send_stock_h264_keyframe_with_repair(
        &mut self,
        layered: bool,
    ) -> Vec<AuthenticatedVideoSample> {
        self.authenticated_video = Some(AuthenticatedVideoCapture::default());
        self.drop_source_packet_after(1);
        self.push_stock_h264_keyframe(layered);
        for _ in 0..2_000 {
            let _ = self.step();
            let samples = self
                .authenticated_video
                .as_ref()
                .expect("capturing authenticated RTP");
            if samples.samples.iter().any(|sample| sample.is_repair)
                && samples.samples.iter().any(|sample| sample.marker)
            {
                return self.authenticated_video.take().expect("capture").samples;
            }
        }
        panic!("stock native keyframe did not finish with its repair");
    }

    pub fn send_fragmented_video_source(&mut self, temporal_index: Option<u8>) -> Vec<RtpPacket> {
        assert!(self.video_source.is_some());
        let mut payload = vec![0x55; 3_500];
        payload[0] = 0x65;
        let first = self.send_video_temporal_source(&payload, temporal_index);
        let mut packets = vec![first];
        for _ in 0..2_000 {
            if packets.last().expect("first source packet").header.marker {
                assert!(
                    packets.len() >= 3,
                    "native frame includes a middle fragment"
                );
                return packets;
            }
            if let Some(PeerEvent::Inbound(packet)) = self.step() {
                assert_eq!(packet.header.timestamp, packets[0].header.timestamp);
                packets.push(packet);
            }
        }
        panic!("native fragmented frame did not finish");
    }

    /// Observe transport submission only. Authenticated receipt is asserted separately.
    pub fn receive_video_wire_header(&mut self) -> FixtureRtpHeader {
        assert!(self.video_source.is_some());
        self.connection_idle = false;
        for _ in 0..2_000 {
            if let Some(header) = self.headers.pop_front() {
                let fresh_primary = self.last_video_wire_header.is_none_or(|(ssrc, sequence)| {
                    header.ssrc == ssrc
                        && matches!(header.sequence_number.wrapping_sub(sequence), 1..=32767)
                });
                if !header.padding && fresh_primary {
                    self.last_video_wire_header = Some((header.ssrc, header.sequence_number));
                    return header;
                }
            }
            let _ = self.step();
        }
        panic!(
            "RTC video observation stalled: {:?}",
            self.connection.stats()
        );
    }

    pub fn receive_video_wire_packet(&mut self) -> FixtureRtpHeader {
        assert!(self.video_source.is_some());
        self.connection_idle = false;
        for _ in 0..2_000 {
            if let Some(header) = self.headers.pop_front() {
                return header;
            }
            let _ = self.step();
        }
        panic!("RTC video packet observation stalled");
    }

    pub fn take_video_wire_packets(&mut self) -> Vec<FixtureRtpHeader> {
        assert!(self.video_source.is_some());
        self.headers.drain(..).collect()
    }

    pub fn receive_video_frame(&mut self) -> pulsebeam_rtc_simulation::EncodedReceivedVideoFrame {
        for _ in 0..2_000 {
            if let Some(frame) = self
                .video_sink
                .as_ref()
                .expect("native video sink")
                .try_next_frame()
            {
                return frame;
            }
            let _ = self.step();
        }
        panic!("native peer did not authenticate and reassemble RTC video");
    }

    pub fn receive_egress(&mut self) -> (FixtureRtpHeader, Vec<u8>) {
        assert!(self.observe_egress, "egress observation is disabled");
        self.connection_idle = false;
        for _ in 0..2_000 {
            if let Some(PeerEvent::Outbound(header, payload)) = self.step() {
                return (header, payload);
            }
        }
        panic!("native peer did not authenticate RTC RTP");
    }

    pub fn at(&self) -> Instant {
        self.now.max(self.world.monotonic_now())
    }

    pub fn advance_clock_without_poll(&mut self, duration: Duration) {
        self.now = self
            .now
            .checked_add(duration)
            .expect("fixture clock advance");
        self.connection_idle = false;
    }

    pub fn drive_for(&mut self, duration: Duration) {
        self.now = self.at();
        let deadline = self.now + duration;
        let mut stalled_steps = 0;
        while self.now < deadline {
            let before = self.now;
            let _ = self.step_until(Some(deadline));
            stalled_steps = if self.now == before {
                stalled_steps + 1
            } else {
                0
            };
            assert!(
                stalled_steps < 100_000,
                "simulation failed to advance protocol time"
            );
        }
    }

    pub fn set_desired_bitrate(&mut self, rate: Option<MediaPayloadBitrate>) {
        self.connection.set_desired_bitrate(rate);
        self.connection_idle = false;
    }

    pub fn twcc_sent(&self) -> usize {
        self.twcc_sent
    }
    pub fn set_feedback_enabled(&mut self, enabled: bool) {
        self.feedback_enabled = enabled;
    }

    pub fn peer_send(&mut self, binary: bool, payload: &[u8]) {
        assert_eq!(
            self.peer_channel
                .as_ref()
                .expect("native channel")
                .send(DataChannelMessage {
                    kind: if binary {
                        DataChannelMessageKind::Binary
                    } else {
                        DataChannelMessageKind::Text
                    },
                    bytes: payload.to_vec(),
                }),
            DataChannelSendResult::Sent
        );
    }
    pub fn next_data_event(&mut self) -> FixtureDataEvent {
        for _ in 0..4_000 {
            if let Some(event) = self.step().and_then(PeerEvent::into_data) {
                return event;
            }
        }
        panic!("DataChannel event stalled");
    }
    pub fn next_coexistence_event(&mut self) -> FixtureCoexistenceEvent {
        for _ in 0..8_000 {
            match self.step() {
                Some(PeerEvent::Outbound(_, payload)) => {
                    return FixtureCoexistenceEvent::Rtp(payload);
                }
                Some(PeerEvent::PeerMessage { binary, payload }) => {
                    return FixtureCoexistenceEvent::Data { binary, payload };
                }
                _ => {}
            }
        }
        panic!("coexistence traffic stalled");
    }

    fn step(&mut self) -> Option<PeerEvent> {
        self.step_until(None)
    }

    fn step_until(&mut self, deadline: Option<Instant>) -> Option<PeerEvent> {
        self.now = self.at();
        self.world.pump();
        if let Some(index) = self
            .network
            .pending
            .iter()
            .position(|packet| packet.due() <= self.now)
        {
            match self
                .network
                .pending
                .remove(index)
                .expect("controlled native fixture operation")
            {
                PendingPacket::Connection { due, input } => {
                    self.connection
                        .receive(self.at(), due, input)
                        .expect("RTC native datagram receive");
                    self.connection_idle = false;
                }
                PendingPacket::Peer {
                    destination,
                    payload,
                    rtp_bytes,
                    emitted_at,
                    queue_sample,
                    ..
                } => {
                    if self.network.bottleneck_bps.is_some() {
                        self.network.delivered_bytes =
                            self.network.delivered_bytes.saturating_add(rtp_bytes);
                    }
                    if rtp_bytes > 0 {
                        self.network.delivered_rtp.push((emitted_at, rtp_bytes));
                        if let Some((sojourn, target)) = queue_sample {
                            self.network
                                .rtp_queue_samples
                                .push((emitted_at, sojourn, target));
                        }
                    }
                    if self.observe_egress
                        && let Some(header) = fixture_rtp_header(&payload, self.extension_ids)
                    {
                        if self.headers.len() == 256 {
                            self.headers.pop_front();
                        }
                        self.headers.push_back(header);
                    }
                    // BUNDLE retires non-primary native candidate sockets after answering.
                    let stun = payload.get(4..8) == Some(&[0x21, 0x12, 0xa4, 0x42][..]);
                    match self.socket.send_to(
                        NetworkAddress::new(destination.ip(), destination.port())
                            .expect("controlled UDP delivery"),
                        payload,
                    ) {
                        Ok(_) => {}
                        Err(NetworkError::DestinationUnavailable) if stun => {}
                        Err(error) => panic!("native virtual send: {error:?}"),
                    }
                }
            }
            return None;
        }
        if let Some(packet) = self.network_adapter.next_packet() {
            if let Some(guard) = self.bounded_udp.as_mut() {
                assert!(
                    matches!(packet.kind, OutboundKind::Udp),
                    "native TCP fallback"
                );
                guard.check(
                    [
                        SocketAddr::new(packet.source.ip(), packet.source.port()),
                        SocketAddr::new(packet.destination.ip(), packet.destination.port()),
                    ],
                    &packet.payload,
                );
            }
            match packet.kind {
                OutboundKind::TcpConnect => {
                    self.tcp_streams.push(FixtureTcpStream {
                        local: packet.destination,
                        remote: packet.source,
                    });
                    self.network_adapter
                        .deliver(packet.id)
                        .expect("accept native TCP client");
                    return None;
                }
                OutboundKind::TcpData => {
                    let stream = self
                        .tcp_streams
                        .iter_mut()
                        .find(|stream| {
                            stream.local == packet.destination && stream.remote == packet.source
                        })
                        .expect("admitted native TCP stream");
                    // The controlled native socket accepts whole writes here;
                    // AsyncTCPSocket has already supplied the RFC 4571 prefix.
                    let length = u16::from_be_bytes(
                        packet.payload[..2].try_into().expect("RFC 4571 prefix"),
                    );
                    assert_eq!(
                        usize::from(length),
                        packet.payload.len() - 2,
                        "complete native RFC 4571 output"
                    );
                    self.network.enqueue(PendingPacket::Connection {
                        due: self.now,
                        input: NetworkInput::IceTcp {
                            local: SocketAddr::new(stream.local.ip(), stream.local.port()),
                            remote: SocketAddr::new(stream.remote.ip(), stream.remote.port()),
                            frame: Bytes::from(packet.payload),
                        },
                    });
                    self.network_adapter
                        .drop_packet(packet.id)
                        .expect("consume external TCP bytes");
                    return None;
                }
                OutboundKind::Udp => {}
            }
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
            self.udp_payload_peaks[0] = self.udp_payload_peaks[0].max(bytes.len());
            let rtcp = is_srtcp(bytes);
            if rtcp {
                self.udp_srtcp_peaks[0] = self.udp_srtcp_peaks[0].max(bytes.len());
            }
            if bytes.len() >= 12 && !rtcp && bytes[0] >> 6 == 2 && bytes[0] & 0x20 == 0 {
                let payload_type = bytes[1] & 127;
                let ssrc =
                    u32::from_be_bytes(bytes[8..12].try_into().expect("fixed source SSRC width"));
                let sequence = u16::from_be_bytes(
                    bytes[2..4].try_into().expect("fixed source sequence width"),
                );
                if self.drop_source_after == Some(0) {
                    self.drop_source_after = None;
                    self.lost_source = Some((ssrc, sequence, payload_type));
                    return None;
                }
                self.drop_source_after = self.drop_source_after.map(|remaining| remaining - 1);
                if let Some(capture) = self.authenticated_video.as_mut() {
                    capture
                        .pending_wire_sizes
                        .insert((ssrc, sequence), bytes.len());
                }
                if self
                    .lost_source
                    .is_some_and(|(primary, _, pt)| ssrc != primary && payload_type != pt)
                {
                    self.source_repair_packets += 1;
                }
            }
            if rtcp && !self.feedback_enabled {
                return None;
            }
            if rtcp && bytes[0] & 31 == 15 && bytes[1] == 205 {
                self.twcc_sent += 1;
            }
            self.network.enqueue(PendingPacket::Connection {
                due: self.now + self.network.policy.delay,
                input: NetworkInput::Udp {
                    local: SocketAddr::new(
                        self.socket.local_address().ip(),
                        self.socket.local_address().port(),
                    ),
                    remote: SocketAddr::new(packet.source.ip(), packet.source.port()),
                    ecn: None,
                    payload: Bytes::from(packet.payload),
                },
            });
            return None;
        }
        for channel in self.peer_channel.iter().chain(self.remote_channels.iter()) {
            if let Some(event) = channel.try_next_event() {
                return match event {
                    pulsebeam_rtc_simulation::DataChannelEvent::StateChanged(
                        DataChannelState::Open,
                    ) => Some(PeerEvent::PeerOpened),
                    pulsebeam_rtc_simulation::DataChannelEvent::StateChanged(
                        DataChannelState::Closed,
                    ) => Some(PeerEvent::PeerClosed),
                    pulsebeam_rtc_simulation::DataChannelEvent::Message(message) => {
                        Some(PeerEvent::PeerMessage {
                            binary: message.kind == DataChannelMessageKind::Binary,
                            payload: message.bytes,
                        })
                    }
                    _ => None,
                };
            }
        }
        if self.observe_egress
            && let Some(sink) = &self.audio_sink
            && let Some(frame) = sink.try_next_frame()
        {
            let index = self
                .headers
                .iter()
                .position(|h| {
                    h.ssrc == frame.ssrc && Some(h.sequence_number) == frame.sequence_number
                })
                .expect("authenticated native frame matches delivered RTP header");
            let header = self
                .headers
                .remove(index)
                .expect("delivered native RTP header");
            assert_eq!(header.timestamp, frame.rtp_timestamp);
            return Some(PeerEvent::Outbound(
                header,
                unpack_opus_payload(&frame.data),
            ));
        }
        if let Some(event) = self.peer.try_next_event() {
            return match event {
                PeerConnectionEvent::ConnectionStateChanged(
                    pulsebeam_rtc_simulation::ConnectionState::Connected,
                ) => {
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
        let mut connection_wakeup = None;
        if !self.connection_idle {
            let now = self.at();
            let output = if let Some(capture) = &mut self.authenticated_video {
                self.connection.poll_with_rtp_observer(
                    now,
                    &mut |packet: pulsebeam_rtc::AuthenticatedRtp<'_>| {
                        let header = packet.wire_header();
                        if packet.primary_codec.name.as_ref() != "h264"
                            || packet.wire_payload().is_empty()
                        {
                            return;
                        }
                        let (sequence, payload) = if packet.is_repair {
                            let [high, low, payload @ ..] = packet.wire_payload() else {
                                return;
                            };
                            if payload.is_empty() {
                                return;
                            }
                            (u16::from_be_bytes([*high, *low]), payload)
                        } else {
                            (header.sequence, packet.wire_payload())
                        };
                        let descriptor = packet.wire_extensions().find_map(|(id, value)| {
                            packet
                                .receive_extensions
                                .iter()
                                .any(|extension| {
                                    extension.id == id
                                        && extension.uri.as_ref() == pulsebeam_core::dd::URI
                                })
                                .then(|| value.to_vec())
                        });
                        let protected_udp_bytes = capture
                            .pending_wire_sizes
                            .remove(&(header.ssrc, header.sequence))
                            .expect("authenticated RTP matches a protected UDP datagram");
                        capture.samples.push(AuthenticatedVideoSample {
                            sequence,
                            timestamp: header.timestamp,
                            is_repair: packet.is_repair,
                            marker: header.marker,
                            codec_bytes: payload.len(),
                            protected_udp_bytes,
                            descriptor,
                            payload_type: header.payload_type,
                            primary_payload_type: packet.primary_payload_type(),
                            repair_payload_type: packet.repair_payload_type,
                            extension_profile: packet.extension_profile(),
                            allow_mixed_extensions: packet.allow_mixed_extensions,
                            extension_lengths: packet
                                .wire_extensions()
                                .map(|(id, value)| (id, value.len()))
                                .collect(),
                        });
                    },
                )
            } else {
                self.connection.poll(now)
            };
            if let Some(read) = self.pacing_profile {
                let (rate, epoch) = read(&self.connection);
                if self.network.pacing_trace.last().is_none_or(
                    |&(_, _, previous_rate, previous_epoch)| {
                        (previous_rate, previous_epoch) != (rate, epoch)
                    },
                ) {
                    self.network.pacing_trace.push((
                        self.now,
                        self.network.emitted_rtp.len(),
                        rate,
                        epoch,
                    ));
                }
            }
            let output = match output {
                ConnectionOutput::Handoff { id, transmit, .. } => {
                    let sequence = match transmit.kind {
                        pulsebeam_rtc::TransmitKind::Original { .. } => {
                            fixture_rtp_header(&transmit.payload, self.extension_ids)
                                .map(|header| header.sequence_number)
                        }
                        _ => None,
                    };
                    let refused = sequence.is_some() && sequence == self.refuse_handoff;
                    assert!(self.connection.resolve_handoff(id, !refused));
                    if refused {
                        self.refuse_handoff = None;
                        return None;
                    }
                    if sequence.is_some() && sequence == self.lose_after_handoff {
                        self.lose_after_handoff = None;
                        return None;
                    }
                    ConnectionOutput::Transmit(transmit)
                }
                other => other,
            };
            match output {
                ConnectionOutput::Transmit(transmit) => {
                    let at = self.at();
                    if let Some(observations) = &mut self.transmit_observations {
                        if observations.len() == 256 {
                            observations.pop_front();
                        }
                        observations.push_back((at, transmit.kind));
                    }
                    let (destination, payload) = match transmit.target {
                        TransmitTarget::Udp { local, remote, .. } => {
                            if let Some(guard) = self.bounded_udp.as_mut() {
                                guard.check([remote, local], &transmit.payload);
                            }
                            let bytes = transmit.payload.as_ref();
                            self.udp_payload_peaks[1] = self.udp_payload_peaks[1].max(bytes.len());
                            if is_srtcp(bytes) {
                                self.udp_srtcp_peaks[1] = self.udp_srtcp_peaks[1].max(bytes.len());
                            }
                            (remote, bytes)
                        }
                        TransmitTarget::IceTcp { local, remote } => {
                            assert!(self.bounded_udp.is_none(), "edge TCP fallback");
                            let stream = self
                                .tcp_streams
                                .iter()
                                .find(|stream| {
                                    SocketAddr::new(stream.local.ip(), stream.local.port()) == local
                                        && SocketAddr::new(stream.remote.ip(), stream.remote.port())
                                            == remote
                                })
                                .expect("output identifies an admitted TCP stream");
                            let length = u16::from_be_bytes(
                                transmit.payload[..2].try_into().expect("RFC 4571 prefix"),
                            );
                            let packet = &transmit.payload[2..];
                            assert_eq!(
                                usize::from(length),
                                packet.len(),
                                "complete RFC 4571 output"
                            );
                            if self.observe_egress
                                && let Some(header) = fixture_rtp_header(packet, self.extension_ids)
                            {
                                if self.headers.len() == 256 {
                                    self.headers.pop_front();
                                }
                                self.headers.push_back(header);
                            }
                            self.network_adapter
                                .inject_tcp_data(stream.local, stream.remote, &transmit.payload)
                                .expect("native TCP client receives framed RTC output");
                            return None;
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
                ConnectionOutput::Event(ConnectionEvent::Rtp { packet, .. }) => {
                    return Some(PeerEvent::Inbound(packet));
                }
                ConnectionOutput::Event(ConnectionEvent::DataChannel(event)) => {
                    return Some(PeerEvent::ConnectionData(event));
                }
                ConnectionOutput::Event(ConnectionEvent::ReceiverReport(report)) => {
                    self.receiver_report = Some(report);
                    return None;
                }
                ConnectionOutput::Event(_) => return None,
                ConnectionOutput::Idle { next_wakeup } => {
                    let at = self.at();
                    if let Some(observations) = &mut self.idle_observations {
                        if observations.len() == 256 {
                            observations.pop_front();
                        }
                        observations.push_back((at, next_wakeup));
                    }
                    connection_wakeup = next_wakeup.map(|at| {
                        self.network.rtc_wakeup_quantum.map_or(at, |quantum| {
                            let nanos = at
                                .saturating_duration_since(self.start)
                                .as_nanos()
                                .div_ceil(quantum.as_nanos())
                                .saturating_mul(quantum.as_nanos());
                            self.start
                                + Duration::from_nanos(
                                    u64::try_from(nanos).expect("bounded fixture timer deadline"),
                                )
                        })
                    });
                    self.connection_idle = true;
                }
                ConnectionOutput::Closed(reason) => panic!("PulseBeam closed: {reason:?}"),
                _ => panic!("unexpected future PulseBeam output"),
            }
        }
        self.now += self
            .network
            .pending
            .iter()
            .min_by_key(|packet| packet.due())
            .map_or(Duration::from_millis(10), |packet| {
                packet
                    .due()
                    .saturating_duration_since(self.now)
                    .max(Duration::from_millis(1))
            })
            .min(self.network.time_quantum.unwrap_or(Duration::MAX))
            .min(
                connection_wakeup
                    .map_or(Duration::MAX, |at| at.saturating_duration_since(self.now)),
            )
            .min(deadline.map_or(Duration::MAX, |at| at.duration_since(self.now)));
        self.world.advance_clock_to(self.now);
        self.connection_idle = false;
        None
    }
}

#[derive(Clone, Copy)]
enum MediaKind {
    Audio,
    Video,
    VideoDd,
    Av1,
    VideoPublisherAudioSubscriber,
}

#[derive(Clone, Debug)]
pub struct FixtureRtpHeader {
    pub wire_bytes: usize,
    pub ssrc: u32,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ext_vals: FixtureExtensions,
    pub padding: bool,
    pub marker: bool,
}
#[derive(Clone, Debug, Default)]
pub struct FixtureExtensions {
    pub mid: Option<String>,
    pub transport_cc: Option<u16>,
    pub play_delay_min: Option<Duration>,
    pub play_delay_max: Option<Duration>,
    pub dependency_descriptor: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Default)]
struct FixtureExtensionIds {
    mid: Option<u8>,
    transport_cc: Option<u8>,
    playout: Option<u8>,
    dependency_descriptor: Option<u8>,
}

fn fixture_rtp_header(bytes: &[u8], ids: FixtureExtensionIds) -> Option<FixtureRtpHeader> {
    if bytes.len() < 12 || bytes[0] >> 6 != 2 || (192..=223).contains(&bytes[1]) {
        return None;
    }
    let mut ext_vals = FixtureExtensions::default();
    let offset = 12 + usize::from(bytes[0] & 15) * 4;
    if bytes[0] & 16 != 0 {
        let profile = u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?);
        let one_byte = profile == 0xbede;
        if !one_byte && profile & 0xfff0 != 0x1000 {
            return None;
        }
        let words = u16::from_be_bytes(bytes.get(offset + 2..offset + 4)?.try_into().ok()?);
        let extensions = bytes.get(offset + 4..offset + 4 + usize::from(words) * 4)?;
        let mut index = 0;
        while index < extensions.len() {
            let tag = extensions[index];
            index += 1;
            if tag == 0 {
                continue;
            }
            let (id, len) = if one_byte {
                if tag >> 4 == 15 {
                    break;
                }
                (tag >> 4, usize::from(tag & 15) + 1)
            } else {
                let len = usize::from(*extensions.get(index)?);
                index += 1;
                (tag, len)
            };
            let value = extensions.get(index..index + len)?;
            match id {
                id if Some(id) == ids.dependency_descriptor => {
                    ext_vals.dependency_descriptor = Some(value.to_vec());
                }
                id if Some(id) == ids.playout && len == 3 => {
                    let min = (u16::from(value[0]) << 4) | (u16::from(value[1]) >> 4);
                    let max = (u16::from(value[1] & 15) << 8) | u16::from(value[2]);
                    ext_vals.play_delay_min = Some(Duration::from_millis(u64::from(min) * 10));
                    ext_vals.play_delay_max = Some(Duration::from_millis(u64::from(max) * 10));
                }
                id if Some(id) == ids.mid => {
                    ext_vals.mid = Some(String::from_utf8(value.to_vec()).ok()?);
                }
                id if Some(id) == ids.transport_cc && len == 2 => {
                    ext_vals.transport_cc = Some(u16::from_be_bytes(value.try_into().ok()?));
                }
                _ => {}
            }
            index += len;
        }
    }
    Some(FixtureRtpHeader {
        wire_bytes: bytes.len(),
        padding: bytes[0] & 0x20 != 0,
        marker: bytes[1] & 0x80 != 0,
        ssrc: u32::from_be_bytes(bytes[8..12].try_into().ok()?),
        sequence_number: u16::from_be_bytes(bytes[2..4].try_into().ok()?),
        timestamp: u32::from_be_bytes(bytes[4..8].try_into().ok()?),
        ext_vals,
    })
}

pub fn wire_observer_reads_two_byte_dependency_descriptor() {
    let bytes = [
        0x90, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0x10, 0, 0, 2, 20, 3, 0xc0, 0, 1, 0, 0, 0,
    ];
    let ids = FixtureExtensionIds {
        dependency_descriptor: Some(20),
        ..Default::default()
    };
    let header = fixture_rtp_header(&bytes, ids).expect("two-byte DD");
    assert_eq!(
        header.ext_vals.dependency_descriptor.as_deref(),
        Some([0xc0, 0, 1].as_slice())
    );
    assert!(fixture_rtp_header(&bytes[..22], ids).is_none());
}

pub fn wire_observer_uses_negotiated_extension_ids() {
    let bytes = [
        0x90, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0xbe, 0xde, 0, 3, 0x40,
        0xff, // ID 4 is not MID in this negotiation.
        0x60, b'v', 0x81, 0, 42, 0xa2, 0, 0x20, 30, 0,
    ];
    let header = fixture_rtp_header(
        &bytes,
        FixtureExtensionIds {
            mid: Some(6),
            transport_cc: Some(8),
            playout: Some(10),
            dependency_descriptor: None,
        },
    )
    .expect("negotiated wire extensions");
    assert!(!header.marker);
    let mut marked = bytes;
    marked[1] |= 0x80;
    assert!(
        fixture_rtp_header(&marked, FixtureExtensionIds::default())
            .expect("marked media packet")
            .marker
    );
    assert_eq!(header.ext_vals.mid.as_deref(), Some("v"));
    assert_eq!(header.ext_vals.transport_cc, Some(42));
    assert_eq!(
        header.ext_vals.play_delay_min,
        Some(Duration::from_millis(20))
    );
    assert_eq!(
        header.ext_vals.play_delay_max,
        Some(Duration::from_millis(300))
    );
}

// RFC 6716 padding carries test labels behind a valid mono 20 ms silence frame.
fn opus_payload(payload: &[u8]) -> Vec<u8> {
    if payload == [0xf8] {
        return payload.to_vec();
    }
    let mut packet = vec![0xfb, 0x41];
    let mut remaining = payload.len();
    while remaining >= 254 {
        packet.push(255);
        remaining -= 254;
    }
    packet.push(u8::try_from(remaining).expect("Opus padding remainder below 254"));
    packet.extend_from_slice(&[0xff, 0xfe]);
    packet.extend_from_slice(payload);
    assert!(packet.len() <= 1_200);
    packet
}
fn unpack_opus_payload(packet: &[u8]) -> Vec<u8> {
    if packet.first() != Some(&0xfb) {
        return packet.to_vec();
    }
    let mut padding = 0;
    for byte in &packet[2..] {
        padding += if *byte == 255 {
            254
        } else {
            usize::from(*byte)
        };
        if *byte != 255 {
            break;
        }
    }
    packet[packet.len() - padding..].to_vec()
}

#[allow(
    clippy::print_stderr,
    reason = "retain measured full-wire RTP and padding totals"
)]
pub fn assert_native_rtp_pacing(
    emitted: &[(Instant, u64, bool)],
    configured: &[(Instant, usize, u64, u64)],
) {
    let Some(&(first, _, _)) = emitted.first() else {
        return;
    };
    assert!(emitted.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    assert!(
        configured
            .windows(2)
            .all(|pair| (pair[0].0, pair[0].1) <= (pair[1].0, pair[1].1))
    );
    let mut at = configured.first().map_or(first, |entry| entry.0);
    let (mut next_config, mut rate, mut epoch) = (0, 0_u64, 0_u64);
    let mut debt = 0_u128;
    let mut padding_clock = false;
    for (index, &(now, bytes, padding)) in emitted.iter().enumerate() {
        let mut service_rate = rate;
        while let Some(&(changed_at, before, new_rate, new_epoch)) = configured.get(next_config) {
            if before > index {
                break;
            }
            assert!(
                changed_at >= at && changed_at <= now,
                "causal native pacing configuration"
            );
            if epoch != new_epoch {
                debt = 0;
                padding_clock = false;
                at = changed_at;
                service_rate = new_rate;
            }
            (rate, epoch) = (new_rate, new_epoch);
            service_rate = service_rate.max(rate);
            next_config += 1;
        }
        assert!(
            rate != 0,
            "native RTP pacing requires a configured data rate"
        );
        // SetPacerConfig can reprice still-unprocessed elapsed time. The largest
        // rate observed since the previous wire commit bounds that service,
        // without borrowing any future configuration or emission.
        debt = debt.saturating_sub(
            u128::from(service_rate).saturating_mul(now.duration_since(at).as_nanos()),
        );
        at = now;
        // Wire-level upper envelope, not a second implementation of eligibility:
        // native 40 ms/63 KB bursts, one-packet overshoot, and at most 50 ms of
        // padding-clock replay. Rate changes cannot fund earlier same-time sends.
        // Exact debt, 5 ms generation and full-drain gates have focused pacer proof.
        let burst = u128::from(rate)
            .saturating_mul(Duration::from_millis(40).as_nanos())
            .min(63_000 * 8_000_000_000_u128);
        let replay = if padding || padding_clock {
            u128::from(service_rate).saturating_mul(Duration::from_millis(50).as_nanos())
        } else {
            0
        };
        assert!(
            debt <= burst.saturating_add(replay),
            "native RTP pacing budget at {now:?}: debt={debt} rate={rate} padding={padding} epoch={epoch}"
        );
        debt = debt
            .saturating_add(u128::from(bytes).saturating_mul(8_000_000_000))
            .min(u128::from(rate).saturating_mul(Duration::from_millis(500).as_nanos()));
        padding_clock = padding;
    }
    let total: u64 = emitted.iter().map(|entry| entry.1).sum();
    let padding: u64 = emitted
        .iter()
        .filter(|entry| entry.2)
        .map(|entry| entry.1)
        .sum();
    eprintln!(
        "native RTP pacing events={} full_wire_bytes={total} padding_wire_bytes={padding}",
        emitted.len()
    );
}

pub fn rtp_write(packet: RtpPacket, sequence: u64) -> RtpWrite {
    RtpWrite {
        sequence, timestamp: packet.header.timestamp, wallclock: packet.arrival,
        marker: packet.header.marker, payload: packet.payload,
        extensions: packet.extensions.into_iter().filter(|value| !matches!(value.uri.as_ref(),
            "urn:ietf:params:rtp-hdrext:sdes:mid"
            | "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id"
            | "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id"
            | "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"
            | "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
            | "urn:ietf:params:rtp-hdrext:transport-wide-cc-02"
        )).collect(),
    }
}

pub fn sample(at: Instant, sequence: u64, payload_bytes: usize, clock_rate: u32) -> RtpWrite {
    RtpWrite {
        sequence,
        timestamp: u32::try_from(sequence)
            .expect("short sample trace")
            .wrapping_mul(clock_rate / 50),
        wallclock: at,
        marker: true,
        payload: Bytes::from(vec![0x55; payload_bytes]),
        extensions: Vec::new(),
    }
}

enum PeerEvent {
    Connected,
    Inbound(RtpPacket),
    Outbound(FixtureRtpHeader, Vec<u8>),
    ConnectionData(DataChannelEvent),
    PeerOpened,
    PeerMessage { binary: bool, payload: Vec<u8> },
    PeerClosed,
}

impl PeerEvent {
    fn into_data(self) -> Option<FixtureDataEvent> {
        match self {
            Self::ConnectionData(DataChannelEvent::Opened { channel, .. }) => {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FixtureCoexistenceEvent {
    Rtp(Vec<u8>),
    Data { binary: bool, payload: Vec<u8> },
}

fn default_accept(
    config: ConnectionConfig,
    offer: SdpOffer,
    now: Instant,
) -> pulsebeam_rtc::AcceptedConnection {
    RtcConnection::accept(
        config,
        offer,
        now,
        std::time::UNIX_EPOCH + Duration::from_secs(1),
        ConnectionEntropy::new([11; 32]),
    )
    .expect("RTC accepts actual native offer")
}

pub fn stock_h264_keyframe_720p(world: &SimulationWorld, layered: bool) -> NativeH264AccessUnit {
    stock_h264_fixture_for_encoding(
        world,
        NativeH264FixtureEncoding {
            width: 1280,
            height: 720,
            bitrate_kbps: 1000,
        },
        if layered { 3 } else { 1 },
        1,
    )
    .expect("stock encoding")
    .into_iter()
    .find(|unit| unit.keyframe)
    .expect("stock keyframe")
}

fn push_stock_h264_keyframe(world: &SimulationWorld, source: &EncodedVideoSource, layered: bool) {
    let unit = stock_h264_keyframe_720p(world, layered);
    world.advance_to(world.controlled.now() + Duration::from_micros(1));
    source
        .push_encoded(EncodedVideoAccessUnit {
            data: unit.data,
            width: unit.width,
            height: unit.height,
            timestamp_us: i64::try_from(world.controlled.now().as_micros())
                .expect("bounded native capture clock"),
            key_frame: true,
            qp: None,
            metadata: EncodedVideoMetadata {
                codec: EncodedVideoCodec::H264 {
                    base_layer_sync: layered && unit.base_layer_sync,
                },
                simulcast_index: None,
                spatial_index: None,
                temporal_index: if layered { unit.temporal_index } else { None },
                end_of_picture: true,
            },
        })
        .expect("native stock H264 access unit");
}

fn publisher_stats(peer: &PeerConnection, world: &SimulationWorld) -> RtcStatsReport {
    let sender = peer
        .video_senders()
        .expect("native publisher senders")
        .into_iter()
        .next()
        .expect("native video publisher");
    let request = peer
        .request_sender_stats(&sender)
        .expect("native publisher stats request");
    collect_stats(peer, world, request)
}

fn collect_stats(
    peer: &PeerConnection,
    world: &SimulationWorld,
    request: OperationId,
) -> RtcStatsReport {
    for _ in 0..2_000 {
        world.pump();
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::Stats(snapshot) = event {
                assert_eq!(snapshot.operation_id, request);
                return snapshot.report;
            }
        }
    }
    panic!("native media stats stalled");
}

pub struct NativeMediaEndpoint {
    peer: PeerConnection,
    audio_source: Option<EncodedAudioSource>,
    video_source: Option<EncodedVideoSource>,
    video_sink: Option<EncodedVideoSink>,
    audio_sink: Option<EncodedAudioSink>,
    network: ControlledSimulatedNetwork,
    socket: SimulatedUdpSocket,
    _factory: PeerConnectionFactory,
    _client: NetworkEndpoint,
    _server: NetworkEndpoint,
    world: std::rc::Rc<SimulationWorld>,
    pub remote: SocketAddr,
    pub dd_id: Option<u8>,
}

impl NativeMediaEndpoint {
    fn video_receiver(&self) -> RtpReceiver {
        let transceivers = self.peer.transceivers().expect("Native transceivers");
        let [video] = transceivers.as_slice() else {
            panic!("one Native video transceiver");
        };
        video.receiver()
    }

    pub fn request_video_refresh(&self) {
        self.video_receiver()
            .request_keyframe()
            .expect("request a Native receiver keyframe");
    }

    pub fn subscriber_refresh_counts(&self, ssrc: u32) -> (u32, u32) {
        let request = self
            .peer
            .request_receiver_stats(&self.video_receiver())
            .expect("Native subscriber stats request");
        collect_stats(&self.peer, &self.world, request)
            .of_class(RtcStatsClass::InboundRtpStreamStats)
            .into_iter()
            .find_map(|entry| match entry.values() {
                RtcStatsValues::InboundRtpStreamStats(value) if value.ssrc == Some(ssrc) => {
                    Some((value.pli_count.unwrap_or(0), value.fir_count.unwrap_or(0)))
                }
                _ => None,
            })
            .expect("refresh counters for the stock subscriber")
    }

    pub fn publisher_pli_count(&self, ssrc: u32) -> u32 {
        publisher_stats(&self.peer, &self.world)
            .of_class(RtcStatsClass::OutboundRtpStreamStats)
            .into_iter()
            .find_map(|entry| match entry.values() {
                RtcStatsValues::OutboundRtpStreamStats(value) if value.ssrc == Some(ssrc) => {
                    Some(value.pli_count.unwrap_or(0))
                }
                _ => None,
            })
            .expect("authenticated feedback counter for the stock publisher")
    }

    pub fn push_opus(&self, data: Vec<u8>, timestamp: u32) {
        self.audio_source
            .as_ref()
            .expect("Native Opus publisher")
            .push_opus_at(
                &OpusInputFrame {
                    data,
                    rtp_timestamp: timestamp,
                    samples_per_channel: 960,
                },
                self.world.controlled.now(),
            )
            .expect("stock Opus access unit");
    }

    pub fn push_stock_av1(&self) {
        let data = stock_av1_keyframe(&self.world).expect("stock libaom keyframe");
        self.world
            .advance_to(self.world.controlled.now() + Duration::from_micros(1));
        self.video_source
            .as_ref()
            .expect("configured native AV1 source")
            .push_encoded(EncodedVideoAccessUnit {
                data,
                width: 1280,
                height: 720,
                timestamp_us: i64::try_from(self.world.controlled.now().as_micros())
                    .expect("bounded native capture clock"),
                key_frame: true,
                qp: None,
                metadata: EncodedVideoMetadata {
                    codec: EncodedVideoCodec::Av1,
                    simulcast_index: None,
                    spatial_index: None,
                    temporal_index: None,
                    end_of_picture: true,
                },
            })
            .expect("native stock AV1 access unit");
    }

    pub fn push_stock_h264(&self) {
        push_stock_h264_keyframe(
            &self.world,
            self.video_source.as_ref().expect("Native H.264 publisher"),
            false,
        );
    }

    pub fn deliver(&self, payload: &[u8]) {
        self.socket
            .send_to(
                NetworkAddress::new(self.remote.ip(), self.remote.port())
                    .expect("selected peer UDP"),
                payload.to_vec(),
            )
            .expect("deliver the original protected UDP bytes to Native");
    }

    pub fn step(&self, now: Instant, mut feedback: impl FnMut(&[u8])) {
        self.world.advance_clock_to(now);
        for _ in 0..64 {
            let Some(packet) = self.network.next_packet() else {
                break;
            };
            assert!(matches!(packet.kind, OutboundKind::Udp));
            self.network
                .deliver(packet.id)
                .expect("controlled UDP feedback");
        }
        while let Some(packet) = self.socket.try_receive() {
            assert_eq!(
                SocketAddr::new(packet.source.ip(), packet.source.port()),
                self.remote
            );
            feedback(&packet.payload);
        }
    }

    pub fn try_next_video(&self) -> Option<EncodedReceivedVideoFrame> {
        self.video_sink
            .as_ref()
            .expect("Native video receiver")
            .try_next_frame()
    }

    pub fn try_next_audio(&self) -> Option<EncodedAudioFrame> {
        self.audio_sink
            .as_ref()
            .expect("Native audio receiver")
            .try_next_frame()
    }
}

fn mono_opus() -> SdpAudioFormat {
    let mut format = SdpAudioFormat::new(b"opus".to_vec(), 48_000, 2);
    format.parameters.insert(b"stereo".to_vec(), b"0".to_vec());
    format
}

pub fn maximum_data_record() -> [u8; 1_155] {
    let mut record = [0; 1_155];
    record[..22].copy_from_slice(&[
        0x50, 0x44, 1, 7, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 1, 4, 0, 0, 0, 4, 0,
    ]);
    record[22..30].copy_from_slice(&(-50_i64).to_be_bytes());
    record[34] = 96;
    record[35..131].fill(0xc0);
    record[131..].fill(0xa3);
    record
}

pub fn stock_opus_packet() -> Vec<u8> {
    let env = Environment::builder().build().expect("encoder environment");
    let factory = AudioEncoderFactory::builtin().expect("stock encoder factory");
    let encoder = factory
        .create(
            &env,
            &mono_opus(),
            &AudioEncoderOptions {
                payload_type: 111,
                codec_pair_id: None,
            },
        )
        .expect("stock Opus encoder")
        .expect("mono Opus support");
    let pcm: Vec<i16> = (0..480)
        .map(|i| if i / 24 % 2 == 0 { 8192 } else { -8192 })
        .collect();
    let first = encoder.encode(0, &pcm).expect("first 10ms");
    assert!(first.bytes.is_empty());
    let second = encoder.encode(480, &pcm).expect("second 10ms");
    assert!(!second.bytes.is_empty());
    second.bytes
}

pub fn assert_decoded_stock_opus(payload: Vec<u8>, timestamp: u32) {
    let env = Environment::builder().build().expect("decoder environment");
    let factory = AudioDecoderFactory::builtin_opus().expect("stock decoder factory");
    let decoder = factory
        .create(&env, &mono_opus())
        .expect("stock Opus decoder")
        .expect("mono Opus support");
    assert_eq!((decoder.sample_rate_hz(), decoder.channels()), (48_000, 1));
    let packet = decoder
        .parse_payload(payload, timestamp)
        .expect("received Opus packet");
    let [packet] = packet.as_slice() else {
        panic!("one decoded frame for this Opus packet");
    };
    assert_eq!(packet.timestamp, timestamp);
    let frame = packet.frame.as_ref().expect("decodable Opus frame");
    let decoded = frame
        .decode()
        .expect("Opus decode")
        .expect("decoded Opus PCM");
    assert_eq!(decoded.samples.len(), 960);
    assert!(
        decoded
            .samples
            .iter()
            .any(|sample| sample.unsigned_abs() > 512)
    );
}

pub fn answer_fid(answer: &str, mid: &str) -> [u32; 2] {
    let mid = format!("a=mid:{mid}");
    let mut fid = answer
        .lines()
        .skip_while(|line| *line != mid)
        .skip(1)
        .take_while(|line| !line.starts_with("m="))
        .filter_map(|line| line.strip_prefix("a=ssrc-group:FID "));
    let group = fid.next().expect("installed answer signals FID");
    assert!(fid.next().is_none(), "one FID pair for {mid}");
    let (primary, repair) = group.split_once(' ').expect("FID pair");
    [
        primary.parse().expect("primary SSRC"),
        repair.parse().expect("repair SSRC"),
    ]
}

pub fn assert_decoded_stock_keyframe(frame: &EncodedReceivedVideoFrame) {
    assert_decoded_keyframe(frame, "H264", 4); // Native kVideoCodecH264.
}

pub fn assert_decoded_stock_av1_keyframe(frame: &EncodedReceivedVideoFrame) {
    assert_decoded_keyframe(frame, "AV1", 3); // Native kVideoCodecAV1.
}

fn assert_decoded_keyframe(frame: &EncodedReceivedVideoFrame, name: &str, codec_type: i32) {
    struct Picture {
        size: [i32; 2],
        timestamp: u32,
        nonconstant_luma: bool,
    }

    struct DecoderOutput(std::sync::mpsc::Sender<Picture>);

    impl DecoderOutput {
        fn capture(&self, frame: &mut RawVideoFrame) {
            let luma = frame
                .buffer()
                .plane(RawVideoPlaneKind::Y)
                .expect("decoded luma plane");
            self.0
                .send(Picture {
                    size: [frame.width(), frame.height()],
                    timestamp: frame.metadata().rtp_timestamp,
                    nonconstant_luma: matches!(&luma.samples, RawVideoSamples::U8(samples)
                    if samples.windows(2).any(|pair| pair[0] != pair[1])),
                })
                .expect("capture decoded picture");
        }
    }

    impl DecodedImageCallbackProvider for DecoderOutput {
        fn decoded(&self, frame: &mut RawVideoFrame) -> i32 {
            self.capture(frame);
            0
        }
        fn decoded_with_time(&self, frame: &mut RawVideoFrame, _: i64) -> i32 {
            self.capture(frame);
            0
        }
        fn decoded_with_metadata(&self, frame: &mut RawVideoFrame, _: Option<i32>, _: Option<u8>) {
            self.capture(frame);
        }
    }

    assert!(
        frame.key_frame
            && frame
                .mime_type
                .eq_ignore_ascii_case(&format!("video/{name}"))
    );
    let environment = Environment::builder().build().expect("decoder environment");
    let factory = VideoDecoderFactoryHandle::builtin().expect("stock decoder factory");
    let (output, pictures) = std::sync::mpsc::channel();
    let callback = DecodedImageCallbackSink::new(DecoderOutput(output));
    let mut decoder = factory
        .create_decoder(&environment, &SdpVideoFormat::new(name))
        .expect("create fresh stock decoder");
    let mut settings = OwnedVideoDecoderSettings::default();
    let mut fields = settings.fields();
    fields.codec_type = codec_type;
    fields.number_of_cores = 1;
    fields.max_render_width = 1280;
    fields.max_render_height = 720;
    settings.set_fields(fields).expect("stock decoder settings");
    assert!(decoder.configure(&settings));
    assert_eq!(decoder.register_callback(Some(&callback)), 0);

    let mut image = EncodedImage::default();
    image.set_buffer(&EncodedVideoBuffer::from_bytes(&frame.data));
    let mut metadata = image.metadata();
    metadata.width = 1280;
    metadata.height = 720;
    metadata.rtp_timestamp = frame.rtp_timestamp;
    metadata.frame_type = EncodedImageFrameType::Key;
    image
        .set_metadata(&metadata)
        .expect("reassembled keyframe metadata");
    let status = decoder
        .decode_with_missing_frames(&image, false, 0)
        .expect("decode reassembled keyframe");
    assert!(
        status >= 0,
        "stock decoder rejected the reassembled keyframe: {status}"
    );
    let picture = pictures
        .try_recv()
        .expect("fresh decoder must output a picture");
    assert_eq!(picture.size, [1280, 720]);
    assert_eq!(picture.timestamp, frame.rtp_timestamp);
    assert!(picture.nonconstant_luma);
    assert_eq!(decoder.release(), 0);
}

fn is_srtcp(bytes: &[u8]) -> bool {
    bytes.first().is_some_and(|byte| byte >> 6 == 2)
        && bytes.get(1).is_some_and(|byte| (192..=223).contains(byte))
}

struct BoundedUdpPath {
    max_payload: usize,
    pair: Option<[SocketAddr; 2]>,
}

impl BoundedUdpPath {
    fn check(&mut self, mut pair: [SocketAddr; 2], payload: &[u8]) {
        assert!(
            (1..=self.max_payload).contains(&payload.len()),
            "UDP payload {} exceeds fixture limit {} on {pair:?}",
            payload.len(),
            self.max_payload,
        );
        // Connectivity checks may probe other candidates. Protected datagrams
        // must use the same endpoints in either direction.
        if payload.get(4..8) != Some(&[0x21, 0x12, 0xa4, 0x42][..]) {
            pair.sort_unstable();
            assert_eq!(
                *self.pair.get_or_insert(pair),
                pair,
                "protected UDP pair changed"
            );
        }
    }
}

#[derive(Clone, Copy)]
enum FixtureTransport {
    Udp,
    Tcp,
}
