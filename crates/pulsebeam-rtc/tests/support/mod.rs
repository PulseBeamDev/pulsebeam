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
    DataChannelEvent, DataChannelId, DataMessage, DeliveryConstraints, Event as ConnectionEvent,
    ForwardedMedia, FrameBoundary, FrameDependencies, FrameId, FrameMetadata, GlobalMediaTime,
    IceTcpFlowId, LocalCandidate, MediaPacket, MediaPayloadBitrate, NetworkInput,
    Output as ConnectionOutput, SdpOffer, SenderAssignment, SenderId, SourceIncarnation, TimePoint,
    TransmitTarget,
};
use pulsebeam_rtc_simulation::*;

pub struct MediaFixture {
    pub sender: SenderId,
    pub sender_mid: String,
}

pub struct PeerFixture<M = MediaFixture> {
    media: M,
    pub connection: Connection,
    pub senders: Vec<SenderId>,
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
    pub publisher_reports: usize,
    pub publisher_nacks: usize,
    pub publisher_feedback: usize,
    network: DeterministicNetwork,
    native_target: Option<fn(&Connection) -> Duration>,
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

struct FixtureTcpStream {
    flow: IceTcpFlowId,
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
            true,
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
    pub fn admit_media(&mut self, media: ForwardedMedia) -> Result<(), CommandError> {
        let deadline = self
            .at()
            .monotonic
            .checked_add(Duration::from_millis(150))
            .expect("fixture admission deadline");
        loop {
            match self.try_command(Command::SendMedia {
                sender: self.sender,
                media: media.clone(),
            }) {
                Err(CommandError::WouldBlock) if self.at().monotonic < deadline => {
                    self.drive_for(Duration::from_millis(1));
                }
                result => return result,
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

    pub fn connected_video_dd() -> Self {
        let mut fixture = Self::build(
            FixtureTransport::Udp,
            None,
            false,
            1,
            MediaKind::VideoDd,
            Self::default_start(),
            |senders| MediaFixture {
                sender: senders[0].id,
                sender_mid: senders[0].mid.to_string(),
            },
        );
        for _ in 0..2_000 {
            let _ = fixture.step();
            if fixture.peer_connected && fixture.connection_connected {
                return fixture;
            }
        }
        panic!("native DD peer did not connect");
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
        let mut fixture =
            Self::new_with_start(transport, limits, datachannels, sender_count, media, start);
        for _ in 0..2_000 {
            let _ = fixture.step();
            if fixture.peer_connected && fixture.connection_connected {
                return fixture;
            }
        }
        panic!(
            "standards peer did not connect: native={}, rtc={}, streams={}, stats={:?}, descriptions={:?}",
            fixture.peer_connected,
            fixture.connection_connected,
            fixture.tcp_streams.len(),
            fixture.connection.stats(),
            fixture.peer.descriptions(),
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
            datachannels,
            sender_count,
            media,
            start,
            |senders| {
                let sender = senders.first().expect("negotiated media sender");
                MediaFixture {
                    sender: sender.id,
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
        datachannels: bool,
        sender_count: usize,
        media: MediaKind,
        start: Instant,
        media_state: impl FnOnce(&[pulsebeam_rtc::SenderInfo]) -> M,
    ) -> Self {
        assert!(sender_count <= 128);
        assert!(sender_count != 0 || datachannels);
        let world = SimulationWorld::acquire();
        world.bind_clock(start);
        let network = world
            .controlled
            .create_network()
            .expect("controlled network");
        let client = network
            .register_endpoint("192.0.2.1".parse().expect("fixture endpoint IP"))
            .expect("fixture endpoint IP");
        let server = network
            .register_endpoint("192.0.2.2".parse().expect("fixture endpoint IP"))
            .expect("fixture endpoint IP");
        let socket = server
            .bind_udp(41000)
            .expect("controlled server UDP socket");
        let video_dd = matches!(media, MediaKind::VideoDd);
        let video_input = if video_dd {
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
            .controlled_media()
            .build()
            .expect("controlled native factory");
        let peer = factory
            .create_peer_connection(PeerConfiguration {
                always_negotiate_data_channels: datachannels,
                ..PeerConfiguration::default()
            })
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
                            if mixed {
                                RtpTransceiverDirection::ReceiveOnly
                            } else {
                                RtpTransceiverDirection::SendReceive
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
                (MediaKind::Video | MediaKind::VideoDd, _)
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
                            if mixed {
                                RtpTransceiverDirection::SendOnly
                            } else {
                                RtpTransceiverDirection::SendReceive
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
            transceivers.push(transceiver);
        }
        let peer_channel = datachannels.then(|| {
            peer.create_data_channel("peer-opened", DataChannelConfiguration::default())
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
        let mut accepted = Connection::accept(
            config,
            SdpOffer::new(offer.sdp),
            TimePoint {
                monotonic: now,
                global: GlobalMediaTime::from_micros(
                    1_000_000
                        + u64::try_from(now.duration_since(start).as_micros())
                            .expect("bounded simulation epoch"),
                ),
            },
            ConnectionEntropy::new([11; 32]),
        )
        .expect("RTC accepts actual native offer");
        let senders = accepted
            .session
            .senders
            .iter()
            .map(|s| s.id)
            .collect::<Vec<_>>();
        for &sender in &senders {
            accepted
                .connection
                .command(
                    TimePoint {
                        monotonic: now,
                        global: GlobalMediaTime::from_micros(
                            1_000_000
                                + u64::try_from(now.duration_since(start).as_micros())
                                    .expect("bounded epoch"),
                        ),
                    },
                    Command::SetSenderAssignment {
                        sender,
                        assignment: Some({
                            let mut assignment =
                                sender_assignment(MediaPayloadBitrate::from_bps(1_000_000));
                            assignment.delivery.max_queue_residence = Duration::from_millis(200);
                            assignment
                        }),
                    },
                )
                .expect("explicit fixture sender assignment");
        }
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
        if video_dd {
            let sender = transceivers[0].sender();
            let mut parameters = sender.parameters().expect("native sender parameters");
            parameters.encodings[0].scalability_mode = Some("L1T3".into());
            sender
                .set_parameters(parameters)
                .expect("native L1T3 configuration");
        }
        let audio_sink = audio_source.as_ref().map(|_| {
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
            publisher_reports: 0,
            publisher_nacks: 0,
            publisher_feedback: 0,
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

    pub fn receive_source_sequence(&mut self, sequence: u16) -> MediaPacket {
        for _ in 0..2_000 {
            if let Some(PeerEvent::Inbound(packet)) = self.step()
                && packet.view().expect("source RTP").sequence_number == sequence
            {
                return packet;
            }
        }
        panic!(
            "source sequence {sequence} was not recovered: nacks={}, rtx={}, reports={}, elapsed={:?}, stats={:?}",
            self.publisher_nacks,
            self.source_repair_packets,
            self.publisher_reports,
            self.now.duration_since(self.start),
            self.connection.stats()
        );
    }

    pub fn send_source(&mut self, payload: &[u8]) -> MediaPacket {
        self.send_video_temporal_source(payload, None)
    }

    pub fn send_video_temporal_source(
        &mut self,
        payload: &[u8],
        temporal_index: Option<u8>,
    ) -> MediaPacket {
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

    pub fn send_fragmented_video_source(&mut self) -> Vec<MediaPacket> {
        assert!(self.video_source.is_some());
        let mut payload = vec![0x55; 3_500];
        payload[0] = 0x65;
        let first = self.send_source(&payload);
        let mut packets = vec![first];
        for _ in 0..2_000 {
            if packets.last().expect("first source packet").bytes()[1] & 0x80 != 0 {
                assert!(
                    packets.len() >= 3,
                    "native frame includes a middle fragment"
                );
                return packets;
            }
            if let Some(PeerEvent::Inbound(packet)) = self.step() {
                assert_eq!(packet.global_media_at(), packets[0].global_media_at());
                packets.push(packet);
            }
        }
        panic!("native fragmented frame did not finish");
    }

    /// Observe transport submission only. Authenticated receipt is asserted separately.
    pub fn receive_video_wire_header(&mut self) -> FixtureRtpHeader {
        assert!(self.video_source.is_some());
        assert!(
            self.extension_ids.playout.is_some(),
            "negotiated video playout extension"
        );
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

    pub fn at(&self) -> TimePoint {
        let now = self.now.max(self.world.monotonic_now());
        TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(
                1_000_000
                    + u64::try_from(now.duration_since(self.start).as_micros())
                        .expect("bounded simulation epoch"),
            ),
        }
    }

    pub fn advance_clock_without_poll(&mut self, duration: Duration) {
        self.now = self
            .now
            .checked_add(duration)
            .expect("fixture clock advance");
        self.connection_idle = false;
    }

    pub fn drive_for(&mut self, duration: Duration) {
        self.now = self.at().monotonic;
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

    pub fn twcc_sent(&self) -> usize {
        self.twcc_sent
    }
    pub fn set_feedback_enabled(&mut self, enabled: bool) {
        self.feedback_enabled = enabled;
    }
    pub fn assignment(&self, sender: SenderId) -> SenderAssignment {
        self.connection
            .stats()
            .senders
            .iter()
            .find(|stats| stats.sender == sender)
            .expect("fixture sender")
            .assignment
            .expect("explicit fixture assignment")
    }

    pub fn command(&mut self, command: Command) {
        if let Err(error) = self.try_command(command) {
            panic!(
                "connection command: {error:?}; stats={:?}",
                self.connection.stats().connection,
            );
        }
    }
    pub fn try_command(&mut self, command: Command) -> Result<(), CommandError> {
        let result = self.connection.command(self.at(), command);
        if result.is_ok() {
            self.connection_idle = false;
        }
        result
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
        self.now = self.at().monotonic;
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
                PendingPacket::Connection { input, .. } => {
                    self.connection
                        .receive(self.at(), input)
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
                    if payload.first().is_some_and(|byte| byte >> 6 == 2) {
                        if payload.get(1) == Some(&201) {
                            self.publisher_reports += 1;
                        }
                        if payload.get(1) == Some(&205) {
                            match payload[0] & 31 {
                                1 => self.publisher_nacks += 1,
                                11 | 15 => self.publisher_feedback += 1,
                                _ => {}
                            }
                        }
                    }
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
            match packet.kind {
                OutboundKind::TcpConnect => {
                    let flow = IceTcpFlowId::from_value(self.tcp_streams.len() as u64 + 1);
                    self.command(Command::AdmitIceTcpFlow {
                        flow,
                        local: SocketAddr::new(packet.destination.ip(), packet.destination.port()),
                        remote: SocketAddr::new(packet.source.ip(), packet.source.port()),
                    });
                    self.tcp_streams.push(FixtureTcpStream {
                        flow,
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
                    // Native non-TLS TCP exposes AsyncPacketSocket payloads, not stream framing.
                    let size = u16::try_from(packet.payload.len()).expect("RFC 4571 packet length");
                    let mut frame = Vec::with_capacity(packet.payload.len() + 2);
                    frame.extend_from_slice(&size.to_be_bytes());
                    frame.extend_from_slice(&packet.payload);
                    self.network.enqueue(PendingPacket::Connection {
                        due: self.now,
                        input: NetworkInput::IceTcp {
                            flow: stream.flow,
                            local: SocketAddr::new(stream.local.ip(), stream.local.port()),
                            remote: SocketAddr::new(stream.remote.ip(), stream.remote.port()),
                            frame: Bytes::from(frame),
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
            let rtcp = bytes.first().is_some_and(|byte| byte >> 6 == 2)
                && bytes.get(1).is_some_and(|byte| (192..=223).contains(byte));
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
            match self.connection.poll(self.at()) {
                ConnectionOutput::Transmit(transmit) => {
                    let (destination, payload) = match transmit.target {
                        TransmitTarget::Udp { remote, .. } => (remote, transmit.payload.as_ref()),
                        TransmitTarget::IceTcp { flow } => {
                            let stream = self
                                .tcp_streams
                                .iter()
                                .find(|stream| stream.flow == flow)
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
                                .inject_tcp_data(stream.local, stream.remote, packet)
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
                ConnectionOutput::Event(ConnectionEvent::Media { packet, .. }) => {
                    return Some(PeerEvent::Inbound(packet));
                }
                ConnectionOutput::Event(ConnectionEvent::DataChannel(event)) => {
                    return Some(PeerEvent::ConnectionData(event));
                }
                ConnectionOutput::Event(_) => return None,
                ConnectionOutput::Idle { next_wakeup } => {
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

pub fn second_negotiated_sender() -> SenderId {
    PeerFixture::new_with(FixtureTransport::Udp, None, false, 2, MediaKind::Audio).senders[1]
}

#[derive(Clone, Copy)]
enum MediaKind {
    Audio,
    Video,
    VideoDd,
    VideoPublisherAudioSubscriber,
}

#[derive(Clone, Debug)]
pub struct FixtureRtpHeader {
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

pub fn assert_probe_windows(emitted: &[(Instant, u64, bool)]) {
    assert_probe_windows_with_allowance(emitted, |_, _| 48 * 1024);
}

pub fn assert_initial_probe_windows(
    emitted: &[(Instant, u64, bool)],
    started_at: Instant,
    potential_bps: u64,
) {
    let initial_allowance = potential_bps.div_ceil(20).clamp(48 * 1024, 96 * 1024);
    assert_probe_windows_with_allowance(emitted, |at, new_probe| {
        let elapsed = at.saturating_duration_since(started_at);
        if at >= started_at
            && (elapsed < Duration::from_secs(5)
                || (!new_probe && elapsed < Duration::from_secs(10)))
        {
            initial_allowance
        } else {
            48 * 1024
        }
    });
}

#[allow(
    clippy::print_stderr,
    reason = "report actual production probe overhead separately from the allowance"
)]
fn assert_probe_windows_with_allowance(
    emitted: &[(Instant, u64, bool)],
    allowance: impl Fn(Instant, bool) -> u64,
) {
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
    let mut check = |at: Instant, nonprobe: u64, probes: u64, new_probe: bool| {
        let total = probes + nonprobe;
        peak_ratio = peak_ratio.max(if total == 0 { 0 } else { probes * 100 / total });
        peak_allowance = peak_allowance.max(probes.saturating_sub(nonprobe / 19));
        assert!(
            probes <= nonprobe / 19 + allowance(at, new_probe),
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
        check(at, nonprobe, probes, false);
        while next < emitted.len() && emitted[next].0 == at {
            let (_, bytes, probe) = emitted[next];
            if probe {
                probes += bytes;
            } else {
                nonprobe += bytes;
            }
            next += 1;
            // Later media at this same clock instant cannot fund an earlier probe.
            check(at, nonprobe, probes, probe);
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

pub fn sender_assignment(rate: MediaPayloadBitrate) -> SenderAssignment {
    SenderAssignment {
        source: SourceIncarnation::from_value(1),
        selected_bitrate: rate,
        potential_bitrate: rate,
        delivery: DeliveryConstraints {
            max_media_age: Duration::from_secs(1),
            max_queue_residence: Duration::from_millis(80),
            repair_window: Duration::from_secs(1),
        },
        playout_delay: None,
        decode_target_mask: None,
    }
}

pub fn forwarded(packet: MediaPacket, id: u64) -> ForwardedMedia {
    ForwardedMedia {
        source: SourceIncarnation::from_value(1),
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

#[derive(Clone, Copy)]
enum FixtureTransport {
    Udp,
    Tcp,
}
