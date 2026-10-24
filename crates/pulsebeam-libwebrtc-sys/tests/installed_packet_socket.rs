//! Real SDK ICE/DTLS/SCTP over a caller-routed general socket factory.
//! No endpoint packet provider, physical socket, or fabricated protocol reply.
use pulsebeam_webrtc_sys::*;
use std::{
    cell::Cell,
    collections::VecDeque,
    net::Ipv4Addr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[path = "support/tools.rs"]
mod tools;

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[path = "installed_packet_socket/mdns_candidates.rs"]
mod mdns_candidates;
#[path = "installed_packet_socket/tls_verifier.rs"]
mod tls_verifier;
#[path = "installed_packet_socket/video_consumers.rs"]
mod video_consumers;

#[derive(Default)]
struct Link {
    sockets: Mutex<Vec<(SocketAddress, PacketSocketEvents)>>,
    packets: Mutex<VecDeque<(SocketAddress, ReceivedIpPacket)>>,
    options: Mutex<Vec<AsyncSocketPacketOptions>>,
    next_port: AtomicUsize,
    created: AtomicUsize,
    dropped: AtomicUsize,
    delivered: AtomicUsize,
    rtp_headers: Mutex<Vec<(SocketAddress, u32, u8)>>,
    nonfinite_remaining: AtomicUsize,
    nonfinite_submitted: AtomicUsize,
    manager_created: AtomicUsize,
    manager_dropped: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
}
struct Udp {
    context: PacketSocketContext,
    local: SocketAddress,
    link: Arc<Link>,
    closed: Cell<bool>,
    error: Cell<i32>,
}
impl PacketSocket for Udp {
    fn local_address(&self) -> SocketAddress {
        self.local.clone()
    }
    fn remote_address(&self) -> SocketAddress {
        SocketAddress::new(Ipv4Addr::UNSPECIFIED, 0)
    }
    fn send(&self, _: &[u8], _: &AsyncSocketPacketOptions) -> i32 {
        -1
    }
    fn send_to(
        &self,
        bytes: &[u8],
        remote: SocketAddress,
        options: &AsyncSocketPacketOptions,
    ) -> i32 {
        if self.closed.get() {
            return -1;
        }
        if bytes.len() >= 12 && bytes[0] & 0xc0 == 0x80 && !(192..=223).contains(&bytes[1]) {
            self.link.rtp_headers.lock().unwrap().push((
                self.local.clone(),
                u32::from_be_bytes(bytes[8..12].try_into().unwrap()),
                bytes[1] & 0x7f,
            ));
        }
        let mut packet = ReceivedIpPacket::new(bytes.to_vec(), self.local.clone());
        packet.arrival_time = Some(self.context.environment().timestamp());
        self.link
            .packets
            .lock()
            .unwrap()
            .push_back((remote, packet));
        self.link.options.lock().unwrap().push(options.clone());
        let mut info = options.info_signaled_after_sent.clone();
        self.context
            .copy_socket_information(bytes.len(), &mut info)
            .unwrap();
        self.context
            .events()
            .post_sent(SentPacketInfo {
                packet_id: options.packet_id,
                send_time_ms: self.context.environment().timestamp().millis().unwrap(),
                info,
            })
            .unwrap();
        bytes.len().try_into().unwrap()
    }
    fn close(&self) -> i32 {
        self.closed.set(true);
        0
    }
    fn state(&self) -> PacketSocketState {
        if self.closed.get() {
            PacketSocketState::CLOSED
        } else {
            PacketSocketState::BOUND
        }
    }
    fn get_option(&self, _: PacketSocketOption) -> Result<i32, PacketSocketOptionError> {
        Ok(0)
    }
    fn set_option(&self, _: PacketSocketOption, _: i32) -> i32 {
        0
    }
    fn error(&self) -> i32 {
        self.error.get()
    }
    fn set_error(&self, value: i32) {
        self.error.set(value);
    }
}
impl Drop for Udp {
    fn drop(&mut self) {
        self.link
            .sockets
            .lock()
            .unwrap()
            .retain(|(address, _)| *address != self.local);
        self.link.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Sockets {
    ip: Ipv4Addr,
    link: Arc<Link>,
}
impl PacketSocketFactoryImplementation for Sockets {
    fn create_udp_socket(
        &self,
        context: PacketSocketContext,
        local: SocketAddress,
        min: u16,
        max: u16,
    ) -> Option<Box<dyn PacketSocket>> {
        assert_eq!(
            context
                .environment()
                .field_trials()
                .lookup("Provider-Transport"),
            "Selected"
        );
        assert_eq!(context.state(), Err(PacketSocketEventError::NotReady));
        assert_eq!(local.ip_address(), IpAddress::from(self.ip));
        let port = if local.port() != 0 {
            local.port()
        } else {
            40000 + u16::try_from(self.link.next_port.fetch_add(1, Ordering::SeqCst)).unwrap()
        };
        assert!(min == 0 || port >= min);
        assert!(max == 0 || port <= max);
        let local = SocketAddress::new(self.ip, port);
        let mut sockets = self.link.sockets.lock().unwrap();
        assert!(!sockets.iter().any(|(address, _)| *address == local));
        sockets.push((local.clone(), context.events()));
        drop(sockets);
        self.link.created.fetch_add(1, Ordering::SeqCst);
        Some(Box::new(Udp {
            context,
            local,
            link: self.link.clone(),
            closed: Cell::new(false),
            error: Cell::new(0),
        }))
    }
    fn create_server_tcp_socket(
        &self,
        _: ListenSocketContext,
        _: SocketAddress,
        _: u16,
        _: u16,
        _: PacketSocketFlags,
    ) -> Option<Box<dyn ListenSocket>> {
        None
    }
    fn create_client_tcp_socket(
        &self,
        _: PacketSocketContext,
        _: SocketAddress,
        _: SocketAddress,
        _: PacketSocketTcpOptions,
    ) -> Option<Box<dyn PacketSocket>> {
        None
    }
    fn create_client_udp_socket(
        &self,
        _: PacketSocketContext,
        _: SocketAddress,
        _: SocketAddress,
        _: u16,
        _: u16,
        _: PacketSocketTcpOptions,
    ) -> Option<Box<dyn PacketSocket>> {
        None
    }
    fn create_async_dns_resolver(&self, _: Environment) -> Box<dyn AsyncDnsResolver> {
        panic!("IP-only direct ICE must not request DNS")
    }
    fn is_controlled(&self) -> bool {
        true
    }
}
struct Managers {
    ip: Ipv4Addr,
    link: Arc<Link>,
}
struct Manager {
    network: Network,
    signals: NetworkManagerSignals,
    link: Arc<Link>,
}
impl NetworkManagerFactoryProvider for Managers {
    fn create(&self, context: NetworkManagerContext) -> Option<Box<dyn NetworkManager>> {
        assert_eq!(
            context
                .environment()
                .field_trials()
                .lookup("Provider-Transport"),
            "Selected"
        );
        let mut network = Network::with_adapter(
            b"provider-udp",
            b"caller routed UDP",
            Ipv4Addr::new(10, 84, 0, 0),
            24,
            NetworkAdapterType::Ethernet,
        );
        network.add_ip(self.ip);
        self.link.manager_created.fetch_add(1, Ordering::SeqCst);
        Some(Box::new(Manager {
            network,
            signals: context.signals(),
            link: self.link.clone(),
        }))
    }
    fn is_controlled(&self) -> bool {
        true
    }
}
impl NetworkManager for Manager {
    fn start_updating(&self) {
        self.link.starts.fetch_add(1, Ordering::SeqCst);
        assert!(self.signals.notify_networks_changed());
    }
    fn stop_updating(&self) {
        self.link.stops.fetch_add(1, Ordering::SeqCst);
    }
    fn networks(&self) -> Vec<Network> {
        vec![self.network.share()]
    }
    fn any_address_networks(&self) -> Vec<Network> {
        self.networks()
    }
    fn default_local_address(&self, family: AddressFamily) -> Option<IpAddress> {
        (family == AddressFamily::Ipv4).then(|| self.network.best_ip())
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        self.link.manager_dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Harness {
    // Rust fields drop in declaration order. Release all SDK context aliases
    // before the final driver/world owner drains creator-side retirement.
    environment: Environment,
    queues: TaskQueueFactory,
    link: Arc<Link>,
    driver: ControlledPeerDriver,
    world: ControlledWorld,
}
impl Harness {
    fn new() -> Self {
        let world = ControlledWorld::acquire(0x84, Duration::from_secs(1)).unwrap();
        let link = Arc::new(Link::default());
        let runtime = world.runtime().unwrap();
        let mut selected = runtime.environment_factory().unwrap();
        selected
            .set_field_trials(
                FieldTrials::parse(concat!(
                    "Provider-Transport/Selected/",
                    "WebRTC-Bwe-InjectedCongestionController/Enabled/",
                    "WebRTC-Audio-SendSideBwe/Enabled/",
                    "WebRTC-RFC8888CongestionControlFeedback/Disabled/",
                ))
                .unwrap(),
            )
            .unwrap();
        let environment = runtime.create_environment(&selected).unwrap();
        let queues = environment.task_queue_factory().unwrap();
        let driver = world.driver().clone();
        Self {
            world,
            queues,
            driver,
            environment,
            link,
        }
    }
    fn factory(
        &self,
        ip: Ipv4Addr,
    ) -> (
        PeerConnectionFactory,
        PacketSocketFactory,
        NetworkManagerFactory,
    ) {
        self.factory_with_controller(ip, None)
    }
    fn factory_with_controller(
        &self,
        ip: Ipv4Addr,
        controller: Option<&NetworkControllerFactory>,
    ) -> (
        PeerConnectionFactory,
        PacketSocketFactory,
        NetworkManagerFactory,
    ) {
        self.factory_with_selected_media(ip, controller, None, None)
    }
    fn factory_with_selected_media(
        &self,
        ip: Ipv4Addr,
        controller: Option<&NetworkControllerFactory>,
        input: Option<&EncodedVideoInput>,
        fec: Option<&FecControllerFactory>,
    ) -> (
        PeerConnectionFactory,
        PacketSocketFactory,
        NetworkManagerFactory,
    ) {
        let sockets = PacketSocketFactory::from_provider(Sockets {
            ip,
            link: self.link.clone(),
        });
        let managers = NetworkManagerFactory::from_provider(Managers {
            ip,
            link: self.link.clone(),
        });
        let mut builder = PeerConnectionFactory::builder()
            .environment(self.environment.clone())
            .controlled_driver(&self.driver)
            .options(FactoryOptions {
                disable_network_monitor: true,
                ssl_max_version: SslProtocolVersion::DTLS_12,
                ..Default::default()
            })
            .network_manager_factory(&managers)
            .socket_factory(&sockets);
        if let Some(fec) = fec {
            builder = builder.fec_controller_factory(fec);
        }
        if let Some(controller) = controller {
            let encoder = input.map_or_else(
                || {
                    EncodedVideoInput::new_for_format(VideoCodecFormat::new("VP8"))
                        .unwrap()
                        .encoder_factory()
                },
                |input| input.encoder_factory(),
            );
            builder = builder
                .network_controller_factory(controller)
                .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
                .audio_decoder_factory(AudioDecoderFactory::builtin_opus().unwrap())
                .video_encoder_factory(encoder)
                .video_decoder_factory(VideoDecoderFactoryHandle::builtin_vp8().unwrap());
        }
        (builder.build().unwrap(), sockets, managers)
    }
    fn step(&self) {
        assert!(!self.driver.pump(1024).blocked);
        assert!(!self.queues.pump(1024).unwrap().blocked);
        let packets = std::mem::take(&mut *self.link.packets.lock().unwrap());
        for (destination, mut packet) in packets {
            // RFC 5761 demultiplexing of actual encrypted RTP headers. Alter
            // only first delivery metadata, never bytes, auth tags or replies.
            let rtp = packet.payload.len() >= 12
                && packet.payload[0] & 0xc0 == 0x80
                && !(192..=223).contains(&packet.payload[1]);
            if rtp
                && self
                    .link
                    .nonfinite_remaining
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
            {
                packet.arrival_time = Some(Timestamp::plus_infinity());
                self.link.nonfinite_submitted.fetch_add(1, Ordering::SeqCst);
            }
            let events = self
                .link
                .sockets
                .lock()
                .unwrap()
                .iter()
                .find(|(address, _)| *address == destination)
                .map(|(_, events)| events.clone());
            if let Some(events) = events {
                events.post_received(packet).unwrap();
                self.link.delivered.fetch_add(1, Ordering::SeqCst);
            }
        }
        // Only the caller advances time. Queued receive/sent events run on the
        // next explicit turn, never on a protocol or product callback stack.
        self.world.advance(Duration::from_millis(1)).unwrap();
    }
    fn completed(
        &self,
        peer: &PeerConnection,
        id: OperationId,
        events: &mut Vec<PeerConnectionEvent>,
    ) -> Option<SessionDescription> {
        for _ in 0..10_000 {
            while let Some(event) = peer.try_next_event() {
                if let PeerConnectionEvent::OperationComplete(done) = &event
                    && done.operation_id == id
                {
                    return done.result.clone().unwrap();
                }
                events.push(event);
            }
            self.step();
        }
        panic!(
            "selected socket transport operation {} did not complete",
            id.get()
        );
    }
    fn gathered(
        &self,
        peer: &PeerConnection,
        events: &mut Vec<PeerConnectionEvent>,
        ip: Ipv4Addr,
    ) -> SessionDescription {
        for _ in 0..10_000 {
            events.extend(std::iter::from_fn(|| peer.try_next_event()));
            if events.iter().any(|event| {
                matches!(
                    event,
                    PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
                )
            }) {
                let descriptions = peer.descriptions().unwrap();
                let description = descriptions
                    .pending_local
                    .or(descriptions.current_local)
                    .unwrap();
                let candidates: Vec<_> = description
                    .sdp
                    .lines()
                    .filter(|line| line.starts_with("a=candidate:"))
                    .collect();
                assert!(
                    !candidates.is_empty(),
                    "custom factory produced no native host candidate"
                );
                assert!(
                    candidates.iter().all(|line| line.contains(&ip.to_string())),
                    "native candidates bypassed the selected interface: {candidates:?}"
                );
                return description;
            }
            self.step();
        }
        panic!("ICE did not finish gathering from the selected manager/socket factory");
    }
}

#[test]
fn selected_general_factory_carries_native_data_channel_traffic_and_retires_products() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let certificate_a = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let certificate_b = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let harness = Harness::new();
    let alice_ip = Ipv4Addr::new(10, 84, 0, 1);
    let bob_ip = Ipv4Addr::new(10, 84, 0, 2);
    let (factory_a, sockets_a, managers_a) = harness.factory(alice_ip);
    let (factory_b, sockets_b, managers_b) = harness.factory(bob_ip);
    let configuration = |certificate: Certificate| {
        let mut config = RtcConfiguration::default();
        config.options.certificates.push(certificate);
        config.options.tcp_candidate_policy = TcpCandidatePolicy::Disabled;
        config
    };
    let mut alice = factory_a
        .create_peer_connection_with_configuration(configuration(certificate_a))
        .unwrap();
    let mut bob = factory_b
        .create_peer_connection_with_configuration(configuration(certificate_b))
        .unwrap();
    let mut alice_channel = alice
        .create_data_channel(
            "selected-general-sockets",
            DataChannelConfiguration::default(),
        )
        .unwrap();
    let mut alice_events = Vec::new();
    let mut bob_events = Vec::new();
    let offer = harness
        .completed(&alice, alice.create_offer().unwrap(), &mut alice_events)
        .unwrap();
    harness.completed(
        &alice,
        alice.set_local_description(offer).unwrap(),
        &mut alice_events,
    );
    let offer = harness.gathered(&alice, &mut alice_events, alice_ip);
    harness.completed(
        &bob,
        bob.set_remote_description(offer).unwrap(),
        &mut bob_events,
    );
    let answer = harness
        .completed(&bob, bob.create_answer().unwrap(), &mut bob_events)
        .unwrap();
    harness.completed(
        &bob,
        bob.set_local_description(answer).unwrap(),
        &mut bob_events,
    );
    let answer = harness.gathered(&bob, &mut bob_events, bob_ip);
    harness.completed(
        &alice,
        alice.set_remote_description(answer).unwrap(),
        &mut alice_events,
    );
    let mut bob_channel = None;
    let mut alice_connected = false;
    let mut bob_connected = false;
    // Include events collected during negotiation, not only future callbacks.
    for event in alice_events.drain(..) {
        alice_connected |= matches!(
            event,
            PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected)
        );
    }
    for event in bob_events.drain(..) {
        match event {
            PeerConnectionEvent::DataChannel(channel) => bob_channel = Some(channel),
            PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected) => {
                bob_connected = true
            }
            _ => {}
        }
    }
    for _ in 0..10_000 {
        while let Some(event) = alice.try_next_event() {
            alice_connected |= matches!(
                event,
                PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected)
            );
        }
        while let Some(event) = bob.try_next_event() {
            match event {
                PeerConnectionEvent::DataChannel(channel) => {
                    assert!(bob_channel.replace(channel).is_none())
                }
                PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected) => {
                    bob_connected = true
                }
                _ => {}
            }
        }
        if alice_connected
            && bob_connected
            && alice_channel.state() == DataChannelState::Open
            && bob_channel
                .as_ref()
                .is_some_and(|channel| channel.state() == DataChannelState::Open)
        {
            break;
        }
        harness.step();
    }
    assert!(
        alice_connected && bob_connected,
        "real SDK ICE/DTLS did not connect"
    );
    let mut bob_channel = bob_channel.expect("real SCTP did not announce a remote data channel");
    assert_eq!(alice_channel.state(), DataChannelState::Open);
    assert_eq!(bob_channel.state(), DataChannelState::Open);
    let from_alice = DataChannelMessage::binary([0, 255, 17, 0]);
    let from_bob = DataChannelMessage::text("native SDK reply");
    assert_eq!(
        alice_channel.send(from_alice.clone()),
        DataChannelSendResult::Sent
    );
    assert_eq!(
        bob_channel.send(from_bob.clone()),
        DataChannelSendResult::Sent
    );
    let mut received_a = Vec::new();
    let mut received_b = Vec::new();
    for _ in 0..10_000 {
        harness.step();
        while let Some(event) = alice_channel.try_next_event() {
            if let DataChannelEvent::Message(message) = event {
                received_a.push(message);
            }
        }
        while let Some(event) = bob_channel.try_next_event() {
            if let DataChannelEvent::Message(message) = event {
                received_b.push(message);
            }
        }
        if !received_a.is_empty() && !received_b.is_empty() {
            break;
        }
    }
    assert_eq!(received_a, vec![from_bob]);
    assert_eq!(received_b, vec![from_alice]);
    assert!(harness.link.delivered.load(Ordering::SeqCst) > 0);
    assert!(!harness.link.options.lock().unwrap().is_empty());
    let late_events: Vec<_> = harness
        .link
        .sockets
        .lock()
        .unwrap()
        .iter()
        .map(|(_, events)| events.clone())
        .collect();
    alice.close().unwrap();
    bob.close().unwrap();
    drop((alice_channel, bob_channel, alice, bob, factory_a, factory_b));
    assert_eq!(
        harness.link.created.load(Ordering::SeqCst),
        harness.link.dropped.load(Ordering::SeqCst)
    );
    assert_eq!(harness.link.manager_created.load(Ordering::SeqCst), 2);
    assert_eq!(harness.link.manager_dropped.load(Ordering::SeqCst), 2);
    assert_eq!(
        harness.link.starts.load(Ordering::SeqCst),
        harness.link.stops.load(Ordering::SeqCst)
    );
    assert!(harness.link.sockets.lock().unwrap().is_empty());
    assert!(
        late_events
            .iter()
            .all(|events| events.post_ready_to_send() == Err(PacketSocketEventError::Retired))
    );
    assert!(!sockets_a.has_failed() && !sockets_b.has_failed());
    assert!(!managers_a.has_failed() && !managers_b.has_failed());
}

#[derive(Default)]
struct EstimatorObserved {
    created: AtomicUsize,
    dropped: AtomicUsize,
    queried: AtomicUsize,
    routes: AtomicUsize,
    feedback: Mutex<Vec<TransportPacketsFeedback>>,
}
struct EstimatorFactory(Arc<EstimatorObserved>);
struct Estimator(Arc<EstimatorObserved>);
impl NetworkStateEstimatorFactoryProvider for EstimatorFactory {
    fn create(
        &mut self,
        trials: Option<FieldTrialsSnapshot>,
    ) -> Option<Box<dyn NetworkStateEstimator>> {
        let trials = trials.expect("GoogCC must provide its actual environment trials");
        assert_eq!(trials.lookup("Provider-Transport"), "Selected");
        assert_eq!(
            trials.lookup("WebRTC-Bwe-InjectedCongestionController"),
            "Enabled"
        );
        self.0.created.fetch_add(1, Ordering::SeqCst);
        Some(Box::new(Estimator(self.0.clone())))
    }
}
impl NetworkStateEstimator for Estimator {
    fn get_current_estimate(&mut self) -> Option<NetworkStateEstimate> {
        self.0.queried.fetch_add(1, Ordering::SeqCst);
        // Preserve native optional absence, not a made-up bandwidth estimate.
        None
    }
    fn on_transport_packets_feedback(&mut self, feedback: TransportPacketsFeedback) {
        self.0.feedback.lock().unwrap().push(feedback);
    }
    fn on_route_change(&mut self, _: NetworkRouteChange) {
        self.0.routes.fetch_add(1, Ordering::SeqCst);
    }
}
impl Drop for Estimator {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn upstream_normalizes_socket_arrivals_and_selected_estimator_consumes_real_feedback() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let certificate_a = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let certificate_b = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let harness = Harness::new();
    let observed = Arc::new(EstimatorObserved::default());
    let estimator = NetworkStateEstimatorFactory::from_provider(EstimatorFactory(observed.clone()));
    // RFC 8888 is explicitly disabled in the actual environment and SDP below.
    // Ordinary negotiated transport-wide feedback still reaches native GoogCC.
    let controller = NetworkControllerFactory::goog_cc(
        GoogCcFactoryConfig::new(false).network_state_estimator_factory(&estimator),
    );
    let alice_ip = Ipv4Addr::new(10, 84, 0, 1);
    let bob_ip = Ipv4Addr::new(10, 84, 0, 2);
    let (factory_a, sockets_a, managers_a) =
        harness.factory_with_controller(alice_ip, Some(&controller));
    let (factory_b, sockets_b, managers_b) =
        harness.factory_with_controller(bob_ip, Some(&controller));
    let configuration = |certificate: Certificate| {
        let mut config = RtcConfiguration::default();
        config.options.certificates.push(certificate);
        config.options.tcp_candidate_policy = TcpCandidatePolicy::Disabled;
        config
    };
    let mut alice = factory_a
        .create_peer_connection_with_configuration(configuration(certificate_a))
        .unwrap();
    let mut bob = factory_b
        .create_peer_connection_with_configuration(configuration(certificate_b))
        .unwrap();
    let mut source = factory_a.create_encoded_audio_source(1).unwrap();
    let track = factory_a
        .create_encoded_audio_track("selected-opus", &source)
        .unwrap();
    let transceiver = alice
        .add_audio_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let mono = alice
        .audio_sender_capabilities()
        .unwrap()
        .into_iter()
        .find(|codec| {
            codec.name().eq_ignore_ascii_case("opus")
                && codec
                    .parameters()
                    .iter()
                    .any(|parameter| parameter.key == "stereo" && parameter.value == "0")
        })
        .expect("mono Opus capability");
    transceiver.set_audio_codec_preferences(&[mono]).unwrap();
    let mut alice_events = Vec::new();
    let mut bob_events = Vec::new();
    let offer = harness
        .completed(&alice, alice.create_offer().unwrap(), &mut alice_events)
        .unwrap();
    assert!(
        offer.sdp.contains("transport-cc"),
        "media must negotiate real transport feedback"
    );
    assert!(
        !offer.sdp.contains("ack ccfb"),
        "RFC 8888 declaration must match actual negotiation"
    );
    harness.completed(
        &alice,
        alice.set_local_description(offer).unwrap(),
        &mut alice_events,
    );
    let offer = harness.gathered(&alice, &mut alice_events, alice_ip);
    harness.completed(
        &bob,
        bob.set_remote_description(offer).unwrap(),
        &mut bob_events,
    );
    let answer = harness
        .completed(&bob, bob.create_answer().unwrap(), &mut bob_events)
        .unwrap();
    assert!(answer.sdp.contains("transport-cc"));
    assert!(!answer.sdp.contains("ack ccfb"));
    harness.completed(
        &bob,
        bob.set_local_description(answer).unwrap(),
        &mut bob_events,
    );
    let answer = harness.gathered(&bob, &mut bob_events, bob_ip);
    harness.completed(
        &alice,
        alice.set_remote_description(answer).unwrap(),
        &mut alice_events,
    );
    let mut sinks = Vec::new();
    let mut connected_a = false;
    let mut connected_b = false;
    for _ in 0..10_000 {
        alice_events.extend(std::iter::from_fn(|| alice.try_next_event()));
        bob_events.extend(std::iter::from_fn(|| bob.try_next_event()));
        for event in alice_events.drain(..) {
            connected_a |= matches!(
                event,
                PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected)
            );
        }
        for event in bob_events.drain(..) {
            match event {
                PeerConnectionEvent::Track(transceiver) => {
                    sinks.push(transceiver.receiver().attach_audio_sink().unwrap())
                }
                PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected) => {
                    connected_b = true
                }
                _ => {}
            }
        }
        if connected_a && connected_b && !sinks.is_empty() {
            break;
        }
        harness.step();
    }
    assert!(connected_a && connected_b && !sinks.is_empty());
    let before = bob
        .call_admission_stats()
        .expect("negotiated native audio must create a Call");
    assert!(before.live);
    assert_eq!(
        before.clock_time_bounds,
        Some(harness.environment.clock_time_bounds())
    );
    assert_eq!(before.rtp_arrival_rejections, 0);
    harness.link.nonfinite_remaining.store(3, Ordering::SeqCst);
    let mut decoded_tone = false;
    for index in 0..300u32 {
        source
            .push_opus_at(
                &OpusInputFrame {
                    data: include_bytes!("fixtures/simulation-tone-440.opus").to_vec(),
                    rtp_timestamp: index * 960,
                    samples_per_channel: 960,
                },
                harness.world.now(),
            )
            .unwrap();
        for _ in 0..20 {
            harness.step();
            for sink in &sinks {
                if let Some(frame) = sink.try_next_received_frame() {
                    decoded_tone |= frame.frame.samples.iter().any(|sample| *sample != 0);
                }
            }
        }
        if index >= 50 && decoded_tone && !observed.feedback.lock().unwrap().is_empty() {
            break;
        }
    }
    assert!(
        decoded_tone,
        "finite RTP must reach the actual Opus decoder, not merely a Track event or PLC silence"
    );
    let after = bob.call_admission_stats().unwrap();
    assert_eq!(harness.link.nonfinite_remaining.load(Ordering::SeqCst), 0);
    assert_eq!(harness.link.nonfinite_submitted.load(Ordering::SeqCst), 3);
    // Pinned WebRtcVoiceReceiveChannel::OnPacketReceived replaces nonfinite
    // socket arrival metadata with env_.clock().CurrentTime() before invoking
    // Call::Receiver(). This transport test must not claim direct Call-guard
    // rejection coverage from the three socket-layer injections.
    assert_eq!(
        after.rtp_arrival_rejections - before.rtp_arrival_rejections,
        0,
        "the upstream voice channel must supply finite arrival time to Call"
    );
    assert_eq!(
        alice.call_admission_stats().unwrap().rtp_arrival_rejections,
        0
    );
    assert!(observed.created.load(Ordering::SeqCst) >= 1);
    assert!(observed.queried.load(Ordering::SeqCst) > 0);
    // RtpTransportControllerSend records the initial connected route without
    // notifying the network controller; only a later route change is forwarded.
    // Actual nonempty transport feedback below is the installed-consumer proof.
    assert!(
        observed
            .feedback
            .lock()
            .unwrap()
            .iter()
            .any(|feedback| !feedback.packet_feedbacks.is_empty()),
        "installed estimator must consume real feedback, not only construction/process calls"
    );
    assert!(!estimator.has_failed());
    assert_eq!(estimator.observed_failure(), None);
    alice.close().unwrap();
    bob.close().unwrap();
    assert_eq!(
        bob.call_admission_stats().unwrap().rtp_arrival_rejections,
        after.rtp_arrival_rejections
    );
    source.close().unwrap();
    drop((
        sinks,
        source,
        track,
        transceiver,
        alice,
        bob,
        factory_a,
        factory_b,
    ));
    assert_eq!(
        observed.created.load(Ordering::SeqCst),
        observed.dropped.load(Ordering::SeqCst)
    );
    assert_eq!(
        harness.link.created.load(Ordering::SeqCst),
        harness.link.dropped.load(Ordering::SeqCst)
    );
    assert!(!controller.has_failed() && !estimator.has_failed());
    assert!(!sockets_a.has_failed() && !sockets_b.has_failed());
    assert!(!managers_a.has_failed() && !managers_b.has_failed());
}
