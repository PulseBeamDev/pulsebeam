//! Safe-public-API qualification of the controlled encoded-input media path.
#[path = "support/isolated.rs"]
mod isolated;
#[path = "support/mixed_h264.rs"]
mod mixed_h264;

use pulsebeam_webrtc_sys::*;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

static CONTROLLED_TEST: Mutex<()> = Mutex::new(());

struct UnavailableVp8;
impl VideoDecoderFactory for UnavailableVp8 {
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        vec![VideoCodecFormat::new("VP8")]
    }
    fn query_support(
        &self,
        _: &VideoCodecFormat,
        _: bool,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        CodecSupport {
            supported: false,
            power_efficient: false,
        }
    }
    fn create(&self, _: &VideoCodecFormat) -> Result<Box<dyn VideoDecoder>, CodecError> {
        Err(CodecError::UnsupportedFormat)
    }
}

const OPUS: [&[u8]; 2] = [
    include_bytes!("fixtures/simulation-tone-440.opus"),
    include_bytes!("fixtures/simulation-tone-880.opus"),
];
const VP8: [&[u8]; 2] = [
    include_bytes!("fixtures/simulation-red.vp8"),
    include_bytes!("fixtures/simulation-blue.vp8"),
];

// Keep every logical SDP field and candidate attribute. Only credential and
// certificate values are excluded from replay, not codec/stream negotiation,
// addresses, ports, priorities, or the timing of candidate observations.
fn normalized_candidate(candidate: &str) -> String {
    let mut credential = false;
    candidate
        .split_whitespace()
        .map(|field| {
            if credential {
                credential = false;
                "[secret]"
            } else {
                credential = matches!(field, "ufrag" | "pwd");
                field
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalized_sdp(sdp: &str) -> String {
    sdp.lines()
        .map(|line| {
            if let Some((prefix, _)) = line.split_once(':')
                && matches!(prefix, "a=ice-ufrag" | "a=ice-pwd")
            {
                return format!("{prefix}:[secret]");
            }
            if let Some(fingerprint) = line.strip_prefix("a=fingerprint:") {
                let algorithm = fingerprint.split_whitespace().next().unwrap_or("");
                return format!("a=fingerprint:{algorithm} [secret]");
            }
            if let Some(candidate) = line.strip_prefix("a=candidate:") {
                return format!("a=candidate:{}", normalized_candidate(candidate));
            }
            line.to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Trace {
    events: Vec<String>,
    messages: Vec<(usize, DataChannelMessage)>,
    audio: Vec<ReceivedAudioFrame>,
    video: Vec<ReceivedVideoFrame>,
    encoded_video: Vec<(usize, EncodedReceivedVideoFrame)>,
    encoded_audio: Vec<(usize, EncodedAudioFrame)>,
}

#[derive(Default)]
struct WireCapture {
    extensions: Vec<(i32, String)>,
    streams: Vec<(String, u32)>,
    packets: Vec<Vec<u8>>,
}

#[derive(Default)]
struct DrivenMetronome {
    now_us: AtomicU64,
    requests: AtomicUsize,
    periods: AtomicUsize,
    pending: Mutex<Vec<(u64, MetronomeTick)>>,
}
struct DrivenProvider(Arc<DrivenMetronome>);
impl MetronomeProvider for DrivenProvider {
    fn tick_period(&self) -> TimeDelta {
        self.0.periods.fetch_add(1, Ordering::Relaxed);
        TimeDelta::from_millis(10).unwrap()
    }
    fn request_call_on_next_tick(&mut self, tick: MetronomeTick) {
        assert!(TaskQueueContext::current().unwrap().is_current());
        self.0.requests.fetch_add(1, Ordering::Relaxed);
        let deadline = self.0.now_us.load(Ordering::Relaxed) + 10_000;
        self.0.pending.lock().unwrap().push((deadline, tick));
    }
}
impl DrivenMetronome {
    fn drive(&self, time: Duration) {
        let now = time.as_micros() as u64;
        self.now_us.store(now, Ordering::Relaxed);
        let ready: Vec<_> = {
            let mut pending = self.pending.lock().unwrap();
            let (ready, remaining): (Vec<_>, Vec<_>) = pending
                .drain(..)
                .partition(|(deadline, _)| *deadline <= now);
            *pending = remaining;
            ready
        };
        for (_, tick) in ready {
            tick.signal().unwrap();
        }
    }
}

#[derive(Default)]
struct Observations {
    impaired: bool,
    metronomes: Vec<Arc<DrivenMetronome>>,
    link_sequence: u64,
    pending_packets: Vec<(Duration, u64, bool)>,
    retired: bool,
    gathered: [bool; 2],
    operations: Vec<(usize, OperationCompletion)>,
    stats: Vec<(usize, PeerStatsSnapshot)>,
    terminal_operations: Vec<(usize, OperationId)>,
    audio_ids: Vec<(usize, String)>,
    video_ids: Vec<(usize, String)>,
    audio: Vec<(usize, AudioSink)>,
    video: Vec<(usize, VideoSink)>,
    encoded_video_only: bool,
    sender_only_simulcast_answer: bool,
    capture_wire: bool,
    wire: WireCapture,
    encoded_video: Vec<(usize, EncodedVideoSink)>,
    transceivers: Vec<(usize, RtpTransceiver)>,
    channels: Vec<(usize, DataChannel)>,
    trace: Trace,
}

impl Observations {
    fn step(
        &mut self,
        world: &ControlledWorld,
        network: &ControlledSimulatedNetwork,
        peers: &[PeerConnection; 2],
        media: bool,
    ) {
        let now = world.now();
        for metronome in &self.metronomes {
            metronome.drive(now);
        }
        let pump = world.pump(512);
        assert!(pump.dispatched <= 512);
        assert_eq!(world.now(), now, "pumping advanced virtual time");
        for _ in 0..256 {
            let Some(packet) = network.next_packet() else {
                break;
            };
            if self.capture_wire && packet.kind == OutboundKind::Udp {
                self.wire.packets.push(packet.payload.clone());
            }
            if self.impaired && !self.retired {
                self.link_sequence += 1;
                let sequence = self.link_sequence;
                // Fixed-seed external fixture policy, not a library policy.
                let random = sequence.wrapping_mul(6364136223846793005).wrapping_add(159);
                if sequence % 23 == 0 {
                    network.drop_packet(packet.id).unwrap();
                    self.trace
                        .events
                        .push(format!("{}:link:{sequence}:drop", now.as_micros()));
                } else {
                    let deadline = now + Duration::from_millis(5 + (random >> 32) % 26);
                    self.pending_packets
                        .push((deadline, packet.id, sequence % 13 == 0));
                    self.trace.events.push(format!(
                        "{}:link:{sequence}:schedule:{}",
                        now.as_micros(),
                        deadline.as_micros()
                    ));
                }
                continue;
            }
            match network.deliver(packet.id) {
                Ok(()) => {}
                Err(NetworkError::DestinationUnavailable) if self.retired => {
                    // Close may emit RTCP BYE/transport shutdown traffic before
                    // tearing down its destination socket. This disposition
                    // is observable, but must not resurrect a retired decoder.
                    self.trace
                        .events
                        .push(format!("{}:network:retired-destination", now.as_micros()));
                }
                Err(error) => panic!("live network delivery failed: {error:?}"),
            }
        }
        self.pending_packets
            .sort_by_key(|(deadline, id, _)| (*deadline, std::cmp::Reverse(*id)));
        while self
            .pending_packets
            .first()
            .is_some_and(|(deadline, _, _)| *deadline <= now)
        {
            let (_, id, duplicate) = self.pending_packets.remove(0);
            if self.retired {
                network.drop_packet(id).unwrap();
            } else {
                if duplicate {
                    network.deliver_copy(id).unwrap();
                    self.trace
                        .events
                        .push(format!("{}:link:duplicate", now.as_micros()));
                }
                network.deliver(id).unwrap();
            }
        }
        for (who, peer) in peers.iter().enumerate() {
            while let Some(event) = peer.try_next_event() {
                let description = match event {
                    PeerConnectionEvent::Stats(snapshot) => {
                        let description = format!("stats:{:?}", snapshot.operation_id);
                        self.stats.push((who, snapshot));
                        description
                    }
                    PeerConnectionEvent::OperationComplete(result) => {
                        let status = match &result.result {
                            Ok(Some(description)) => format!(
                                "ok:{:?}:{}",
                                description.kind,
                                normalized_sdp(&description.sdp)
                            ),
                            Ok(None) => "ok:none".to_owned(),
                            Err(error) => format!("error:{:?}:{}", error.kind, error.message),
                        };
                        let description = format!("operation:{:?}:{status}", result.operation_id);
                        assert!(
                            !self
                                .terminal_operations
                                .contains(&(who, result.operation_id)),
                            "duplicate terminal operation"
                        );
                        self.terminal_operations.push((who, result.operation_id));
                        self.operations.push((who, result));
                        description
                    }
                    PeerConnectionEvent::IceGatheringStateChanged(state) => {
                        self.gathered[who] = state == IceGatheringState::Complete;
                        format!("gathering:{state:?}")
                    }
                    PeerConnectionEvent::ConnectionStateChanged(state) => {
                        format!("connection:{state:?}")
                    }
                    PeerConnectionEvent::IceConnectionStateChanged(state) => {
                        format!("ice-connection:{state:?}")
                    }
                    PeerConnectionEvent::StandardizedIceConnectionStateChanged(state) => {
                        format!("standardized-ice-connection:{state:?}")
                    }
                    PeerConnectionEvent::SignalingStateChanged(state) => {
                        format!("signaling:{state:?}")
                    }
                    PeerConnectionEvent::IceConnectionStateChanged(state) => {
                        format!("ice-connection:{state:?}")
                    }
                    PeerConnectionEvent::StandardizedIceConnectionStateChanged(state) => {
                        format!("standardized-ice-connection:{state:?}")
                    }
                    PeerConnectionEvent::Track(transceiver) => {
                        let receiver = transceiver.receiver();
                        let id = receiver.id();
                        if let Some(track) = receiver.track() {
                            self.video_ids.push((who, track.id()));
                            if self.encoded_video_only {
                                assert_eq!(
                                    track.attach_sink().err().unwrap().kind,
                                    PeerErrorKind::UnsupportedOperation
                                );
                                self.encoded_video
                                    .push((who, receiver.attach_ordered_encoded_sink().unwrap()));
                            } else {
                                self.video.push((who, track.attach_sink().unwrap()));
                            }
                            self.transceivers.push((who, transceiver));
                        } else {
                            self.audio_ids.push((who, id.clone()));
                            self.audio
                                .push((who, receiver.attach_audio_sink().unwrap()));
                        }
                        format!("track:{id}")
                    }
                    PeerConnectionEvent::TrackAdded { receiver, streams } => {
                        let streams: Vec<_> = streams.iter().map(MediaStream::id).collect();
                        format!("track-added:{}:{streams:?}", receiver.id())
                    }
                    PeerConnectionEvent::TrackRemoved(receiver) => {
                        format!("removed:{}", receiver.id())
                    }
                    PeerConnectionEvent::StreamAdded(stream) => {
                        format!("stream-added:{:?}", stream.id())
                    }
                    PeerConnectionEvent::StreamRemoved(stream) => {
                        format!("stream-removed:{:?}", stream.id())
                    }
                    PeerConnectionEvent::IceConnectionReceivingChanged(value) => {
                        format!("ice-receiving:{value}")
                    }
                    PeerConnectionEvent::IceSelectedCandidatePairChanged(change) => {
                        format!(
                            "selected-pair:{:?}:{:?}",
                            change.transport_name, change.reason
                        )
                    }
                    PeerConnectionEvent::InterestingUsage { usage_pattern } => {
                        format!("usage:{usage_pattern}")
                    }
                    PeerConnectionEvent::DataChannelMessage(message) => {
                        format!(
                            "channel-message:{}:{:?}:{:?}:{:?}",
                            message.datachannel_id,
                            message.direction,
                            message.label,
                            message.message
                        )
                    }
                    PeerConnectionEvent::DataChannel(channel) => {
                        self.channels.push((who, channel));
                        "channel".into()
                    }
                    PeerConnectionEvent::Closed => "closed".into(),
                    PeerConnectionEvent::IceCandidate(candidate) => format!(
                        "candidate:{}:{}:{}",
                        candidate.sdp_mid,
                        candidate.sdp_mline_index,
                        normalized_candidate(&candidate.candidate)
                    ),
                    PeerConnectionEvent::IceCandidateRemoved(candidate) => format!(
                        "candidate-removed:{}:{}:{}",
                        candidate.sdp_mid,
                        candidate.sdp_mline_index,
                        normalized_candidate(&candidate.candidate)
                    ),
                    PeerConnectionEvent::NegotiationNeeded { event_id } => {
                        format!("negotiation-needed:{event_id}")
                    }
                    PeerConnectionEvent::RenegotiationNeeded => "renegotiation-needed".into(),
                    other => panic!("unexpected peer event: {other:?}"),
                };
                self.trace
                    .events
                    .push(format!("{}:{who}:{description}", now.as_micros()));
            }
        }
        for (who, channel) in &self.channels {
            while let Some(event) = channel.try_next_event() {
                if let DataChannelEvent::Message(message) = &event {
                    self.trace.messages.push((*who, message.clone()));
                }
                self.trace
                    .events
                    .push(format!("{}:{who}:data:{event:?}", now.as_micros()));
            }
        }
        if media {
            for (who, sink) in &self.encoded_video {
                while let Some(frame) = sink.try_next_frame() {
                    self.trace.encoded_video.push((*who, frame));
                }
            }
            for (_, sink) in &self.audio {
                if let Some(frame) = sink.try_next_received_frame() {
                    self.trace.audio.push(frame);
                }
            }
            for (_, sink) in &self.video {
                for _ in 0..4 {
                    let Some(frame) = sink.try_next_received_frame() else {
                        break;
                    };
                    self.trace.video.push(frame);
                }
            }
        }
    }

    fn completed(
        &mut self,
        world: &ControlledWorld,
        network: &ControlledSimulatedNetwork,
        peers: &[PeerConnection; 2],
        who: usize,
        operation: OperationId,
    ) -> Option<SessionDescription> {
        for _ in 0..2_000 {
            self.step(world, network, peers, false);
            if let Some(at) = self
                .operations
                .iter()
                .position(|(owner, result)| *owner == who && result.operation_id == operation)
            {
                return self.operations.remove(at).1.result.unwrap();
            }
            world.advance(Duration::from_millis(1)).unwrap();
        }
        panic!(
            "operation {operation:?} did not complete; trace={:?}",
            self.trace.events
        );
    }

    fn gather(
        &mut self,
        world: &ControlledWorld,
        network: &ControlledSimulatedNetwork,
        peers: &[PeerConnection; 2],
        who: usize,
    ) -> SessionDescription {
        for _ in 0..2_000 {
            self.step(world, network, peers, false);
            if self.gathered[who] {
                let descriptions = peers[who].descriptions().unwrap();
                let description = descriptions
                    .pending_local
                    .or(descriptions.current_local)
                    .unwrap();
                self.trace.events.push(format!(
                    "{}:{who}:gathered-sdp:{:?}:{}",
                    world.now().as_micros(),
                    description.kind,
                    normalized_sdp(&description.sdp)
                ));
                return description;
            }
            world.advance(Duration::from_millis(1)).unwrap();
        }
        panic!("gathering did not complete");
    }

    fn negotiate(
        &mut self,
        world: &ControlledWorld,
        network: &ControlledSimulatedNetwork,
        peers: &[PeerConnection; 2],
        offerer: usize,
    ) {
        self.negotiate_with_layers(world, network, peers, offerer, false);
    }

    fn negotiate_with_layers(
        &mut self,
        world: &ControlledWorld,
        network: &ControlledSimulatedNetwork,
        peers: &[PeerConnection; 2],
        offerer: usize,
        layered: bool,
    ) {
        fn enable(peer: &PeerConnection) {
            for transceiver in peer.video_transceivers().unwrap() {
                let mut extensions = transceiver.header_extensions_to_negotiate().unwrap();
                for uri in [
                    "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension",
                    "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00",
                ] {
                    extensions
                        .iter_mut()
                        .find(|extension| extension.uri() == uri)
                        .unwrap()
                        .direction = RtpHeaderExtensionDirection::SendReceive;
                }
                transceiver
                    .set_header_extensions_to_negotiate(&extensions)
                    .unwrap();
            }
        }
        if layered {
            enable(&peers[offerer]);
        }
        let answerer = 1 - offerer;
        let offer = self
            .completed(
                world,
                network,
                peers,
                offerer,
                peers[offerer].create_offer().unwrap(),
            )
            .unwrap();
        self.completed(
            world,
            network,
            peers,
            offerer,
            peers[offerer].set_local_description(offer).unwrap(),
        );
        let offer = self.gather(world, network, peers, offerer);
        self.completed(
            world,
            network,
            peers,
            answerer,
            peers[answerer].set_remote_description(offer).unwrap(),
        );
        if layered {
            enable(&peers[answerer]);
        }
        let answer = self
            .completed(
                world,
                network,
                peers,
                answerer,
                peers[answerer].create_answer().unwrap(),
            )
            .unwrap();
        self.completed(
            world,
            network,
            peers,
            answerer,
            peers[answerer].set_local_description(answer).unwrap(),
        );
        let mut answer = self.gather(world, network, peers, answerer);
        if self.sender_only_simulcast_answer {
            // External SFU acceptance fixture for sender evidence only. The
            // stock endpoint's local description is NOT changed and its
            // receiver does not claim simultaneous three-rung support.
            answer.sdp.push_str(
                "a=rid:q recv\r\na=rid:h recv\r\na=rid:f recv\r\na=simulcast:recv q;h;f\r\n",
            );
        }
        self.completed(
            world,
            network,
            peers,
            offerer,
            peers[offerer].set_remote_description(answer).unwrap(),
        );
    }
}

fn vp8_frame(world: &ControlledWorld, bytes: &[u8]) -> EncodedVideoAccessUnit {
    EncodedVideoAccessUnit {
        data: bytes.to_vec(),
        width: 32,
        height: 32,
        timestamp_us: world.now().as_micros() as i64,
        key_frame: true,
        qp: None,
        metadata: EncodedVideoMetadata {
            codec: EncodedVideoCodec::Vp8 {
                non_reference: false,
                layer_sync: false,
                key_index: None,
            },
            temporal_index: None,
            simulcast_index: None,
            spatial_index: None,
            end_of_picture: true,
        },
    }
}

fn input_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(14695981039346656037, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(1099511628211)
    })
}

// Fixture-only producer wrapper: every normal lifecycle submission explicitly
// declares metadata. Assertions inspect actual native receive output below.
struct DeclaredOpusSource {
    source: EncodedAudioSource,
    level: OpusAudioLevel,
}
impl std::ops::Deref for DeclaredOpusSource {
    type Target = EncodedAudioSource;
    fn deref(&self) -> &Self::Target {
        &self.source
    }
}
impl DeclaredOpusSource {
    fn push_opus_at(&self, frame: &OpusInputFrame, time: Duration) -> Result<(), OpusInputError> {
        self.source
            .push_opus_at_with_audio_level(frame, time, self.level)
    }
}

#[derive(Default)]
struct InstalledLog {
    active_outputs: AtomicUsize,
    headers: Arc<Mutex<Vec<Vec<u8>>>>,
    incoming: Mutex<Option<RtcEvent>>,
    outgoing: Mutex<Option<RtcEvent>>,
    configs: Mutex<Vec<RtcEventStreamConfig>>,
    creations: AtomicUsize,
    drops: AtomicUsize,
    stops: AtomicUsize,
}
struct InstalledLogFactory(Arc<InstalledLog>);
struct InstalledLogger {
    audit: Arc<InstalledLog>,
    output: Mutex<Option<EventLogOutput>>,
}
struct InstalledOutput(Arc<Mutex<Vec<Vec<u8>>>>);
impl RtcEventLogFactory for InstalledLogFactory {
    fn create(&self, environment: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        assert!(environment.timestamp().microseconds().unwrap() >= 10_000_000);
        self.0.creations.fetch_add(1, Ordering::SeqCst);
        Box::new(InstalledLogger {
            audit: self.0.clone(),
            output: Mutex::new(None),
        })
    }
}
impl EventLogOutputSink for InstalledOutput {
    fn is_active(&self) -> bool {
        true
    }
    fn write(&mut self, bytes: &[u8]) -> bool {
        self.0.lock().unwrap().push(bytes.to_vec());
        true
    }
}
impl RtcEventLogger for InstalledLogger {
    fn start(&self, output: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        let mut selected = self.output.lock().unwrap();
        if selected.is_some() {
            return false;
        }
        *selected = Some(output);
        self.audit.active_outputs.fetch_add(1, Ordering::SeqCst);
        true
    }
    fn stop(&self) {
        if let Some(mut output) = self.output.lock().unwrap().take() {
            output.flush();
            self.audit.active_outputs.fetch_sub(1, Ordering::SeqCst);
        }
        self.audit.stops.fetch_add(1, Ordering::SeqCst);
    }
    fn log(&self, event: RtcEvent) {
        let (packet, slot) = match event.data() {
            RtcEventData::AudioSendStreamConfig(config)
            | RtcEventData::AudioReceiveStreamConfig(config) => {
                self.audit.configs.lock().unwrap().push(config);
                return;
            }
            RtcEventData::RtpPacketIncoming(packet) => (packet, &self.audit.incoming),
            RtcEventData::RtpPacketOutgoing { packet, .. } => (packet, &self.audit.outgoing),
            _ => return,
        };
        if event
            .rtp_extension(RtcEventRtpExtensionKind::AudioLevel)
            .is_none()
        {
            return;
        }
        let mut retained = slot.lock().unwrap();
        if retained.is_none() {
            // This caller log writes the actual native event's header bytes,
            // not a fabricated packet or the stock logger's serialization.
            if let Some(output) = self.output.lock().unwrap().as_mut() {
                assert!(output.write(&packet.raw_header));
            }
            *retained = Some(event);
        }
    }
}
impl Drop for InstalledLogger {
    fn drop(&mut self) {
        self.stop();
        self.audit.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn run_media(addresses: [IpAddr; 2], impaired: bool) -> Trace {
    run_media_with_metronomes(addresses, impaired, None)
}
fn run_media_with_metronomes(
    addresses: [IpAddr; 2],
    impaired: bool,
    metronomes: Option<&[Arc<DrivenMetronome>; 2]>,
) -> Trace {
    run_media_with_logs(addresses, impaired, metronomes, None)
}
fn run_media_with_logs(
    addresses: [IpAddr; 2],
    impaired: bool,
    metronomes: Option<&[Arc<DrivenMetronome>; 2]>,
    logs: Option<&[Arc<InstalledLog>; 2]>,
) -> Trace {
    eprintln!("CONTROLLED_MEDIA_BEGIN");
    let trace = {
        let world = ControlledWorld::acquire(731, Duration::from_secs(10)).unwrap();
        let network = world.create_network().unwrap();
        if let Some(metronomes) = metronomes {
            for metronome in metronomes {
                metronome.drive(world.now());
            }
        }
        let input = EncodedVideoInput::new_for_format(VideoCodecFormat::new("VP8")).unwrap();
        let audio_decoders = [
            AudioDecoderFactory::builtin_opus().unwrap(),
            AudioDecoderFactory::builtin_opus().unwrap(),
        ];
        let audio_encoders = [
            AudioEncoderFactory::with_opus_frames().unwrap(),
            AudioEncoderFactory::with_opus_frames().unwrap(),
        ];
        let video_decoders = [
            VideoDecoderFactoryHandle::builtin_vp8().unwrap(),
            VideoDecoderFactoryHandle::builtin_vp8().unwrap(),
        ];
        // Endpoint drop is an explicit link shutdown. Keep the endpoint
        // owners alive, not just the provider recipes built from them.
        let endpoints: Vec<_> = addresses
            .into_iter()
            .map(|address| network.register_endpoint(address).unwrap())
            .collect();
        let factories: Vec<_> = (0..2)
            .map(|who| {
                let endpoint = &endpoints[who];
                let mut builder = world.peer_factory_builder().unwrap();
                if let Some(logs) = logs {
                    builder = builder.event_log_factory(InstalledLogFactory(logs[who].clone()));
                }
                if let Some(metronomes) = metronomes {
                    builder = builder
                        .encode_metronome_provider(DrivenProvider(metronomes[0].clone()))
                        .decode_metronome_provider(DrivenProvider(metronomes[1].clone()));
                }
                builder
                    .network_manager(endpoint.network_manager().unwrap())
                    .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
                    .audio_encoder_factory(audio_encoders[who].clone())
                    .audio_decoder_factory(audio_decoders[who].clone())
                    .video_encoder_factory(input.encoder_factory())
                    .video_decoder_factory(video_decoders[who].clone())
                    .build()
                    .unwrap()
            })
            .collect();
        let mut peers = [
            factories[0]
                .create_peer_connection(PeerConfiguration::default())
                .unwrap(),
            factories[1]
                .create_peer_connection(PeerConfiguration::default())
                .unwrap(),
        ];
        if let Some(logs) = logs {
            for (peer, log) in peers.iter().zip(logs) {
                peer.start_event_log(
                    EventLogOutput::from_sink(InstalledOutput(log.headers.clone())),
                    Some(EventLogOutputPeriod::Immediate),
                )
                .unwrap();
            }
        }
        let audio_sources: Vec<_> = factories
            .iter()
            .enumerate()
            .map(|(who, factory)| DeclaredOpusSource {
                source: factory.create_encoded_audio_source(1).unwrap(),
                level: OpusAudioLevel::new(if who == 0 { 127 } else { 0 }, who == 1).unwrap(),
            })
            .collect();
        let mut video_sources: Vec<_> = factories
            .iter()
            .map(|factory| input.create_source(factory).unwrap())
            .collect();
        let mut tracks = Vec::new();
        let mut audio_tracks = Vec::new();
        let mut transceivers = Vec::new();
        for who in 0..2 {
            let track = video_sources[who]
                .create_track(&factories[who], &format!("video-{who}"))
                .unwrap();
            let audio_track = factories[who]
                .create_encoded_audio_track(&format!("audio-{who}"), &audio_sources[who])
                .unwrap();
            transceivers.push(
                peers[who]
                    .add_video_transceiver(&track, RtpTransceiverDirection::SendReceive)
                    .unwrap(),
            );
            let audio_transceiver = peers[who]
                .add_audio_transceiver(&audio_track, RtpTransceiverDirection::SendReceive)
                .unwrap();
            let mono = peers[who]
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
            audio_transceiver
                .set_audio_codec_preferences(&[mono])
                .unwrap();
            transceivers.push(audio_transceiver);
            tracks.push(track);
            audio_tracks.push(audio_track);
        }
        let outgoing = peers[0]
            .create_data_channel("media-control", DataChannelConfiguration::default())
            .unwrap();
        let mut observations = Observations {
            impaired,
            metronomes: metronomes.map_or_else(Vec::new, |sources| sources.to_vec()),
            ..Observations::default()
        };
        let offer = observations
            .completed(
                &world,
                &network,
                &peers,
                0,
                peers[0].create_offer().unwrap(),
            )
            .unwrap();
        observations.completed(
            &world,
            &network,
            &peers,
            0,
            peers[0].set_local_description(offer).unwrap(),
        );
        let offer = observations.gather(&world, &network, &peers, 0);
        observations.completed(
            &world,
            &network,
            &peers,
            1,
            peers[1].set_remote_description(offer).unwrap(),
        );
        let answer = observations
            .completed(
                &world,
                &network,
                &peers,
                1,
                peers[1].create_answer().unwrap(),
            )
            .unwrap();
        observations.completed(
            &world,
            &network,
            &peers,
            1,
            peers[1].set_local_description(answer).unwrap(),
        );
        let answer = observations.gather(&world, &network, &peers, 1);
        observations.completed(
            &world,
            &network,
            &peers,
            0,
            peers[0].set_remote_description(answer).unwrap(),
        );
        // Explicit AddTransceiver entries on the answerer are not implicitly
        // reused for remote offer m-lines. Negotiate its two senders in a second
        // gathered-SDP exchange rather than feeding unnegotiated sources.
        let reverse_offer = observations
            .completed(
                &world,
                &network,
                &peers,
                1,
                peers[1].create_offer().unwrap(),
            )
            .unwrap();
        observations.completed(
            &world,
            &network,
            &peers,
            1,
            peers[1].set_local_description(reverse_offer).unwrap(),
        );
        let reverse_offer = observations.gather(&world, &network, &peers, 1);
        observations.completed(
            &world,
            &network,
            &peers,
            0,
            peers[0].set_remote_description(reverse_offer).unwrap(),
        );
        let reverse_answer = observations
            .completed(
                &world,
                &network,
                &peers,
                0,
                peers[0].create_answer().unwrap(),
            )
            .unwrap();
        observations.completed(
            &world,
            &network,
            &peers,
            0,
            peers[0].set_local_description(reverse_answer).unwrap(),
        );
        let reverse_answer = observations.gather(&world, &network, &peers, 0);
        observations.completed(
            &world,
            &network,
            &peers,
            1,
            peers[1].set_remote_description(reverse_answer).unwrap(),
        );
        for _ in 0..2_000 {
            observations.step(&world, &network, &peers, false);
            if outgoing.state() == DataChannelState::Open
                && observations
                    .channels
                    .iter()
                    .any(|(_, channel)| channel.state() == DataChannelState::Open)
            {
                break;
            }
            world.advance(Duration::from_millis(1)).unwrap();
        }
        assert_eq!(
            outgoing.state(),
            DataChannelState::Open,
            "connection trace: {:?}",
            observations.trace.events
        );
        assert_eq!(
            outgoing.send(DataChannelMessage::text("forward")),
            DataChannelSendResult::Sent
        );
        let remote = &observations.channels[0].1;
        assert_eq!(
            remote.send(DataChannelMessage::text("reverse")),
            DataChannelSendResult::Sent
        );
        observations.channels.push((0, outgoing));
        let before_invalid = audio_decoders[0].decoder_statistics();
        assert_eq!(
            audio_sources[0].push_opus_at(
                &OpusInputFrame {
                    data: Vec::new(),
                    rtp_timestamp: 0,
                    samples_per_channel: 960
                },
                world.now()
            ),
            Err(OpusInputError::InvalidPacket)
        );
        assert_eq!(
            audio_sources[0].push_opus_at(
                &OpusInputFrame {
                    data: OPUS[0].to_vec(),
                    rtp_timestamp: 0,
                    samples_per_channel: 480
                },
                world.now()
            ),
            Err(OpusInputError::InvalidDuration)
        );
        assert_eq!(
            video_sources[0].push_encoded(vp8_frame(&world, &[0, 0, 0])),
            Err(CodecError::InvalidFrame)
        );
        assert_eq!(audio_decoders[0].decoder_statistics(), before_invalid);
        assert_eq!(world.pump(0).dispatched, 0);
        // Merely advancing the clock does not execute queued engine work.
        world.advance(Duration::from_millis(1)).unwrap();
        assert_eq!(audio_decoders[0].decoder_statistics(), before_invalid);

        // A third, unnegotiated sender exercises bounded Opus reservations and
        // drop with pending offer/media without consuming the observed pair's
        // media. No pump is needed to complete its destruction.
        {
            let abandoned = factories[0]
                .create_peer_connection(PeerConfiguration::default())
                .unwrap();
            let mut buffered_audio = factories[0].create_encoded_audio_source(1).unwrap();
            let buffered_track = factories[0]
                .create_encoded_audio_track("buffered-audio", &buffered_audio)
                .unwrap();
            let buffered_transceiver = abandoned
                .add_audio_transceiver(&buffered_track, RtpTransceiverDirection::SendOnly)
                .unwrap();
            for packet in 0..12 {
                buffered_audio
                    .push_opus_at(
                        &OpusInputFrame {
                            data: OPUS[0].to_vec(),
                            rtp_timestamp: packet * 960,
                            samples_per_channel: 960,
                        },
                        world.now(),
                    )
                    .unwrap();
            }
            assert_eq!(
                buffered_audio.push_opus_at(
                    &OpusInputFrame {
                        data: OPUS[0].to_vec(),
                        rtp_timestamp: 12 * 960,
                        samples_per_channel: 960
                    },
                    world.now()
                ),
                Err(OpusInputError::Backpressure)
            );
            abandoned.create_offer().unwrap();
            buffered_audio.close().unwrap();
            drop(buffered_transceiver);
            drop(abandoned);
        }
        // Video overflow uses an isolated unsent source; counted eviction must
        // not affect either active producer sharing the same input broker.
        {
            let mut buffered_video = input.create_source(&factories[0]).unwrap();
            for offset in 0..18 {
                let mut frame = vp8_frame(&world, VP8[0]);
                frame.timestamp_us += offset;
                buffered_video.push_encoded(frame).unwrap();
            }
            assert_eq!(buffered_video.pending_frames(), 16);
            assert_eq!(buffered_video.dropped_frames(), 2);
            buffered_video.close().unwrap();
            assert_eq!(buffered_video.pending_frames(), 0);
        }
        for tick in 0..80 {
            for who in 0..2 {
                if tick % 2 == 0 {
                    let packet = OpusInputFrame {
                        data: OPUS[who].to_vec(),
                        rtp_timestamp: 480_000 + tick * 480,
                        samples_per_channel: 960,
                    };
                    audio_sources[who]
                        .push_opus_at(&packet, world.now())
                        .unwrap_or_else(|error| {
                            panic!(
                                "Opus push {who} tick {tick}: {error:?}; decoder={:?}; events={:?}",
                                audio_decoders[1 - who].decoder_statistics(),
                                observations.trace.events
                            )
                        });
                }
                if tick % 10 == 0 {
                    video_sources[who]
                        .push_encoded(EncodedVideoAccessUnit {
                            data: VP8[who].to_vec(),
                            width: 32,
                            height: 32,
                            timestamp_us: world.now().as_micros() as i64,
                            key_frame: true,
                            qp: None,
                            metadata: EncodedVideoMetadata {
                                codec: EncodedVideoCodec::Vp8 {
                                    non_reference: false,
                                    layer_sync: false,
                                    key_index: None,
                                },
                                temporal_index: None,
                                simulcast_index: None,
                                spatial_index: None,
                                end_of_picture: true,
                            },
                        })
                        .unwrap();
                }
            }
            observations.step(&world, &network, &peers, true);
            world.advance(Duration::from_millis(10)).unwrap();
        }
        let caller = DecoderStatistics::current_thread_token();
        for who in 0..2 {
            for statistics in [
                audio_decoders[who].decoder_statistics().unwrap(),
                video_decoders[who].decoder_statistics().unwrap(),
            ] {
                assert!(
                    statistics.decoder_creations > 0
                        && statistics.decode_calls > 0
                        && statistics.decoded_outputs > 0,
                    "no real decoding: {statistics:?}"
                );
                assert_eq!(
                    statistics.thread_token, caller,
                    "native decoder/output ran on another thread"
                );
                assert_eq!(statistics.thread_mismatches, 0);
            }
            // Native decoder inputs must equal the prerecorded compressed
            // bytes, independently proving that compression was bypassed.
            assert_eq!(
                audio_decoders[who]
                    .decoder_statistics()
                    .unwrap()
                    .last_input_hash,
                input_hash(OPUS[1 - who])
            );
            assert_eq!(
                video_decoders[who]
                    .decoder_statistics()
                    .unwrap()
                    .last_input_hash,
                input_hash(VP8[1 - who])
            );
            let recipient = peers[who].controlled_id();
            assert!(
                observations
                    .trace
                    .audio
                    .iter()
                    .any(|frame| frame.peer_id == recipient
                        && observations
                            .audio_ids
                            .contains(&(who, frame.receiver_id.clone()))
                        && frame
                            .frame
                            .samples
                            .iter()
                            .any(|sample| sample.unsigned_abs() > 100)),
                "missing recognizable PCM for peer {who}"
            );
            let blocks: Vec<_> = observations
                .trace
                .audio
                .iter()
                .filter(|frame| frame.peer_id == recipient)
                .collect();
            let samples: Vec<_> = blocks
                .iter()
                .flat_map(|block| {
                    block
                        .frame
                        .samples
                        .iter()
                        .step_by(usize::from(block.frame.channels))
                })
                .copied()
                .collect();
            let crossings = samples
                .windows(2)
                .filter(|pair| pair[0] <= 0 && pair[1] > 0)
                .count();
            let frequency =
                crossings as f64 * f64::from(blocks[0].frame.sample_rate_hz) / samples.len() as f64;
            let band = if who == 0 {
                700.0..1200.0
            } else {
                300.0..650.0
            };
            assert!(
                band.contains(&frequency),
                "unexpected received tone band for peer {who}: {frequency}"
            );
            assert!(
                observations.trace.video.iter().any(|frame| {
                    if frame.peer_id != recipient
                        || !observations
                            .video_ids
                            .contains(&(who, frame.track_id.clone()))
                    {
                        return false;
                    }
                    let pixels = frame.frame.buffer.as_bytes();
                    frame.frame.width == 32
                        && frame.frame.height == 32
                        && pixels.len() == 1536
                        && if who == 0 {
                            pixels[1024] > 200 && pixels[1280] < 150
                        } else {
                            pixels[1024] < 150 && pixels[1280] > 200
                        }
                }),
                "missing recognizable pixels for peer {who}"
            );
        }
        assert!(
            observations
                .trace
                .messages
                .iter()
                .any(|(who, message)| *who == 1
                    && message.kind == DataChannelMessageKind::Text
                    && message.bytes == b"forward")
        );
        assert!(
            observations
                .trace
                .messages
                .iter()
                .any(|(who, message)| *who == 0
                    && message.kind == DataChannelMessageKind::Text
                    && message.bytes == b"reverse")
        );
        // Replace only the first video producer on the negotiated sender.
        // Its receiver identity remains stable until subsequent renegotiation.
        let saved_video = observations.trace.video[0].clone();
        let saved_audio = observations.trace.audio[0].clone();
        let mut replacement = input.create_source(&factories[0]).unwrap();
        let replacement_track = replacement
            .create_track(&factories[0], "video-0-replacement")
            .unwrap();
        let sender = transceivers[0].sender();
        let sender_id = sender.id();
        sender.set_track(Some(&replacement_track)).unwrap();
        assert_eq!(sender.id(), sender_id);
        video_sources[0].close().unwrap();
        assert_eq!(
            video_sources[0].push_encoded(vp8_frame(&world, VP8[0])),
            Err(CodecError::Released)
        );
        sender.request_keyframe(&[]).unwrap();
        let replacement_begin = observations.trace.video.len();
        let audio_begin = observations.trace.audio.len();
        let mut keyframe_seen = false;
        for tick in 0..50 {
            if tick % 2 == 0 {
                for who in 0..2 {
                    audio_sources[who]
                        .push_opus_at(
                            &OpusInputFrame {
                                data: OPUS[who].to_vec(),
                                rtp_timestamp: (world.now().as_micros() * 48 / 1000) as u32,
                                samples_per_channel: 960,
                            },
                            world.now(),
                        )
                        .unwrap();
                }
            }
            if tick % 10 == 0 {
                replacement.push_encoded(vp8_frame(&world, VP8[1])).unwrap();
                video_sources[1]
                    .push_encoded(vp8_frame(&world, VP8[1]))
                    .unwrap();
            }
            observations.step(&world, &network, &peers, true);
            if replacement.take_keyframe_request() {
                keyframe_seen = true;
                observations.trace.events.push(format!(
                    "{}:keyframe:stream:{}",
                    world.now().as_micros(),
                    replacement.stream_id()
                ));
            }
            world.advance(Duration::from_millis(10)).unwrap();
        }
        assert!(
            keyframe_seen,
            "keyframe feedback missing on replacement source"
        );
        assert!(
            observations.trace.video[replacement_begin..]
                .iter()
                .any(|frame| frame.peer_id == peers[1].controlled_id()
                    && frame.frame.buffer.as_bytes()[1024] > 200)
        );
        for who in 0..2 {
            assert!(
                observations.trace.audio[audio_begin..]
                    .iter()
                    .any(|frame| frame.peer_id == peers[who].controlled_id()),
                "unrelated audio interrupted"
            );
        }
        assert!(
            observations.trace.video[replacement_begin..]
                .iter()
                .any(|frame| frame.peer_id == peers[0].controlled_id()),
            "unrelated video interrupted"
        );

        // Stop and negotiate away that receiver, then create a fresh receiver
        // for a new producer. Old owned output stays readable throughout.
        transceivers[0].stop().unwrap();
        replacement.close().unwrap();
        observations.negotiate(&world, &network, &peers, 0);
        let retired_at = world.now();
        let reborn = input.create_source(&factories[0]).unwrap();
        let reborn_track = reborn
            .create_track(&factories[0], "video-0-reborn")
            .unwrap();
        let reborn_transceiver = peers[0]
            .add_video_transceiver(&reborn_track, RtpTransceiverDirection::SendOnly)
            .unwrap();
        observations.negotiate(&world, &network, &peers, 0);
        let recreation_begin = observations.trace.video.len();
        for tick in 0..50 {
            if tick % 2 == 0 {
                for who in 0..2 {
                    audio_sources[who]
                        .push_opus_at(
                            &OpusInputFrame {
                                data: OPUS[who].to_vec(),
                                rtp_timestamp: (world.now().as_micros() * 48 / 1000) as u32,
                                samples_per_channel: 960,
                            },
                            world.now(),
                        )
                        .unwrap();
                }
            }
            if tick % 10 == 0 {
                reborn.push_encoded(vp8_frame(&world, VP8[0])).unwrap();
                video_sources[1]
                    .push_encoded(vp8_frame(&world, VP8[1]))
                    .unwrap();
            }
            observations.step(&world, &network, &peers, true);
            world.advance(Duration::from_millis(10)).unwrap();
        }
        assert!(
            observations.trace.video[recreation_begin..]
                .iter()
                .any(|frame| frame.peer_id == peers[1].controlled_id()
                    && frame.track_id == "video-0-reborn"
                    && frame.frame.buffer.as_bytes()[1280] > 200)
        );
        assert!(
            observations
                .trace
                .video
                .iter()
                .filter(|frame| frame.peer_id == peers[1].controlled_id()
                    && frame.track_id == "video-0")
                .all(|frame| frame.observed_at.unwrap() <= retired_at)
        );
        assert_eq!(saved_video, observations.trace.video[0]);
        assert_eq!(saved_audio, observations.trace.audio[0]);
        assert!(
            video_decoders[1]
                .decoder_statistics()
                .unwrap()
                .decoder_creations
                >= 2
        );
        assert!(!reborn_transceiver.stopped());

        // Exercise audio mode changes inside the controlled lifecycle, while
        // the other audio/video streams continue using their native paths.
        let slot = observations
            .audio
            .iter()
            .position(|(who, _)| *who == 1)
            .unwrap();
        let (_, mut old_decoded) = observations.audio.remove(slot);
        let receiver_id = observations
            .audio_ids
            .iter()
            .find(|(who, _)| *who == 1)
            .unwrap()
            .1
            .clone();
        let receiver = peers[1]
            .audio_receivers()
            .unwrap()
            .into_iter()
            .find(|receiver| receiver.id() == receiver_id)
            .unwrap();
        old_decoded.close().unwrap();
        let mut encoded = receiver.attach_encoded_audio_sink().unwrap();
        drop(old_decoded);
        assert_eq!(
            receiver.attach_audio_sink().err().unwrap().kind,
            PeerErrorKind::InvalidState
        );
        observations.trace.events.push(format!(
            "{}:audio-mode:{receiver_id}:encoded",
            world.now().as_micros()
        ));
        let before = audio_decoders[1]
            .decoder_statistics()
            .unwrap()
            .input_packets;
        let encoded_begin = observations.trace.encoded_audio.len();
        let other_audio_begin = observations.trace.audio.len();
        let other_video_begin = observations.trace.video.len();
        for tick in 0..40 {
            if tick % 2 == 0 {
                for who in 0..2 {
                    audio_sources[who]
                        .push_opus_at(
                            &OpusInputFrame {
                                data: OPUS[who].to_vec(),
                                rtp_timestamp: (world.now().as_micros() * 48 / 1000) as u32,
                                samples_per_channel: 960,
                            },
                            world.now(),
                        )
                        .unwrap();
                }
            }
            if tick % 10 == 0 {
                reborn.push_encoded(vp8_frame(&world, VP8[0])).unwrap();
                video_sources[1]
                    .push_encoded(vp8_frame(&world, VP8[1]))
                    .unwrap();
            }
            observations.step(&world, &network, &peers, true);
            while let Some(frame) = encoded.try_next_frame() {
                assert_eq!(frame.data, OPUS[0]);
                assert!(frame.sequence_number.is_some());
                assert_eq!(frame.audio_level_dbov, Some(127));
                assert_eq!(frame.voice_activity, Some(false));
                observations.trace.encoded_audio.push((1, frame));
            }
            world.advance(Duration::from_millis(10)).unwrap();
        }
        assert!(observations.trace.encoded_audio.len() > encoded_begin);
        assert!(
            observations.trace.audio[other_audio_begin..]
                .iter()
                .any(|frame| frame.peer_id == peers[0].controlled_id())
        );
        for who in 0..2 {
            assert!(
                observations.trace.video[other_video_begin..]
                    .iter()
                    .any(|frame| frame.peer_id == peers[who].controlled_id()),
                "audio mode change interrupted unrelated video"
            );
        }
        assert_eq!(
            audio_decoders[1]
                .decoder_statistics()
                .unwrap()
                .input_packets,
            before,
            "encoded receive must not admit new packets to NetEq"
        );
        let saved_encoded = observations.trace.encoded_audio[encoded_begin].clone();
        encoded.close().unwrap();
        encoded.close().unwrap();
        assert!(encoded.try_next_frame().is_none());
        let restored = receiver.attach_audio_sink().unwrap();
        drop(encoded);
        assert_eq!(
            receiver.attach_encoded_audio_sink().err().unwrap().kind,
            PeerErrorKind::InvalidState
        );
        observations.audio.push((1, restored));
        observations.trace.events.push(format!(
            "{}:audio-mode:{receiver_id}:decoded",
            world.now().as_micros()
        ));
        let resume_begin = observations.trace.audio.len();
        for tick in 0..40 {
            if tick % 2 == 0 {
                for who in 0..2 {
                    audio_sources[who]
                        .push_opus_at(
                            &OpusInputFrame {
                                data: OPUS[who].to_vec(),
                                rtp_timestamp: (world.now().as_micros() * 48 / 1000) as u32,
                                samples_per_channel: 960,
                            },
                            world.now(),
                        )
                        .unwrap();
                }
            }
            if tick % 10 == 0 {
                reborn.push_encoded(vp8_frame(&world, VP8[0])).unwrap();
                video_sources[1]
                    .push_encoded(vp8_frame(&world, VP8[1]))
                    .unwrap();
            }
            observations.step(&world, &network, &peers, true);
            world.advance(Duration::from_millis(10)).unwrap();
        }
        assert!(
            audio_decoders[1]
                .decoder_statistics()
                .unwrap()
                .input_packets
                > before,
            "closed encoded transformer must restore native packet delivery"
        );
        for who in 0..2 {
            assert!(
                observations.trace.audio[resume_begin..]
                    .iter()
                    .any(|frame| frame.peer_id == peers[who].controlled_id())
            );
        }
        assert_eq!(
            saved_encoded,
            observations.trace.encoded_audio[encoded_begin]
        );

        // Close with accepted signaling, encoded input and network work still
        // queued. Every observed operation must terminate exactly once, without
        // decoding or emitting media after close.
        for who in 0..2 {
            audio_sources[who]
                .push_opus_at(
                    &OpusInputFrame {
                        data: vec![3 << 3, 0xff, 0xfe],
                        rtp_timestamp: (world.now().as_micros() * 48 / 1000) as u32,
                        samples_per_channel: 2880,
                    },
                    world.now(),
                )
                .unwrap();
        }
        reborn.push_encoded(vp8_frame(&world, VP8[0])).unwrap();
        let pending = [
            (0, peers[0].create_offer().unwrap()),
            (1, peers[1].create_offer().unwrap()),
        ];
        assert_eq!(
            peers[0]
                .set_local_description(peers[0].descriptions().unwrap().current_local.unwrap())
                .unwrap_err()
                .kind,
            PeerErrorKind::ResourceExhausted
        );
        if logs.is_some() {
            for peer in &peers {
                peer.stop_event_log().unwrap();
            }
        }
        peers[0].close().unwrap();
        peers[0].close().unwrap();
        peers[1].close().unwrap();
        observations.retired = true;
        for (_, id, _) in observations.pending_packets.drain(..) {
            network.drop_packet(id).unwrap();
        }
        let before = [
            audio_decoders[0].decoder_statistics(),
            audio_decoders[1].decoder_statistics(),
            video_decoders[0].decoder_statistics(),
            video_decoders[1].decoder_statistics(),
        ];
        for _ in 0..8 {
            observations.step(&world, &network, &peers, false);
        }
        assert_eq!(
            before,
            [
                audio_decoders[0].decoder_statistics(),
                audio_decoders[1].decoder_statistics(),
                video_decoders[0].decoder_statistics(),
                video_decoders[1].decoder_statistics()
            ]
        );
        for (who, operation) in pending {
            assert_eq!(
                observations
                    .terminal_operations
                    .iter()
                    .filter(|entry| **entry == (who, operation))
                    .count(),
                1
            );
        }
        assert!(
            observations
                .audio
                .iter()
                .all(|(_, sink)| sink.try_next_received_frame().is_none())
        );
        assert!(
            observations
                .video
                .iter()
                .all(|(_, sink)| sink.try_next_received_frame().is_none())
        );
        assert!(
            audio_encoders
                .iter()
                .all(|encoder| encoder.opus_frame_handoff_failures() == 0)
        );
        observations.trace
    };
    // All native roots have gone away; only passive owned outputs survive.
    eprintln!("CONTROLLED_MEDIA_END");
    trace
}

#[test]
fn selected_logs_receive_native_rtp_and_extension_values_at_owned_outputs() {
    let _guard = CONTROLLED_TEST
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let logs = [
        Arc::new(InstalledLog::default()),
        Arc::new(InstalledLog::default()),
    ];
    run_media_with_logs(
        [
            Ipv4Addr::new(10, 79, 0, 1).into(),
            Ipv4Addr::new(10, 79, 0, 2).into(),
        ],
        false,
        None,
        Some(&logs),
    );
    for (who, log) in logs.iter().enumerate() {
        // The fixture also retires one unnegotiated peer on the first factory.
        let creations = 1 + usize::from(who == 0);
        assert_eq!(log.creations.load(Ordering::SeqCst), creations);
        assert_eq!(log.drops.load(Ordering::SeqCst), creations);
        assert!(log.stops.load(Ordering::SeqCst) >= creations);
        assert_eq!(log.active_outputs.load(Ordering::SeqCst), 0);
        let configs = log.configs.lock().unwrap();
        assert!(
            configs.iter().any(
                |config| config.rtp_extensions.iter().any(|extension| extension.uri
                    == "urn:ietf:params:rtp-hdrext:ssrc-audio-level"
                    && (1..=255).contains(&extension.id))
            )
        );
        drop(configs);
        for (incoming, slot) in [(true, &log.incoming), (false, &log.outgoing)] {
            let event = slot
                .lock()
                .unwrap()
                .take()
                .expect("native RTP event was not delivered");
            let copy = event.try_clone().unwrap();
            drop(event);
            let packet = match copy.data() {
                RtcEventData::RtpPacketIncoming(packet) if incoming => packet,
                RtcEventData::RtpPacketOutgoing { packet, .. } if !incoming => packet,
                other => panic!("wrong native event: {other:?}"),
            };
            assert_eq!(packet.raw_header.len(), packet.header_length);
            assert!(packet.payload_length > 0);
            assert!(
                log.headers.lock().unwrap().contains(&packet.raw_header),
                "selected output did not receive the native event bytes"
            );
            let source = if incoming { 1 - who } else { who };
            let extension = copy
                .rtp_extension(RtcEventRtpExtensionKind::AudioLevel)
                .unwrap();
            assert_eq!(
                extension.decoded,
                Some(RtcEventDecodedRtpExtension::AudioLevel(
                    RtcEventAudioLevel {
                        level: if source == 0 { 127 } else { 0 },
                        voice_activity: source == 1
                    }
                ))
            );
            // Event bytes and the original parser map outlive all peer,
            // logger, output, factory, endpoint and controlled-world owners.
            std::thread::spawn(move || {
                assert_eq!(
                    copy.rtp_extension(RtcEventRtpExtensionKind::AudioLevel),
                    Some(extension)
                );
            })
            .join()
            .unwrap();
        }
    }
    drop(logs);
    let _next = ControlledWorld::acquire(731, Duration::from_secs(10)).unwrap();
}

fn run_h264_profile(temporal: bool, impaired: bool, simulcast_mode: u8) -> Trace {
    run_h264_profile_with_wire(temporal, impaired, simulcast_mode, None, false)
}

fn run_h264_profile_with_wire(
    temporal: bool,
    impaired: bool,
    simulcast_mode: u8,
    mut capture: Option<&mut WireCapture>,
    opaque: bool,
) -> Trace {
    let simulcast = simulcast_mode != 0;
    const KEY: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x0a, 0xd9, 0x1e, 0x84, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0xf0,
        0x3c, 0x48, 0x99, 0x20, 0, 0, 0, 1, 0x68, 0xcb, 0x80, 0xc4, 0xb2, 0, 0, 0, 1, 0x65, 0x88,
        0x84, 0xf1, 0x18, 0xa0, 0, 0x20, 0x5b, 0x1c, 0, 4, 7, 0xe3, 0x80, 0, 0x80, 0xfe,
    ];
    // Pinned PpsParser consumes the first three Exp-Golomb slice fields.
    // This fixture encodes them in 88 84 after the NAL header. Keep SPS/PPS
    // and that clear prefix; replace the remainder, not a complete RBSP.
    // Nonzero synthetic bytes avoid Annex-B start codes. No crypto or valid
    // decodable-slice claim is made, and native decode is unavailable here.
    let key_data = if opaque {
        let mut data = KEY[..41].to_vec();
        data.resize(41 + 4096, 0x55);
        data
    } else {
        KEY.to_vec()
    };
    fn fidelity(received: &[u8], expected: &[u8], opaque: bool) {
        if opaque {
            // Clear SPS/VUI and Annex-B framing may be normalized by native
            // receive. Protect only the tail, plus check its clear VCL prefix.
            assert!(received.len() >= 4099);
            assert_eq!(
                &received[received.len() - 4099..],
                &expected[expected.len() - 4099..]
            );
        } else {
            assert_eq!(received, expected);
        }
    }
    fn outcome(
        world: &ControlledWorld,
        network: &ControlledSimulatedNetwork,
        peers: &[PeerConnection; 2],
        source: &EncodedVideoSource,
        observations: &mut Observations,
        before: usize,
    ) -> Result<EncodedReceivedVideoFrame, CodecError> {
        for _ in 0..2_000 {
            observations.step(world, network, peers, true);
            if let Some(error) = source.take_encoder_error() {
                return Err(error);
            }
            let previous_timestamp = before
                .checked_sub(1)
                .map(|index| observations.trace.encoded_video[index].1.rtp_timestamp);
            if let Some((_, frame)) = observations.trace.encoded_video[before..]
                .iter()
                .rev()
                .find(|(_, frame)| Some(frame.rtp_timestamp) != previous_timestamp)
            {
                return Ok(frame.clone());
            }
            world.advance(Duration::from_millis(1)).unwrap();
        }
        panic!(
            "controlled H264 missing: temporal={}, pending={}, dropped={}, trace={:?}",
            source.latest_rate_control().is_some(),
            source.pending_frames(),
            source.dropped_frames(),
            observations.trace.events
        );
    }
    eprintln!("CONTROLLED_MEDIA_BEGIN");
    let trace = 'lifecycle: {
        let world = ControlledWorld::acquire(731, Duration::from_secs(10)).unwrap();
        let network = world.create_network().unwrap();
        let input = if simulcast {
            EncodedVideoInput::new_simulcast().unwrap()
        } else if temporal {
            EncodedVideoInput::new_l1t3([64, 128, 255]).unwrap()
        } else {
            EncodedVideoInput::new().unwrap()
        };
        let decoder = input.encoded_receive_factory().unwrap();
        assert!(
            decoder
                .query_support(&decoder.supported_formats()[0], false, None)
                .supported
        );
        assert!(decoder.decoder_statistics().is_none());
        let endpoints: Vec<_> = [Ipv4Addr::new(10, 79, 0, 1), Ipv4Addr::new(10, 79, 0, 2)]
            .into_iter()
            .map(|address| network.register_endpoint(address.into()).unwrap())
            .collect();
        let factories: Vec<_> = endpoints
            .iter()
            .map(|endpoint| {
                world
                    .peer_factory_builder()
                    .unwrap()
                    .network_manager(endpoint.network_manager().unwrap())
                    .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
                    .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
                    .audio_decoder_factory(AudioDecoderFactory::builtin_opus().unwrap())
                    .video_encoder_factory(input.encoder_factory())
                    .video_decoder_factory(decoder.clone())
                    .build()
                    .unwrap()
            })
            .collect();
        let mut peers = [
            factories[0]
                .create_peer_connection(PeerConfiguration::default())
                .unwrap(),
            factories[1]
                .create_peer_connection(PeerConfiguration::default())
                .unwrap(),
        ];
        let mut source = input.create_source(&factories[0]).unwrap();
        let track = source
            .create_track(&factories[0], "controlled-h264")
            .unwrap();
        let transceiver = if simulcast {
            peers[0]
                .add_video_transceiver_with_rids(
                    &track,
                    RtpTransceiverDirection::SendOnly,
                    &["q".into(), "h".into(), "f".into()],
                )
                .unwrap()
        } else {
            peers[0]
                .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
                .unwrap()
        };
        let mut observations = Observations {
            impaired,
            encoded_video_only: true,
            sender_only_simulcast_answer: simulcast_mode >= 2,
            capture_wire: capture.is_some(),
            ..Observations::default()
        };
        observations.negotiate_with_layers(&world, &network, &peers, 0, temporal);
        let sender = transceiver.sender();
        if capture.is_some() {
            observations.wire.extensions = transceiver
                .negotiated_header_extensions()
                .unwrap()
                .into_iter()
                .filter(|extension| extension.direction != RtpHeaderExtensionDirection::Stopped)
                .map(|extension| {
                    assert!(
                        !extension.preferred_encrypt(),
                        "offline probe requires clear extensions"
                    );
                    (
                        extension.preferred_id().unwrap(),
                        extension.uri().to_owned(),
                    )
                })
                .collect();
        }
        if simulcast_mode >= 2 || opaque {
            // Fund >MTU opaque fixtures through native allocation/pacing;
            // 4 KiB at 30 fps cannot fit the initial 300 kbps budget.
            peers[0]
                .set_bitrate(Some(3_000_000), Some(3_000_000), Some(4_000_000))
                .unwrap();
        }
        if temporal {
            let mut parameters = sender.parameters().unwrap();
            parameters.encodings[0].scalability_mode = Some("L1T3".into());
            sender.set_parameters(parameters).unwrap();
        }
        if simulcast_mode == 3 {
            let mut parameters = sender.parameters().unwrap();
            assert_eq!(parameters.encodings.len(), 3);
            for (index, encoding) in parameters.encodings.iter_mut().enumerate() {
                encoding.active = index == 1;
            }
            sender.set_parameters(parameters).unwrap();
        }
        assert_eq!(observations.encoded_video.len(), 1);
        let mut unit = EncodedVideoAccessUnit {
            data: key_data.clone(),
            width: if simulcast { 320 } else { 16 },
            height: if simulcast { 180 } else { 16 },
            timestamp_us: i64::try_from(world.now().as_micros()).unwrap(),
            key_frame: true,
            qp: None,
            metadata: EncodedVideoMetadata {
                codec: EncodedVideoCodec::H264 {
                    base_layer_sync: false,
                },
                simulcast_index: None,
                spatial_index: None,
                temporal_index: temporal.then_some(0),
                end_of_picture: true,
            },
        };
        let before = observations.trace.encoded_video.len();
        push_h264(&source, unit.clone(), simulcast);
        let result = if simulcast_mode == 2 {
            // The answer advertises external SFU acceptance only to the sender.
            // Its stock receiver is not configured for three simultaneous rungs.
            // Wait for native encoder consumption here, then qualify publication
            // with per-RID packet/SSRC statistics below, not receive callbacks.
            for _ in 0..2_000 {
                observations.step(&world, &network, &peers, true);
                assert!(source.take_encoder_error().is_none());
                if source.pending_frames() == 0 {
                    break;
                }
                world.advance(Duration::from_millis(1)).unwrap();
            }
            assert_eq!(source.pending_frames(), 0);
            Ok(None)
        } else {
            outcome(&world, &network, &peers, &source, &mut observations, before).map(Some)
        };
        if matches!(simulcast_mode, 1 | 3) {
            // Neither collapsed senders nor single-encoder fallback expose
            // enough native initialization identity to associate a rung.
            assert_eq!(result.unwrap_err(), CodecError::InvalidConfiguration);
            assert_eq!(source.pending_frames(), 0);
            assert_eq!(source.dropped_frames(), 3);
            assert!(observations.trace.encoded_video.is_empty());
            source.close().unwrap();
            for (_, sink) in &mut observations.encoded_video {
                sink.close();
            }
            for peer in &mut peers {
                peer.close().unwrap();
            }
            break 'lifecycle observations.trace;
        }
        let first = result.unwrap();
        if let Some(first) = &first {
            fidelity(&first.data, &unit.data, opaque);
            assert!(first.key_frame);
            assert_eq!(first.temporal_index, (temporal || simulcast).then_some(0));
            assert!(first.dependencies.is_empty());
            assert_eq!(first.frame_id.is_some(), temporal || simulcast);
        }
        let mut base_id = first.as_ref().and_then(|frame| frame.frame_id);
        source.take_keyframe_request();
        if simulcast {
            assert_eq!(
                source.take_encoding_keyframe_requests(),
                if simulcast_mode == 3 {
                    [false, true, false]
                } else {
                    [true; 3]
                }
            );
            let rates = source.latest_rate_control().unwrap();
            if simulcast_mode == 3 {
                assert!(rates.layer_bitrates_bps[1][0].unwrap_or(0) > 0);
                for index in [0, 2, 3, 4] {
                    assert!(
                        rates.layer_bitrates_bps[index]
                            .iter()
                            .all(|cell| cell.unwrap_or(0) == 0)
                    );
                }
            } else {
                assert!(
                    rates.layer_bitrates_bps[..3]
                        .iter()
                        .all(|layer| layer[0].unwrap_or(0) > 0)
                );
            }
        }
        let mut rejected = 0;
        for temporal_index in [2, 1, 2, 0] {
            if simulcast_mode == 2 {
                // Sender-only SFU fixture: stock native receiver is not a
                // three-rung metadata boundary. Qualify publication using
                // native allocation, callback feedback and sender statistics.
                world.advance(Duration::from_millis(34)).unwrap();
                unit.timestamp_us = i64::try_from(world.now().as_micros()).unwrap();
                push_h264(&source, unit.clone(), true);
                for _ in 0..2_000 {
                    observations.step(&world, &network, &peers, true);
                    assert!(source.take_encoder_error().is_none());
                    if source.pending_frames() == 0 {
                        break;
                    }
                    world.advance(Duration::from_millis(1)).unwrap();
                }
                assert_eq!(source.pending_frames(), 0);
                continue;
            }
            let mut delivered = None;
            for _ in 0..4 {
                world.advance(Duration::from_millis(34)).unwrap();
                unit.timestamp_us = i64::try_from(world.now().as_micros()).unwrap();
                unit.data = vec![0, 0, 0, 1, 0x41, 0x88, 0x84, 0xf1, temporal_index];
                if opaque {
                    unit.data.truncate(7);
                    unit.data.resize(7 + 4096, 0x60 + temporal_index);
                }
                unit.key_frame = false;
                unit.metadata.temporal_index = temporal.then_some(temporal_index);
                unit.metadata.codec = EncodedVideoCodec::H264 {
                    base_layer_sync: temporal && temporal_index != 0,
                };
                let before = observations.trace.encoded_video.len();
                push_h264(&source, unit.clone(), simulcast);
                match outcome(&world, &network, &peers, &source, &mut observations, before) {
                    Ok(frame) => {
                        delivered = Some(frame);
                        break;
                    }
                    Err(error) => {
                        assert_eq!(error, CodecError::InvalidFrame);
                        assert!(source.take_keyframe_request());
                        rejected += 1;
                        world.advance(Duration::from_millis(34)).unwrap();
                        let mut key = unit.clone();
                        key.timestamp_us = i64::try_from(world.now().as_micros()).unwrap();
                        key.data = key_data.clone();
                        key.key_frame = true;
                        key.metadata.temporal_index = temporal.then_some(0);
                        key.metadata.codec = EncodedVideoCodec::H264 {
                            base_layer_sync: false,
                        };
                        let before = observations.trace.encoded_video.len();
                        push_h264(&source, key.clone(), simulcast);
                        let frame =
                            outcome(&world, &network, &peers, &source, &mut observations, before)
                                .unwrap();
                        fidelity(&frame.data, &key.data, opaque);
                        base_id = frame.frame_id;
                        source.take_keyframe_request();
                    }
                }
            }
            let received =
                delivered.expect("controlled producer responds to native keyframe feedback");
            fidelity(&received.data, &unit.data, opaque);
            assert_eq!(received.ssrc, first.as_ref().unwrap().ssrc);
            assert_eq!(
                received.temporal_index,
                if simulcast {
                    Some(0)
                } else {
                    temporal.then_some(i32::from(temporal_index))
                }
            );
            if temporal {
                assert!(received.frame_id.unwrap() > base_id.unwrap());
                assert_eq!(received.dependencies, vec![base_id.unwrap()]);
                assert_eq!(received.decode_target_indications.len(), 4);
                let rates = source.latest_rate_control().unwrap();
                assert!(rates.layer_bitrates_bps[0][2].is_some());
            } else if simulcast {
                assert!(received.frame_id.unwrap() > base_id.unwrap());
                assert_eq!(received.dependencies, vec![base_id.unwrap()]);
                // Pinned H264ToGeneric advertises kMaxTemporalStreams (4)
                // decode targets even for an absent temporal index / TL0.
                assert_eq!(received.decode_target_indications.len(), 4);
            } else {
                assert!(received.frame_id.is_none());
                assert!(received.decode_target_indications.is_empty());
            }
            if temporal_index == 0 || simulcast {
                base_id = received.frame_id;
            }
        }
        assert_eq!(
            source.dropped_frames(),
            if simulcast_mode == 3 {
                10 + 5 * rejected
            } else if simulcast {
                3 * rejected
            } else {
                rejected
            }
        );
        assert_eq!(source.pending_frames(), 0);
        if simulcast_mode >= 2 {
            let operation = peers[0].request_stats().unwrap();
            let mut snapshot = None;
            for _ in 0..2_000 {
                observations.step(&world, &network, &peers, true);
                if let Some(at) = observations
                    .stats
                    .iter()
                    .position(|(who, stats)| *who == 0 && stats.operation_id == operation)
                {
                    snapshot = Some(observations.stats.remove(at).1);
                    break;
                }
                world.advance(Duration::from_millis(1)).unwrap();
            }
            let snapshot = snapshot.expect("native sender stats missing");
            let outgoing: Vec<_> = snapshot
                .records
                .iter()
                .filter_map(|record| match record {
                    PeerStatsRecord::OutboundRtp(stats)
                        if stats.kind.as_deref() == Some(b"video".as_slice())
                            && stats.packets_sent.unwrap_or(0) > 0 =>
                    {
                        Some((stats.rid.clone().unwrap(), stats.ssrc.unwrap()))
                    }
                    _ => None,
                })
                .collect();
            if capture.is_some() {
                observations.wire.streams = outgoing
                    .iter()
                    .map(|(rid, ssrc)| {
                        (
                            String::from_utf8(rid.clone()).unwrap(),
                            u32::try_from(*ssrc).unwrap(),
                        )
                    })
                    .collect();
            }
            assert_eq!(
                outgoing
                    .iter()
                    .map(|(rid, _)| std::str::from_utf8(rid).unwrap())
                    .collect::<std::collections::HashSet<_>>(),
                if simulcast_mode == 3 {
                    std::collections::HashSet::from(["h"])
                } else {
                    std::collections::HashSet::from(["q", "h", "f"])
                }
            );
            assert_eq!(
                outgoing
                    .iter()
                    .map(|(_, ssrc)| *ssrc)
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                outgoing.len()
            );
        }
        source.close().unwrap();
        for (_, sink) in &mut observations.encoded_video {
            if simulcast_mode != 2 {
                assert_eq!(sink.dropped_frames(), 0);
            } else {
                // Unsupported simultaneous-receive endpoint is not evidence
                // of media delivery. Preserve its observable queue-drop count.
                observations.trace.events.push(format!(
                    "sender-only-endpoint-drops:{}",
                    sink.dropped_frames()
                ));
            }
            sink.close();
            assert!(sink.try_next_frame().is_none());
        }
        let receiver = observations.transceivers[0].1.receiver();
        assert!(
            receiver.attach_encoded_sink().is_err(),
            "native encoded attachment is terminal for this receiver"
        );
        assert!(receiver.attach_ordered_encoded_sink().is_err());
        assert!(
            receiver.track().unwrap().attach_sink().is_err(),
            "closing encoded interception must not advertise H264 decoding"
        );
        for peer in &mut peers {
            peer.close().unwrap();
        }
        if let Some(target) = capture.as_mut() {
            **target = std::mem::take(&mut observations.wire);
        }
        observations.trace
    };
    eprintln!("CONTROLLED_MEDIA_END");
    trace
}

fn run_h264(temporal: bool, impaired: bool) -> Trace {
    run_h264_profile(temporal, impaired, 0)
}

fn push_h264(source: &EncodedVideoSource, unit: EncodedVideoAccessUnit, simulcast: bool) {
    if simulcast {
        let frames = std::array::from_fn(|index| {
            let mut frame = unit.clone();
            frame.width *= 1 << index;
            frame.height *= 1 << index;
            frame.metadata.simulcast_index = Some(index as u8);
            frame
        });
        source.push_simulcast(frames).unwrap();
    } else {
        source.push_encoded(unit).unwrap();
    }
}

#[test]
fn controlled_simulcast_input_rejects_collapsed_sender_and_replays() {
    let _guard = CONTROLLED_TEST
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // Honest stock-native answer declines simultaneous simulcast reception.
    // Reject the ambiguous native fallback, not claim three-rung receipt.
    for impaired in [false, true] {
        let first = run_h264_profile(false, impaired, 1);
        let replay = run_h264_profile(false, impaired, 1);
        assert_eq!(first, replay);
        assert!(first.video.is_empty());
        assert!(first.encoded_video.is_empty());
    }
}

#[test]
fn controlled_h264_simulcast_native_sender_rejects_ambiguous_fallback() {
    let _guard = CONTROLLED_TEST
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    for mode in [2, 3] {
        for impaired in [false, true] {
            let first = run_h264_profile(false, impaired, mode);
            let replay = run_h264_profile(false, impaired, mode);
            assert_eq!(first, replay);
            assert!(first.video.is_empty());
            // Mode 2's per-RID packet and distinct-SSRC statistics inside the
            // fixture establish publication, not simultaneous stock reception.
            // The rejected ambiguous fallback must still emit no receive output.
            if mode != 2 {
                assert!(first.encoded_video.is_empty());
            }
        }
    }
}

#[test]
fn controlled_h264_opaque_tail_survives_fragmentation_without_decode() {
    let _guard = CONTROLLED_TEST
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    for temporal in [false, true] {
        for impaired in [false, true] {
            let mut wire = WireCapture::default();
            let first = run_h264_profile_with_wire(temporal, impaired, 0, Some(&mut wire), true);
            let replay = run_h264_profile_with_wire(temporal, impaired, 0, None, true);
            assert_eq!(first, replay);
            assert!(first.video.is_empty());
            assert!(first.encoded_video.len() >= 5);
            assert!(
                first
                    .encoded_video
                    .iter()
                    .all(|(_, frame)| frame.data.len() >= 4099)
            );
            // Captured full UDP payloads, not a guessed RTP header/payload
            // accounting model. Reassembled protected tails exceed this bound.
            assert!(!wire.packets.is_empty());
            // IPv4 fixture: include the 28-byte IP/UDP transport overhead.
            assert!(wire.packets.iter().all(|packet| packet.len() + 28 <= 1500));
        }
    }
}

#[test]
#[ignore = "requires PULSEBEAM_NATIVE_VLA_PROBE compiled against the matching exported SDK"]
fn native_vla_wire_uses_provided_offline_parser() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let _guard = CONTROLLED_TEST
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let probe = std::env::var_os("PULSEBEAM_NATIVE_VLA_PROBE").expect("native parser probe path");
    for (impaired, opaque) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut wire = WireCapture::default();
        let _trace = run_h264_profile_with_wire(false, impaired, 2, Some(&mut wire), opaque);
        // All native roots and controlled hooks are gone. Only the external
        // test process invokes this offline parser, never an engine helper.
        let mut child = Command::new(&probe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        for (id, uri) in wire.extensions {
            writeln!(input, "extension {id} {uri}").unwrap();
        }
        for (rid, ssrc) in wire.streams {
            writeln!(input, "stream {rid} {ssrc}").unwrap();
        }
        for packet in wire.packets {
            let mut hex = String::with_capacity(packet.len() * 2);
            for byte in packet {
                std::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}")).unwrap();
            }
            writeln!(input, "packet {hex}").unwrap();
        }
        drop(input);
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "native wire probe impaired={impaired} opaque={opaque}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!(
            "impaired={impaired} opaque={opaque}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn controlled_opus_source_fanout_bounds_copies_and_preserves_packets() {
    let _guard = CONTROLLED_TEST
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    for include_level in [true, false] {
        run_opus_fanout_metadata(include_level);
    }
}

fn run_opus_fanout_metadata(include_level: bool) {
    let world = ControlledWorld::acquire(734, Duration::from_secs(10)).unwrap();
    let network = world.create_network().unwrap();
    let input = EncodedVideoInput::new_for_format(VideoCodecFormat::new("VP8")).unwrap();
    let decoder = AudioDecoderFactory::builtin_opus().unwrap();
    let video_decoder = VideoDecoderFactoryHandle::builtin_vp8().unwrap();
    let audio_encoder = AudioEncoderFactory::with_opus_frames().unwrap();
    assert_eq!(
        OpusAudioLevel::new(128, false),
        Err(OpusInputError::InvalidAudioLevel)
    );
    let endpoints: Vec<_> = [1, 2]
        .into_iter()
        .map(|host| {
            network
                .register_endpoint(Ipv4Addr::new(10, 80, 1, host).into())
                .unwrap()
        })
        .collect();
    let factories: Vec<_> = endpoints
        .iter()
        .map(|endpoint| {
            world
                .peer_factory_builder()
                .unwrap()
                .network_manager(endpoint.network_manager().unwrap())
                .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
                .audio_encoder_factory(audio_encoder.clone())
                .audio_decoder_factory(decoder.clone())
                .video_encoder_factory(input.encoder_factory())
                .video_decoder_factory(video_decoder.clone())
                .build()
                .unwrap()
        })
        .collect();
    let mut peers = [
        factories[0]
            .create_peer_connection(PeerConfiguration::default())
            .unwrap(),
        factories[1]
            .create_peer_connection(PeerConfiguration::default())
            .unwrap(),
    ];
    let mut source = factories[0].create_encoded_audio_source(1).unwrap();
    let mut independent = factories[0].create_encoded_audio_source(1).unwrap();
    let mut transceivers = Vec::new();
    for (id, source) in [
        ("fanout-one", &source),
        ("fanout-two", &source),
        ("independent-three", &independent),
    ] {
        let track = factories[0].create_encoded_audio_track(id, source).unwrap();
        let transceiver = peers[0]
            .add_audio_transceiver(&track, RtpTransceiverDirection::SendOnly)
            .unwrap();
        if !include_level {
            let mut extensions = transceiver.header_extensions_to_negotiate().unwrap();
            extensions
                .iter_mut()
                .find(|extension| extension.uri() == "urn:ietf:params:rtp-hdrext:ssrc-audio-level")
                .unwrap()
                .direction = RtpHeaderExtensionDirection::Stopped;
            transceiver
                .set_header_extensions_to_negotiate(&extensions)
                .unwrap();
        }
        transceivers.push(transceiver);
    }
    let mut observations = Observations::default();
    observations.negotiate(&world, &network, &peers, 0);
    for _ in 0..2_000 {
        observations.step(&world, &network, &peers, false);
        if [0, 1].into_iter().all(|who| {
            observations
                .trace
                .events
                .iter()
                .any(|event| event.contains(&format!(":{who}:connection:Connected")))
        }) {
            break;
        }
        world.advance(Duration::from_millis(1)).unwrap();
    }
    for who in [0, 1] {
        assert!(
            observations
                .trace
                .events
                .iter()
                .any(|event| { event.contains(&format!(":{who}:connection:Connected")) })
        );
    }
    for (_, sink) in &mut observations.audio {
        sink.close().unwrap();
    }
    observations.audio.clear();
    let receivers = peers[1].audio_receivers().unwrap();
    assert_eq!(receivers.len(), 3);
    let sinks: Vec<_> = receivers
        .iter()
        .map(|receiver| receiver.attach_encoded_audio_sink().unwrap())
        .collect();
    let before = decoder.decoder_statistics().unwrap().input_packets;
    let frame = |index: u32| OpusInputFrame {
        data: OPUS[0].to_vec(),
        rtp_timestamp: index * 960,
        samples_per_channel: 960,
    };
    // No pumping occurs during admission: 6 packets * 2 slots * 2 native
    // recipients exhaust the 24-copy source budget, not 12 source packets.
    let level = |index: usize, opposite: bool| {
        let dbov = [0, 127, 73, 2, 111, 127, 42, 88, 7, 120][index];
        let voice = index % 2 == 1;
        OpusAudioLevel::new(if opposite { 127 - dbov } else { dbov }, voice != opposite).unwrap()
    };
    for index in 0..6 {
        source
            .push_opus_with_audio_level(&frame(index), level(index as usize, false))
            .unwrap();
        independent
            .push_opus_with_audio_level(&frame(index), level(index as usize, true))
            .unwrap();
    }
    let retry = frame(6);
    assert_eq!(source.push_opus(&retry), Err(OpusInputError::Backpressure));
    world.pump(512);
    // A legacy submission immediately after declared metadata must not inherit
    // its predecessor's declared digital-silence level.
    source.push_opus(&retry).unwrap();
    independent
        .push_opus_with_audio_level(&retry, level(6, true))
        .unwrap();
    // All three SILK bandwidth groups' final TOC configuration means 60 ms,
    // not 80 ms. Opaque packet bodies are deliberately never decoded.
    for (index, config) in [3, 7, 11].into_iter().enumerate() {
        world.pump(512);
        let frame = OpusInputFrame {
            data: vec![config << 3, 0xff, 0xfe],
            rtp_timestamp: 6720 + index as u32 * 2880,
            samples_per_channel: 2880,
        };
        assert_eq!(source.capture_time_now().unwrap(), world.now());
        let future = world.now() + Duration::from_millis(1);
        assert_eq!(
            source.push_opus_at(&frame, future),
            Err(OpusInputError::InvalidTimestamp)
        );
        assert_eq!(
            source.push_opus_at_with_audio_level(&frame, future, level(index + 7, false)),
            Err(OpusInputError::InvalidTimestamp)
        );
        // Failed timing validation neither reserves capacity nor advances RTP
        // admission: the same frame remains valid at the current capture time.
        source
            .push_opus_at_with_audio_level(&frame, world.now(), level(index + 7, false))
            .unwrap();
        independent
            .push_opus_with_audio_level(&frame, level(index + 7, true))
            .unwrap();
    }
    let mut received = [Vec::new(), Vec::new(), Vec::new()];
    for _ in 0..2_000 {
        observations.step(&world, &network, &peers, false);
        for (index, sink) in sinks.iter().enumerate() {
            while let Some(frame) = sink.try_next_frame() {
                received[index].push(frame);
            }
        }
        if received.iter().all(|frames| frames.len() == 10) {
            break;
        }
        world.advance(Duration::from_millis(1)).unwrap();
    }
    assert_eq!(received.each_ref().map(Vec::len), [10, 10, 10]);
    assert_ne!(received[0][0].ssrc, received[1][0].ssrc);
    assert_ne!(received[0][0].ssrc, received[2][0].ssrc);
    assert_ne!(received[1][0].ssrc, received[2][0].ssrc);
    for (who, frames) in received.iter().enumerate() {
        for (index, frame) in frames.iter().enumerate() {
            if !include_level {
                assert_eq!(frame.audio_level_dbov, None);
                assert_eq!(frame.voice_activity, None);
            } else if index == 6 && who < 2 {
                assert_ne!(frame.audio_level_dbov, Some(127));
                assert_eq!(frame.voice_activity, Some(true));
            } else {
                let expected = level(index, who == 2);
                assert_eq!(frame.audio_level_dbov, Some(expected.level_dbov()));
                assert_eq!(frame.voice_activity, Some(expected.voice_activity()));
            }
            if index < 7 {
                assert_eq!(frame.data, OPUS[0]);
                assert_eq!(frame.samples_per_channel, 960);
            } else {
                assert_eq!(frame.data, vec![[3, 7, 11][index - 7] << 3, 0xff, 0xfe]);
                assert_eq!(frame.samples_per_channel, 2880);
            }
        }
        for (index, pair) in frames.windows(2).enumerate() {
            assert_eq!(
                pair[1].rtp_timestamp.wrapping_sub(pair[0].rtp_timestamp),
                if index < 7 { 960 } else { 2880 }
            );
        }
    }
    assert_eq!(decoder.decoder_statistics().unwrap().input_packets, before);
    assert_eq!(audio_encoder.opus_frame_handoff_failures(), 0);
    let owned = received[0][0].clone();
    // Accept six 10 ms inputs, give bounded queued/no-output work one turn,
    // then retire both old senders. Their generations cannot supply metadata
    // to a later encoder even if the native queue allocator reuses an address.
    source
        .push_opus_with_audio_level(
            &OpusInputFrame {
                data: vec![3 << 3, 0xff, 0xfe],
                rtp_timestamp: 15360,
                samples_per_channel: 2880,
            },
            OpusAudioLevel::new(126, false).unwrap(),
        )
        .unwrap();
    world.pump(1);
    for transceiver in &transceivers[..2] {
        transceiver.stop().unwrap();
    }
    source.close().unwrap();
    let previous_ids: Vec<_> = receivers.iter().map(|receiver| receiver.id()).collect();
    let mut replacement = factories[0].create_encoded_audio_source(1).unwrap();
    let track = factories[0]
        .create_encoded_audio_track("fanout-reborn", &replacement)
        .unwrap();
    let transceiver = peers[0]
        .add_audio_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    if !include_level {
        let mut extensions = transceiver.header_extensions_to_negotiate().unwrap();
        extensions
            .iter_mut()
            .find(|extension| extension.uri() == "urn:ietf:params:rtp-hdrext:ssrc-audio-level")
            .unwrap()
            .direction = RtpHeaderExtensionDirection::Stopped;
        transceiver
            .set_header_extensions_to_negotiate(&extensions)
            .unwrap();
    }
    observations.negotiate(&world, &network, &peers, 0);
    for (_, sink) in &mut observations.audio {
        sink.close().unwrap();
    }
    observations.audio.clear();
    let receiver = peers[1]
        .audio_receivers()
        .unwrap()
        .into_iter()
        .find(|receiver| !previous_ids.contains(&receiver.id()))
        .expect("fresh audio receiver");
    let new_sink = receiver.attach_encoded_audio_sink().unwrap();
    let new_payload = vec![0xf8, 0xfe, 0xff];
    replacement
        .push_opus_with_audio_level(
            &OpusInputFrame {
                data: new_payload.clone(),
                rtp_timestamp: 0,
                samples_per_channel: 960,
            },
            OpusAudioLevel::new(76, true).unwrap(),
        )
        .unwrap();
    independent
        .push_opus_with_audio_level(
            &OpusInputFrame {
                data: OPUS[0].to_vec(),
                rtp_timestamp: 15360,
                samples_per_channel: 960,
            },
            OpusAudioLevel::new(3, false).unwrap(),
        )
        .unwrap();
    let mut reborn = None;
    let mut continuous = None;
    for _ in 0..2_000 {
        observations.step(&world, &network, &peers, false);
        if let Some(frame) = new_sink.try_next_frame() {
            reborn = Some(frame);
        }
        if let Some(frame) = sinks[2].try_next_frame() {
            continuous = Some(frame);
        }
        if reborn.is_some() && continuous.is_some() {
            break;
        }
        world.advance(Duration::from_millis(1)).unwrap();
    }
    let reborn = reborn.expect("replacement packet");
    let continuous = continuous.expect("unrelated sender continuity");
    assert_eq!(reborn.data, new_payload);
    assert_eq!(reborn.samples_per_channel, 960);
    assert_eq!(continuous.data, OPUS[0]);
    assert_eq!(reborn.audio_level_dbov, include_level.then_some(76));
    assert_eq!(reborn.voice_activity, include_level.then_some(true));
    assert_eq!(continuous.audio_level_dbov, include_level.then_some(3));
    assert_eq!(continuous.voice_activity, include_level.then_some(false));
    assert_eq!(decoder.decoder_statistics().unwrap().input_packets, before);
    // Shutdown with freshly accepted metadata still queued.
    replacement
        .push_opus_with_audio_level(
            &OpusInputFrame {
                data: new_payload,
                rtp_timestamp: 960,
                samples_per_channel: 960,
            },
            OpusAudioLevel::new(91, false).unwrap(),
        )
        .unwrap();
    replacement.close().unwrap();
    independent.close().unwrap();
    for peer in &mut peers {
        peer.close().unwrap();
    }
    assert_eq!(audio_encoder.opus_frame_handoff_failures(), 0);
    assert_eq!(owned.data, OPUS[0]);
}

#[test]
fn controlled_simulcast_admission_is_atomic_and_bitrate_errors_are_native() {
    let _guard = CONTROLLED_TEST
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let world = ControlledWorld::acquire(733, Duration::from_secs(10)).unwrap();
    let network = world.create_network().unwrap();
    let endpoint = network
        .register_endpoint(Ipv4Addr::new(10, 79, 1, 1).into())
        .unwrap();
    let input = EncodedVideoInput::new_simulcast().unwrap();
    let factory = world
        .peer_factory_builder()
        .unwrap()
        .network_manager(endpoint.network_manager().unwrap())
        .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
        .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
        .audio_decoder_factory(AudioDecoderFactory::builtin_opus().unwrap())
        .video_encoder_factory(input.encoder_factory())
        .video_decoder_factory(input.encoded_receive_factory().unwrap())
        .build()
        .unwrap();
    let mut source = input.create_source(&factory).unwrap();
    let frames = std::array::from_fn(|index| EncodedVideoAccessUnit {
        data: vec![
            0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0, 0, 1, 0x68, 1, 0, 0, 1, 0x65, 1,
        ],
        width: 320 << index,
        height: 180 << index,
        timestamp_us: 1,
        key_frame: true,
        qp: None,
        metadata: EncodedVideoMetadata {
            codec: EncodedVideoCodec::H264 {
                base_layer_sync: false,
            },
            simulcast_index: Some(index as u8),
            spatial_index: None,
            temporal_index: None,
            end_of_picture: true,
        },
    });
    let mutations: [fn(&mut [EncodedVideoAccessUnit; 3]); 8] = [
        |units| units[2].metadata.simulcast_index = Some(1),
        |units| units[2].metadata.temporal_index = Some(0),
        |units| units[2].metadata.codec = EncodedVideoCodec::Av1,
        |units| units[2].timestamp_us = 2,
        |units| units[2].width = 320,
        |units| units[2].qp = Some(52),
        |units| units[2].data.clear(),
        |units| units[2].key_frame = false,
    ];
    for mutate in mutations {
        let mut invalid = frames.clone();
        mutate(&mut invalid);
        assert_eq!(
            source.push_simulcast(invalid),
            Err(CodecError::InvalidFrame)
        );
        assert_eq!(source.pending_frames(), 0);
        assert_eq!(source.dropped_frames(), 0);
    }
    assert_eq!(
        source.push_encoded(frames[0].clone()),
        Err(CodecError::InvalidConfiguration)
    );
    source.push_simulcast(frames.clone()).unwrap();
    assert_eq!(source.pending_frames(), 3);
    assert_eq!(
        source.push_simulcast(frames.clone()),
        Err(CodecError::InvalidFrame)
    );
    assert_eq!(source.pending_frames(), 3);
    source.close().unwrap();
    assert_eq!(source.pending_frames(), 0);
    assert_eq!(source.push_simulcast(frames), Err(CodecError::Released));

    let mut peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    assert_eq!(
        peer.set_bitrate(Some(u32::MAX), None, None)
            .unwrap_err()
            .kind,
        PeerErrorKind::InvalidParameter
    );
    let native_error = peer
        .set_bitrate(Some(200_000), Some(100_000), Some(300_000))
        .unwrap_err();
    assert!(!native_error.message.is_empty());
    peer.set_bitrate(Some(100_000), Some(200_000), Some(300_000))
        .unwrap();
    peer.set_bitrate(None, None, None).unwrap();
    peer.close().unwrap();
    assert_eq!(
        peer.set_bitrate(None, None, None).unwrap_err().kind,
        PeerErrorKind::Closed
    );
}

#[test]
fn controlled_h264_encoded_temporal_media_preserves_native_dependencies_and_replay() {
    let _guard = CONTROLLED_TEST
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    for temporal in [false, true] {
        for impaired in [false, true] {
            let first = run_h264(temporal, impaired);
            let replay = run_h264(temporal, impaired);
            assert_eq!(
                first, replay,
                "H264 temporal={temporal} impaired={impaired}"
            );
            assert!(first.video.is_empty(), "H264 was not decoded");
            assert!(first.encoded_video.len() >= 5);
            if impaired {
                assert!(
                    first
                        .events
                        .iter()
                        .any(|event| event.contains(":schedule:"))
                );
            }
        }
    }
}

#[test]
fn replay_normalization_excludes_only_crypto_values() {
    let sdp = "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=ice-ufrag:first\r\na=ice-pwd:password\r\na=fingerprint:sha-256 AA:BB\r\na=mid:0\r\na=candidate:1 1 udp 123 10.77.0.1 5000 typ host ufrag first\r\n";
    let changed_secrets = sdp
        .replace("first", "second")
        .replace("password", "different")
        .replace("AA:BB", "CC:DD");
    assert_eq!(normalized_sdp(sdp), normalized_sdp(&changed_secrets));
    for (old, new) in [
        ("5000", "5001"),
        ("a=mid:0", "a=mid:1"),
        ("111", "112"),
        ("sha-256", "sha-512"),
    ] {
        assert_ne!(normalized_sdp(sdp), normalized_sdp(&sdp.replace(old, new)));
    }
}

#[test]
fn controlled_metronomes_deliver_real_media_and_retire_pending_ticks() {
    let _guard = CONTROLLED_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let addresses = [
        Ipv4Addr::new(10, 77, 2, 1).into(),
        Ipv4Addr::new(10, 77, 2, 2).into(),
    ];
    let mut traces = Vec::new();
    for _ in 0..2 {
        let metronomes = [
            Arc::new(DrivenMetronome::default()),
            Arc::new(DrivenMetronome::default()),
        ];
        traces.push(run_media_with_metronomes(
            addresses,
            false,
            Some(&metronomes),
        ));
        for source in &metronomes {
            assert!(source.periods.load(Ordering::Relaxed) > 0);
            assert!(source.requests.load(Ordering::Relaxed) > 0);
        }
        let pending: Vec<_> = metronomes
            .iter()
            .flat_map(|source| {
                std::mem::take(&mut *source.pending.lock().unwrap())
                    .into_iter()
                    .map(|(_, tick)| tick)
            })
            .collect();
        assert!(
            !pending.is_empty(),
            "decode consumers must retain their next tick"
        );
        // No native factory, world, task identity or worker survives. Retained
        // tickets can migrate and reject delivery without accessing that worker.
        std::thread::spawn(move || {
            for tick in pending {
                assert_eq!(tick.signal(), Err(MetronomeTickError::Retired));
            }
        })
        .join()
        .unwrap();
    }
    assert_eq!(traces[0], traces[1]);
}

#[test]
fn builtin_opus_vp8_links_have_real_owned_output_and_replay() {
    let _guard = CONTROLLED_TEST
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let mut scenarios = Vec::new();
    for addresses in [
        [
            Ipv4Addr::new(10, 77, 0, 1).into(),
            Ipv4Addr::new(10, 77, 0, 2).into(),
        ],
        [
            Ipv6Addr::new(0xfd00, 77, 0, 0, 0, 0, 0, 1).into(),
            Ipv6Addr::new(0xfd00, 77, 0, 0, 0, 0, 0, 2).into(),
        ],
    ] {
        for impaired in [false, true] {
            let first = run_media(addresses, impaired);
            let second = run_media(addresses, impaired);
            assert_eq!(
                first, second,
                "logical/media replay diverged: {addresses:?} impaired={impaired}"
            );
            if impaired {
                assert!(first.events.iter().any(|event| event.ends_with(":drop")));
                assert!(
                    first
                        .events
                        .iter()
                        .any(|event| event.ends_with(":duplicate"))
                );
            }
            scenarios.push((addresses, impaired, first));
        }
    }
    // Outside all controlled lifecycles, prove owned outputs are passive and
    // safely readable on another thread without any native root or dispatch.
    let passive = (
        scenarios[0].2.audio[0].clone(),
        scenarios[0].2.video[0].clone(),
    );
    let expected = passive.clone();
    let returned = std::thread::spawn(move || {
        assert!(!passive.0.frame.samples.is_empty());
        assert!(!passive.1.frame.buffer.as_bytes().is_empty());
        passive
    })
    .join()
    .unwrap();
    assert_eq!(returned, expected);
    if let Some(path) = std::env::var_os("PULSEBEAM_CONTROLLED_TRACE_PATH") {
        let traces: Vec<_> = scenarios.into_iter().map(|(addresses, impaired, first)| {
        let trace = serde_json::json!({
            "addresses": addresses.map(|ip| ip.to_string()), "impaired": impaired,
            "events": first.events,
            "messages": first.messages.iter().map(|(peer, message)| serde_json::json!({ "peer": peer, "kind": format!("{:?}", message.kind), "bytes": message.bytes })).collect::<Vec<_>>(),
            "audio": first.audio.iter().map(|frame| serde_json::json!({
                "peer": frame.peer_id, "receiver": frame.receiver_id,
                "observed_us": frame.observed_at.map(|time| time.as_micros()),
                "capture_ms": frame.frame.capture_time_ms,
                "capture_us": frame.frame.capture_time_us,
                "rate": frame.frame.sample_rate_hz, "channels": frame.frame.channels,
                "samples_per_channel": frame.frame.samples_per_channel,
                "samples": frame.frame.samples,
            })).collect::<Vec<_>>(),
            "video": first.video.iter().map(|frame| serde_json::json!({
                "peer": frame.peer_id, "track": frame.track_id,
                "observed_us": frame.observed_at.map(|time| time.as_micros()),
                "capture_us": frame.frame.timestamp_us, "rtp_timestamp": frame.frame.rtp_timestamp,
                "width": frame.frame.width, "height": frame.frame.height,
                "rotation": frame.frame.rotation as u16,
                "i420": frame.frame.buffer.as_bytes(),
            })).collect::<Vec<_>>(),
        });
        trace
        }).collect();
        // Serialization/file I/O is outside every controlled lifecycle marker.
        std::fs::write(path, serde_json::to_vec(&traces).unwrap()).unwrap();
    }
}

#[test]
fn controlled_peer_admission_and_pending_close_preserve_exact_outcomes() {
    if isolated::run_in_child("controlled_peer_admission_and_pending_close_preserve_exact_outcomes")
    {
        return;
    }
    let _guard = CONTROLLED_TEST.lock().unwrap_or_else(|e| e.into_inner());
    eprintln!("CONTROLLED_MEDIA_BEGIN");
    {
        let world = ControlledWorld::acquire(735, Duration::from_secs(10)).unwrap();
        let network = world.create_network().unwrap();
        let endpoint = network
            .register_endpoint(Ipv4Addr::new(10, 79, 0, 1).into())
            .unwrap();
        let factory = world
            .peer_factory_builder()
            .unwrap()
            .network_manager(endpoint.network_manager().unwrap())
            .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
            .build()
            .unwrap();
        let mut peer = factory
            .create_peer_connection(PeerConfiguration::default())
            .unwrap();
        let mut accepted = std::collections::HashSet::new();
        accepted.insert(peer.create_offer().unwrap());
        // Keep one real native SDP callback pending; queuing more behind it
        // is rejected rather than creating upstream shutdown callback hazards.
        assert_eq!(
            peer.create_offer().unwrap_err().kind,
            PeerErrorKind::ResourceExhausted
        );
        for _ in 0..63 {
            accepted.insert(
                peer.add_ice_candidate(IceCandidate {
                    sdp_mid: "unit".into(),
                    sdp_mline_index: 0,
                    candidate: "not a candidate".into(),
                })
                .unwrap(),
            );
        }
        let before = peer.event_observation();
        assert_eq!(before.admitted_operations, 64);
        assert!(
            before.pending_operations > 0,
            "no real pending native work exercised"
        );
        #[cfg(target_os = "linux")]
        let threads_before: std::collections::BTreeSet<_> = std::fs::read_dir("/proc/self/task")
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let source = factory.create_video_source().unwrap();
        let track = factory
            .create_video_track("controlled-source-controls", &source)
            .unwrap();
        for view in [source.as_view(), track.source().unwrap()] {
            view.request_refresh_frame();
            view.generate_key_frame();
            view.process_constraints(VideoSourceConstraints {
                min_fps: Some(0.0),
                max_fps: None,
            });
        }
        track.request_refresh_frame();
        #[cfg(target_os = "linux")]
        {
            let threads_after: std::collections::BTreeSet<_> = std::fs::read_dir("/proc/self/task")
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(threads_before, threads_after);
        }
        // Real callbacks were pending before these source operations. They
        // must not pump that work, advance time or create worker threads.
        assert_eq!(peer.event_observation(), before);
        assert_eq!(world.now(), Duration::from_secs(10));
        for _ in 0..128 {
            assert_eq!(
                peer.create_offer().unwrap_err().kind,
                PeerErrorKind::ResourceExhausted
            );
        }
        assert_eq!(peer.event_observation(), before);
        assert_eq!(world.now(), Duration::from_secs(10));
        peer.close().unwrap();
        peer.close().unwrap();
        let mut closed = 0;
        while let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::OperationComplete(done) => {
                    assert!(accepted.remove(&done.operation_id));
                    assert!(
                        done.result.is_err(),
                        "unprogressed work unexpectedly succeeded"
                    );
                    // Native close can invoke an engine Internal error before
                    // binding cancellation; keep that actual error, not Closed.
                }
                PeerConnectionEvent::Closed => closed += 1,
                event => panic!("unexpected control after close: {event:?}"),
            }
        }
        assert!(accepted.is_empty());
        assert_eq!(closed, 1);
        assert_eq!(peer.event_observation().admitted_operations, 0);
        drop(peer);
        drop(factory);
        // No later pump of now-inaccessible peer resources is needed for drop.
    }
    {
        let _next = ControlledWorld::acquire(736, Duration::from_secs(10)).unwrap();
    }
    eprintln!("CONTROLLED_MEDIA_END");
}

#[test]
fn controlled_configuration_rejects_mismatched_endpoints_before_work() {
    let _guard = CONTROLLED_TEST
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    eprintln!("CONTROLLED_MEDIA_BEGIN");
    {
        let world = ControlledWorld::acquire(731, Duration::from_secs(10)).unwrap();
        let network = world.create_network().unwrap();
        let first = network
            .register_endpoint(Ipv4Addr::new(10, 78, 0, 1).into())
            .unwrap();
        let second = network
            .register_endpoint(Ipv4Addr::new(10, 78, 0, 2).into())
            .unwrap();
        let another_network = world.create_network().unwrap();
        let foreign = another_network
            .register_endpoint(Ipv4Addr::new(10, 78, 0, 1).into())
            .unwrap();
        for endpoint in [&second, &foreign] {
            let error = world
                .peer_factory_builder()
                .unwrap()
                .network_manager(first.network_manager().unwrap())
                .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
                .build()
                .err()
                .expect("mismatched endpoint accepted");
            assert_eq!(error.kind, PeerErrorKind::InvalidParameter);
            assert_eq!(world.now(), Duration::from_secs(10));
            assert_eq!(world.next_deadline(), None);
        }
        // Omitted codecs select normal native defaults, not an unadmitted
        // simulation profile. Merely constructing them does not supply media.
        let defaults = world
            .peer_factory_builder()
            .unwrap()
            .network_manager(first.network_manager().unwrap())
            .packet_socket_factory(first.packet_socket_factory().unwrap())
            .build()
            .unwrap();
        drop(defaults);
        // A successful native construction may post ready cleanup. Unlike a
        // rejected endpoint selection, it need not leave the scheduler empty
        // before the harness runs that cleanup.
        world.pump(1024);
        assert_eq!(world.now(), Duration::from_secs(10));
        assert_eq!(world.next_deadline(), None);
        let input = EncodedVideoInput::new_for_format(VideoCodecFormat::new("VP8")).unwrap();
        let audio_encoder = AudioEncoderFactory::with_opus_frames().unwrap();
        let audio_decoder = AudioDecoderFactory::builtin_opus().unwrap();
        let video_decoder = VideoDecoderFactoryHandle::builtin_vp8().unwrap();
        let builder = || {
            world
                .peer_factory_builder()
                .unwrap()
                .network_manager(first.network_manager().unwrap())
                .packet_socket_factory(first.packet_socket_factory().unwrap())
                .audio_encoder_factory(audio_encoder.clone())
                .audio_decoder_factory(audio_decoder.clone())
                .video_encoder_factory(input.encoder_factory())
                .video_decoder_factory(video_decoder.clone())
        };
        #[cfg(feature = "native")]
        {
            let error = builder()
                .native_audio(true)
                .build()
                .err()
                .expect("encoded carrier admitted with a PCM microphone");
            assert_eq!(error.kind, PeerErrorKind::InvalidParameter);
            assert_eq!(world.next_deadline(), None);
        }
        // These are actual codec/provider selections, not whitelist failures.
        // The SDK remains responsible for their advertised/negotiated support.
        for selected in [
            builder().audio_encoder_factory(AudioEncoderFactory::builtin().unwrap()),
            builder().audio_decoder_factory(AudioDecoderFactory::builtin().unwrap()),
            builder().video_encoder_factory(EncodedVideoInput::new().unwrap().encoder_factory()),
            builder()
                .video_decoder_factory(VideoDecoderFactoryHandle::new(UnavailableVp8).unwrap()),
        ] {
            let factory = selected.build().unwrap();
            drop(factory);
            world.pump(1024);
            assert_eq!(world.now(), Duration::from_secs(10));
            assert_eq!(world.next_deadline(), None);
        }
        // An encoded-packet carrier cannot process captured PCM in any mode.
        let error = builder()
            .audio_processing(AudioProcessingConfig::default())
            .build()
            .err()
            .expect("encoded carrier admitted PCM processing");
        assert_eq!(error.kind, PeerErrorKind::InvalidParameter);
        assert_eq!(world.now(), Duration::from_secs(10));
        assert_eq!(world.next_deadline(), None);
    }
    eprintln!("CONTROLLED_MEDIA_END");
}
