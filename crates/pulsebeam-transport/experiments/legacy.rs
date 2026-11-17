// Test-only fixture drivers retained for unique protocol/codec controls.
const FRAME_MS: u64 = 33;
const DEADLINE_MS: u64 = 180;
const KEY: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x0a, 0xd9, 0x1e, 0x84, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0xf0, 0x3c,
    0x48, 0x99, 0x20, 0, 0, 0, 1, 0x68, 0xcb, 0x80, 0xc4, 0xb2, 0, 0, 0, 1, 0x65, 0x88, 0x84, 0xf1,
    0x18, 0xa0, 0, 0x20, 0x5b, 0x1c, 0, 4, 7, 0xe3, 0x80, 0, 0x80, 0xfe,
];
const FRAME_TAG: &[u8; 16] = b"pulsebeam-frame:";

fn frame(id: usize) -> Vec<u8> {
    let size = if id.is_multiple_of(60) {
        20_000
    } else {
        4_000 + id % 5 * 700
    };
    // Preserve the complete test-fixture slice from libwebrtc-sys/tests/video.rs.
    // The source API admits SEI, not filler_data NALs. Unregistered user data
    // carries identity and padding without modifying the coded picture.
    let payload_len = (1..=size / 255 + 1)
        .find_map(|header_len| {
            let payload_len = size - KEY.len() - 7 - header_len;
            (payload_len / 255 + 1 == header_len).then_some(payload_len)
        })
        .unwrap();
    let mut bytes = KEY[..34].to_vec();
    bytes.extend_from_slice(&[0, 0, 0, 1, 6, 5]);
    bytes.resize(bytes.len() + payload_len / 255, 255);
    bytes.push(u8::try_from(payload_len % 255).unwrap());
    bytes.extend_from_slice(FRAME_TAG);
    bytes.extend_from_slice(format!("{id:08x}").as_bytes());
    bytes.resize(size - (KEY.len() - 34) - 1, 0x55);
    bytes.push(0x80);
    bytes.extend_from_slice(&KEY[34..]);
    assert_eq!(bytes.len(), size);
    bytes
}

struct Metrics {
    frames: usize,
    complete: BTreeMap<usize, u64>,
    parts: BTreeMap<usize, BTreeSet<u16>>,
    useful_bytes: u64,
    expired_packets: u64,
    peak_app_bytes: Option<usize>,
    quic_stalls: Option<u64>,
    staged_expired_packets: Option<u64>,
    closed: bool,
    failure: Option<Error>,
    failure_at_ms: Option<u64>,
    network_drops: u64,
    quic_losses: u64,
}
impl Metrics {
    fn new(duration: u64) -> Self {
        let frames = usize::try_from(duration.div_ceil(FRAME_MS)).unwrap();
        Self {
            frames,
            complete: BTreeMap::new(),
            parts: BTreeMap::new(),
            useful_bytes: 0,
            expired_packets: 0,
            peak_app_bytes: None,
            quic_stalls: None,
            staged_expired_packets: None,
            closed: false,
            failure: None,
            failure_at_ms: None,
            network_drops: 0,
            quic_losses: 0,
        }
    }
    fn completed(&mut self, id: usize, now: u64) {
        if id >= self.frames || self.complete.contains_key(&id) {
            return;
        }
        self.complete.insert(id, now);
        if now <= id as u64 * FRAME_MS + DEADLINE_MS {
            self.useful_bytes += frame(id).len() as u64;
        }
    }
    fn packet(&mut self, bytes: &[u8], now: u64) {
        assert!(bytes.len() >= 16);
        let id = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize / 3000;
        let part = u16::from_be_bytes(bytes[12..14].try_into().unwrap());
        let count = u16::from_be_bytes(bytes[14..16].try_into().unwrap()) as usize;
        assert!(id < self.frames);
        let expected = frame(id);
        assert_eq!(count, expected.len().div_ceil(CHUNK));
        let offset = usize::from(part) * CHUNK;
        assert!(usize::from(part) < count);
        assert_eq!(
            &bytes[16..],
            &expected[offset..(offset + CHUNK).min(expected.len())]
        );
        if self.parts.entry(id).or_default().insert(part) && self.parts[&id].len() == count {
            self.completed(id, now);
        }
    }
}

fn packets(id: usize, epoch: u64, send_ttl_ms: u64) -> Vec<Packet> {
    let data = frame(id);
    let total = data.len().div_ceil(CHUNK);
    data.chunks(CHUNK)
        .enumerate()
        .map(|(part, bytes)| {
            let mut packet = vec![0x80, 111, 0, 0];
            packet.extend_from_slice(&(u32::try_from(id).unwrap() * 3000).to_be_bytes());
            packet.extend_from_slice(&7u32.to_be_bytes());
            packet.extend_from_slice(&u16::try_from(part).unwrap().to_be_bytes());
            packet.extend_from_slice(&u16::try_from(total).unwrap().to_be_bytes());
            packet.extend_from_slice(bytes);
            Packet {
                bytes: packet,
                expires: Time((epoch + id as u64 * FRAME_MS + send_ttl_ms) * 1_000_000),
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
struct Audit {
    complete_drain: bool,
    retain_protected: bool,
    send_ttl_ms: u64,
    poll_budget: usize,
}
impl Default for Audit {
    fn default() -> Self {
        Self {
            complete_drain: true,
            retain_protected: true,
            send_ttl_ms: DEADLINE_MS,
            poll_budget: 256,
        }
    }
}

fn quic_run(
    scenario: Scenario,
    seed: u64,
    duration: u64,
    cc: QuicCongestion,
    overlay: bool,
    audit: Audit,
) -> Metrics {
    let credentials = [
        QuicCredential::generate().unwrap(),
        QuicCredential::generate().unwrap(),
    ];
    let addresses: [SocketAddr; 2] = [
        "127.0.0.1:41001".parse().unwrap(),
        "127.0.0.1:41002".parse().unwrap(),
    ];
    let origin = Instant::now();
    let mut peers = std::array::from_fn::<_, 2, _>(|who| {
        Session::new(
            QuicPeer {
                endpoint: addresses[1 - who],
                credential: credentials[who].clone(),
                expected_peer: credentials[1 - who].fingerprint(),
                role: if who == 0 {
                    PeerRole::Initiator
                } else {
                    PeerRole::Responder
                },
                class: NetworkClass::Wan,
                tuning: match cc {
                    QuicCongestion::OuterControlled => QuicTuning::default(),
                    _ => QuicTuning::webtransport_like(cc).unwrap(),
                },
            },
            overlay,
            origin,
        )
    });
    let mut inbound = [VecDeque::<Vec<u8>>::new(), VecDeque::new()];
    let mut epoch = 0;
    let mut link = Link::new(scenario, seed, u64::MAX / 4);
    for at in 0..5000 {
        while let Some((to, bytes)) = link.pop(at) {
            inbound[to].push_back(bytes);
        }
        for who in 0..2 {
            let mut output = Vec::new();
            peers[who].poll(at, &mut output, &mut Vec::new()).unwrap();
            for bytes in output {
                let _ = link.push(at, who, bytes.len(), bytes);
            }
            for _ in 0..256 {
                if !peers[who].input_ready() {
                    break;
                }
                let Some(bytes) = inbound[who].pop_front() else {
                    break;
                };
                peers[who].receive(addresses[1 - who], &bytes, at);
                let mut output = Vec::new();
                peers[who].poll(at, &mut output, &mut Vec::new()).unwrap();
                for bytes in output {
                    let _ = link.push(at, who, bytes.len(), bytes);
                }
            }
        }
        if at >= 200
            && peers.iter().all(Session::ready)
            && inbound.iter().all(VecDeque::is_empty)
            && link.pending.is_empty()
        {
            epoch = at + 1;
            break;
        }
    }
    assert!(epoch > 0, "QUIC handshake did not complete");
    link.begin_measurement(epoch, duration, seed);
    let mut metrics = Metrics::new(duration);
    let mut queued = VecDeque::<Packet>::new();
    for elapsed in 0..duration + 600 {
        let at = epoch + elapsed;
        while let Some((to, bytes)) = link.pop(elapsed) {
            inbound[to].push_back(bytes);
        }
        if elapsed < duration && elapsed.is_multiple_of(FRAME_MS) {
            queued.extend(packets(
                (elapsed / FRAME_MS) as usize,
                epoch,
                audit.send_ttl_ms,
            ));
        }
        while queued
            .front()
            .is_some_and(|packet| packet.expires.0 <= at * 1_000_000)
        {
            queued.pop_front();
            metrics.expired_packets += 1;
        }
        metrics.peak_app_bytes = Some(
            metrics
                .peak_app_bytes
                .unwrap_or(0)
                .max(queued.iter().map(|p| p.bytes.len()).sum()),
        );
        if !scenario.turn(elapsed) {
            continue;
        }
        peers[0].demand(!queued.is_empty());
        for who in 0..2 {
            let mut settled = false;
            for _ in 0..if audit.complete_drain { 4096 } else { 64 } {
                let mut output = Vec::new();
                let mut received = Vec::new();
                if peers[who]
                    .poll_budget(at, audit.poll_budget, &mut output, &mut received)
                    .is_err()
                {
                    break;
                }
                for bytes in output {
                    let _ = link.push(elapsed, who, bytes.len(), bytes);
                }
                if who == 1 {
                    for bytes in received {
                        metrics.packet(&bytes, elapsed);
                    }
                }
                if !peers[who].input_ready() {
                    if audit.complete_drain && peers[who].needs_progress() {
                        continue;
                    }
                    settled = !peers[who].needs_progress();
                    break;
                }
                if let Some(bytes) = inbound[who].pop_front() {
                    peers[who].receive(addresses[1 - who], &bytes, at);
                    continue;
                }
                if who == 0
                    && let Some(packet) = queued.front()
                {
                    match peers[who].submit(packet, audit.retain_protected) {
                        Ok(()) => {
                            queued.pop_front();
                            continue;
                        }
                        Err(Error::Blocked | Error::NotReady) => {}
                        Err(error @ (Error::Closed | Error::FeedbackUnavailable)) => {
                            metrics.closed = true;
                            metrics.failure = Some(error);
                            metrics.failure_at_ms = Some(elapsed);
                        }
                        Err(error) => panic!("unexpected admission error {error:?}"),
                    }
                }
                settled = true;
                break;
            }
            assert!(
                !audit.complete_drain || settled || metrics.closed,
                "peer turn exhausted before quiescence"
            );
        }
        if metrics.closed {
            break;
        }
    }
    metrics.quic_stalls = Some(peers.iter().map(Session::stalls).sum());
    metrics.staged_expired_packets = Some(
        peers
            .iter()
            .map(|peer| match peer {
                Session::Bare(port) => port.expired(),
                Session::Overlay(port) => port.health().expired,
            })
            .sum(),
    );
    metrics.network_drops = link.dropped;
    metrics.quic_losses = peers.iter().map(Session::losses).sum();
    metrics
}

struct Native {
    peers: [PeerConnection; 2],
    source: Option<EncodedVideoSource>,
    raw_source: Option<VideoSource>,
    sender: RtpSender,
    sinks: Vec<EncodedVideoSink>,
    decoded_sinks: Vec<VideoSink>,
    decode_video: bool,
    operations: Vec<(usize, OperationCompletion)>,
    gathered: [bool; 2],
    connected: [bool; 2],
    network: ControlledSimulatedNetwork,
    _endpoints: Vec<NetworkEndpoint>,
    _factories: Vec<PeerConnectionFactory>,
    _input: EncodedVideoInput,
    _track: VideoTrack,
    world: ControlledWorld,
    link: Link<u64>,
    ip_udp_overhead: usize,
    scream_feedback: std::sync::mpsc::Receiver<()>,
}
impl Native {
    fn configured(seed: u64, scenario: Scenario, options: NativeOptions) -> Self {
        let NativeOptions {
            decode_video,
            raw_video,
            disable_frame_dropper,
            controller,
            initial_bps,
            minimum_bps,
        } = options;
        assert!(!disable_frame_dropper || raw_video);
        let (controller_log, scream_feedback) = ControllerLog::channel();
        let world = ControlledWorld::acquire(seed, Duration::from_secs(10)).unwrap();
        let network = world.create_network().unwrap();
        let endpoints: Vec<_> = [Ipv4Addr::new(10, 79, 0, 1), Ipv4Addr::new(10, 79, 0, 2)]
            .into_iter()
            .map(|ip| network.register_endpoint(ip.into()).unwrap())
            .collect();
        let input = EncodedVideoInput::new().unwrap();
        let decoder = if decode_video {
            VideoDecoderFactoryHandle::builtin().unwrap()
        } else {
            input.encoded_receive_factory().unwrap()
        };
        let factories: Vec<_> = endpoints
            .iter()
            .enumerate()
            .map(|(who, endpoint)| {
                let builder = world
                    .peer_factory_builder()
                    .unwrap()
                    .network_manager(endpoint.network_manager().unwrap())
                    .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
                    .video_encoder_factory(if raw_video {
                        VideoEncoderFactoryHandle::builtin().unwrap()
                    } else {
                        input.encoder_factory()
                    })
                    .video_decoder_factory(decoder.clone());
                let builder = if who == 0 {
                    builder.event_log_factory(controller_log.clone())
                } else {
                    builder
                };
                builder.build().unwrap()
            })
            .collect();
        let peers = [
            factories[0]
                .create_peer_connection_with_dependencies(
                    PeerConfiguration::default().into(),
                    PeerConnectionDependencies::default().field_trials(NativeTrials {
                        base: factories[0].field_trials(),
                        controller,
                        disable_frame_dropper,
                    }),
                )
                .unwrap(),
            factories[1]
                .create_peer_connection(PeerConfiguration::default())
                .unwrap(),
        ];
        peers[0]
            .set_bitrate(minimum_bps, Some(initial_bps), None)
            .unwrap();
        let source = (!raw_video).then(|| input.create_source(&factories[0]).unwrap());
        let raw_source = raw_video.then(|| factories[0].create_video_source().unwrap());
        let track = if let Some(raw) = &raw_source {
            factories[0]
                .create_video_track("udp-baseline", raw)
                .unwrap()
        } else {
            source
                .as_ref()
                .unwrap()
                .create_track(&factories[0], "encoded-trace")
                .unwrap()
        };
        let transceiver = peers[0]
            .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
            .unwrap();
        let codecs: Vec<_> = peers[0]
            .video_sender_capabilities()
            .unwrap()
            .into_iter()
            .filter(|codec| codec.format().name == "H264")
            .collect();
        assert!(!codecs.is_empty());
        transceiver.set_codec_preferences(&codecs).unwrap();
        let mut native = Self {
            peers,
            source,
            raw_source,
            sender: transceiver.sender(),
            sinks: Vec::new(),
            decoded_sinks: Vec::new(),
            decode_video,
            operations: Vec::new(),
            gathered: [false; 2],
            connected: [false; 2],
            network,
            _endpoints: endpoints,
            _factories: factories,
            _input: input,
            _track: track,
            world,
            link: Link::new(scenario, seed, u64::MAX / 4),
            ip_udp_overhead: if decode_video { 28 } else { 0 },
            scream_feedback,
        };
        let offer_op = native.peers[0].create_offer().unwrap();
        let offer = native.operation(0, offer_op).unwrap();
        let set_op = native.peers[0].set_local_description(offer).unwrap();
        native.operation(0, set_op);
        let offer = native.gather(0);
        let set_op = native.peers[1].set_remote_description(offer).unwrap();
        native.operation(1, set_op);
        let answer_op = native.peers[1].create_answer().unwrap();
        let answer = native.operation(1, answer_op).unwrap();
        let set_op = native.peers[1].set_local_description(answer).unwrap();
        native.operation(1, set_op);
        let answer = native.gather(1);
        let set_op = native.peers[0].set_remote_description(answer).unwrap();
        native.operation(0, set_op);
        for _ in 0..2000 {
            native.warm_tick();
            if native.connected == [true; 2]
                && (!native.sinks.is_empty() || !native.decoded_sinks.is_empty())
            {
                break;
            }
        }
        assert_eq!(native.connected, [true; 2]);
        assert!(!native.sinks.is_empty() || !native.decoded_sinks.is_empty());
        for _ in 0..200 {
            native.warm_tick();
        }
        native
    }
    fn events(&mut self) {
        for who in 0..2 {
            while let Some(event) = self.peers[who].try_next_event() {
                match event {
                    PeerConnectionEvent::OperationComplete(result) => {
                        self.operations.push((who, result));
                    }
                    PeerConnectionEvent::IceGatheringStateChanged(state) => {
                        self.gathered[who] = state == IceGatheringState::Complete;
                    }
                    PeerConnectionEvent::ConnectionStateChanged(state) => {
                        self.connected[who] = state == ConnectionState::Connected;
                    }
                    PeerConnectionEvent::Track(transceiver) if who == 1 => {
                        if self.decode_video {
                            self.decoded_sinks.push(
                                transceiver
                                    .receiver()
                                    .track()
                                    .unwrap()
                                    .attach_sink()
                                    .unwrap(),
                            );
                        } else {
                            self.sinks
                                .push(transceiver.receiver().attach_encoded_sink().unwrap());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    fn pump(&self) {
        for _ in 0..32 {
            let result = self.world.pump(512);
            assert!(!result.blocked, "native dispatch unexpectedly blocked");
            if !result.ready {
                return;
            }
            assert!(result.dispatched > 0);
        }
        panic!("native continuation did not settle");
    }
    fn warm_tick(&mut self) {
        let at = u64::try_from(self.world.now().as_millis()).unwrap() - 10_000;
        self.pump();
        while let Some((_, id)) = self.link.pop(at) {
            self.network.deliver(id).unwrap();
        }
        self.pump();
        while let Some(packet) = self.network.next_packet() {
            let from = usize::from(packet.source.ip() != Ipv4Addr::new(10, 79, 0, 1));
            if let Err(id) = self.link.push(
                at,
                from,
                packet.payload.len() + self.ip_udp_overhead,
                packet.id,
            ) {
                self.network.drop_packet(id).unwrap();
            }
        }
        self.events();
        self.world.advance(Duration::from_millis(1)).unwrap();
    }
    fn operation(&mut self, who: usize, id: OperationId) -> Option<SessionDescription> {
        for _ in 0..2000 {
            self.warm_tick();
            if let Some(at) = self
                .operations
                .iter()
                .position(|(owner, op)| *owner == who && op.operation_id == id)
            {
                return self.operations.remove(at).1.result.unwrap();
            }
        }
        panic!("native operation timed out");
    }
    fn gather(&mut self, who: usize) -> SessionDescription {
        for _ in 0..2000 {
            self.warm_tick();
            if self.gathered[who] {
                let descriptions = self.peers[who].descriptions().unwrap();
                return descriptions
                    .pending_local
                    .or(descriptions.current_local)
                    .unwrap();
            }
        }
        panic!("native gathering timed out");
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        if let Some(source) = &mut self.source {
            source.close().unwrap();
        }
        if let Some(source) = &mut self.raw_source {
            source.close().unwrap();
        }
        self.decoded_sinks.clear();
        for sink in &mut self.sinks {
            sink.close();
        }
        for peer in &mut self.peers {
            peer.close().unwrap();
        }
    }
}
