//! One real native peer, one RTC connection, no ongoing server SDP.
use super::*;
use pulsebeam_agent_core::client_sdp as client;
use pulsebeam_rtc::{
    RtpBinding, RtpCapabilities, RtpCodec, RtpCodecParameters, RtpConfiguration,
    RtpConfigurationLimits, RtpEncoding, RtpExtensionMapping,
};
use std::{collections::BTreeMap, num::NonZeroU32};

fn opus() -> RtpCodecParameters {
    RtpCodecParameters {
        name: "opus".into(),
        clock_rate: 48000,
        channels: Some(2),
        fmtp: [
            ("minptime".into(), "10".into()),
            ("useinbandfec".into(), "1".into()),
        ]
        .into_iter()
        .collect(),
        feedback: ["transport-cc".into()].into_iter().collect(),
    }
}
fn video_codec(name: &str) -> RtpCodecParameters {
    RtpCodecParameters {
        name: name.into(),
        clock_rate: 90000,
        channels: None,
        fmtp: match name {
            "h264" => [
                ("packetization-mode".into(), "1".into()),
                ("level-asymmetry-allowed".into(), "1".into()),
                ("profile-level-id".into(), "42e01f".into()),
            ]
            .into_iter()
            .collect(),
            "vp9" => [("profile-id".into(), "0".into())].into_iter().collect(),
            _ => BTreeMap::new(),
        },
        feedback: ["nack", "nack pli", "ccm fir", "transport-cc"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    }
}

fn profile() -> RtpCapabilities {
    RtpCapabilities {
        codecs: vec![
            opus(),
            video_codec("vp8"),
            video_codec("h264"),
            video_codec("vp9"),
        ],
        extensions: [
            pulsebeam_rtc::MID_EXTENSION,
            pulsebeam_rtc::TWCC_EXTENSION,
            pulsebeam_rtc::RID_EXTENSION,
            pulsebeam_rtc::REPAIRED_RID_EXTENSION,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        repair: true,
        feedback: pulsebeam_rtc::PacketFeedbackKind::TransportWide,
        limits: RtpConfigurationLimits::default(),
    }
}
fn client_capabilities(profile: &RtpCapabilities) -> client::Capabilities {
    client::Capabilities {
        codecs: profile
            .codecs
            .iter()
            .map(|p| client::CodecParameters {
                name: p.name.clone(),
                clock_rate: p.clock_rate,
                channels: p.channels,
                fmtp: p.fmtp.clone(),
                feedback: p.feedback.clone(),
            })
            .collect(),
        extensions: profile.extensions.clone(),
        repair: profile.repair,
    }
}
fn binding(value: &client::Binding) -> RtpBinding {
    assert!(
        value.complete(),
        "incomplete runtime sender facts are not RTC admission"
    );
    RtpBinding {
        mid: value.mid.clone(),
        kind: match value.kind {
            client::Kind::Audio => pulsebeam_rtc::MediaKind::Audio,
            client::Kind::Video => pulsebeam_rtc::MediaKind::Video,
            _ => panic!("RTP kind"),
        },
        codec: RtpCodec {
            payload_type: value.codec.payload_type,
            parameters: RtpCodecParameters {
                name: value.codec.parameters.name.clone(),
                clock_rate: value.codec.parameters.clock_rate,
                channels: value.codec.parameters.channels,
                fmtp: value.codec.parameters.fmtp.clone(),
                feedback: value.codec.parameters.feedback.clone(),
            },
            repair_payload_type: value.codec.repair.as_ref().map(|r| r.payload_type),
        },
        extensions: value
            .extensions
            .iter()
            .map(|e| RtpExtensionMapping {
                id: e.id,
                uri: e.uri.clone(),
            })
            .collect(),
        encodings: value
            .encodings
            .iter()
            .map(|e| RtpEncoding {
                primary: e.primary.unwrap(),
                repair: e.repair,
                rid: e.rid.clone(),
                active: e.runtime.active.unwrap_or(!e.paused),
            })
            .collect(),
    }
}
fn configuration(bindings: &client::Bindings) -> RtpConfiguration {
    RtpConfiguration {
        inbound: bindings.publications.iter().map(binding).collect(),
        outbound: bindings.receptions.iter().map(binding).collect(),
    }
}
fn local_offer(world: &ControlledWorld, peer: &PeerConnection) -> SessionDescription {
    let offer = complete(world, peer, peer.create_offer().unwrap()).unwrap();
    complete(world, peer, peer.set_local_description(offer).unwrap());
    for _ in 0..2000 {
        world.pump(512);
        if peer.ice_gathering_state() == IceGatheringState::Complete {
            return peer.descriptions().unwrap().pending_local.unwrap();
        }
        world.advance(Duration::from_millis(1)).unwrap();
    }
    panic!("ICE gathering stalled")
}

struct Pump<'a> {
    world: &'a ControlledWorld,
    network: &'a ControlledSimulatedNetwork,
    socket: &'a SimulatedUdpSocket,
    connection: Connection,
    start: Instant,
    origin: Duration,
    echo_mids: BTreeMap<String, String>,
    echo_sequences: BTreeMap<String, u64>,
    output_sources: BTreeMap<String, Vec<u8>>,
    received: Vec<(String, pulsebeam_rtc::RtpPacket)>,
    data: Vec<pulsebeam_rtc::DataMessage>,
    feedback: usize,
    connected: bool,
    drop_primary: Option<(u8, u32)>,
    dropped_primary: Option<WireRtp>,
    repair_packets: Vec<WireRtp>,
}
impl Pump<'_> {
    fn now(&self) -> Instant {
        self.start + self.world.now().saturating_sub(self.origin)
    }
    fn drive(&mut self, millis: u32) {
        for _ in 0..millis {
            self.world.pump(512);
            while let Some(packet) = self.network.next_packet() {
                if packet.source == self.socket.local_address()
                    && let Some(header) = wire_rtp(&packet.payload)
                    && self.drop_primary == Some((header.payload_type, header.ssrc))
                {
                    self.drop_primary = None;
                    self.dropped_primary = Some(header);
                    self.network.drop_packet(packet.id).unwrap();
                    continue;
                }
                if packet.destination == self.socket.local_address() && is_twcc(&packet.payload) {
                    self.feedback += 1;
                }
                match self.network.deliver(packet.id) {
                    Ok(()) => {}
                    Err(NetworkError::DestinationUnavailable)
                        if packet.source == self.socket.local_address()
                            && packet.payload.get(4..8) == Some(&[0x21, 0x12, 0xa4, 0x42][..]) => {}
                    Err(error) => panic!("virtual delivery: {error:?}"),
                }
            }
            for _ in 0..16 {
                let Some(packet) = self.socket.try_receive() else {
                    break;
                };
                let local = self.socket.local_address();
                self.connection
                    .receive(
                        self.now(),
                        self.now(),
                        NetworkInput::Udp {
                            local: SocketAddr::new(local.ip(), local.port()),
                            remote: SocketAddr::new(packet.source.ip(), packet.source.port()),
                            ecn: None,
                            payload: packet.payload.into(),
                        },
                    )
                    .unwrap();
            }
            for _ in 0..512 {
                match self.connection.poll(self.now()) {
                    Output::Transmit(packet) => {
                        if matches!(packet.kind, pulsebeam_rtc::TransmitKind::Repair { .. }) {
                            self.repair_packets
                                .push(wire_rtp(&packet.payload).expect("repair RTP header"));
                        }
                        let TransmitTarget::Udp { remote, .. } = packet.target else {
                            panic!("UDP")
                        };
                        self.socket
                            .send_to(
                                NetworkAddress::new(remote.ip(), remote.port()).unwrap(),
                                packet.payload.to_vec(),
                            )
                            .unwrap();
                    }
                    Output::Event(Event::Connected) => self.connected = true,
                    Output::Event(Event::Rtp { mid, packet, .. }) => {
                        if let Some(output) = self.echo_mids.get(mid.as_ref()) {
                            let sequence = self.echo_sequences.entry(output.clone()).or_default();
                            *sequence += 1;
                            let sequence = *sequence;
                            self.connection
                                .write_rtp(
                                    self.now(),
                                    output,
                                    RtpWrite {
                                        sequence,
                                        timestamp: packet.header.timestamp,
                                        wallclock: packet.arrival,
                                        marker: packet.header.marker,
                                        payload: self
                                            .output_sources
                                            .get(mid.as_ref())
                                            .cloned()
                                            .map(Into::into)
                                            .unwrap_or_else(|| packet.payload.clone()),
                                        extensions: vec![],
                                    },
                                )
                                .unwrap();
                        }
                        self.received.push((mid.to_string(), packet));
                    }
                    Output::Event(Event::DataChannel(
                        pulsebeam_rtc::DataChannelEvent::Message { channel, message },
                    )) => {
                        self.data.push(message.clone());
                        self.connection
                            .send_data(self.now(), channel, message)
                            .unwrap();
                    }
                    Output::Event(_) => {}
                    Output::Idle { next_wakeup } => {
                        if next_wakeup.is_some_and(|deadline| deadline <= self.now()) {
                            continue;
                        }
                        break;
                    }
                    Output::Closed(reason) => panic!("RTC closed: {reason:?}"),
                    _ => panic!("unknown output"),
                }
            }
            self.world.advance(Duration::from_millis(1)).unwrap();
        }
    }
    fn audio(&mut self, source: &EncodedAudioSource, frames: u32) {
        for _ in 0..frames {
            let timestamp = u32::try_from(self.world.now().as_millis())
                .unwrap()
                .wrapping_mul(48);
            source
                .push_opus_at(
                    &OpusInputFrame {
                        data: vec![0xf8, 0xff, 0xfe],
                        rtp_timestamp: timestamp,
                        samples_per_channel: 960,
                    },
                    self.world.now(),
                )
                .unwrap();
            self.drive(20);
        }
        self.drive(50);
    }
}

#[test]
fn sctp_bootstrap_reconciles_dynamic_media_and_repair_without_server_sdp() {
    let world = ControlledWorld::acquire(37, Duration::from_secs(10)).unwrap();
    let network = world.create_network().unwrap();
    let client_endpoint = network
        .register_endpoint(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)))
        .unwrap();
    let server_endpoint = network
        .register_endpoint(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)))
        .unwrap();
    let socket = server_endpoint.bind_udp(41000).unwrap();
    let factory = world
        .peer_factory_builder()
        .unwrap()
        .network_manager(client_endpoint.network_manager().unwrap())
        .packet_socket_factory(client_endpoint.packet_socket_factory().unwrap())
        .audio_encoder_factory(AudioEncoderFactory::with_opus_frames().unwrap())
        .audio_decoder_factory(AudioDecoderFactory::builtin_opus().unwrap())
        .video_encoder_factory(VideoEncoderFactoryHandle::builtin().unwrap())
        .video_decoder_factory(VideoDecoderFactoryHandle::builtin().unwrap())
        .build()
        .unwrap();
    let mut peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let channel = peer
        .create_data_channel("continuity", DataChannelConfiguration::default())
        .unwrap();
    let bootstrap = local_offer(&world, &peer);
    assert!(
        client::inspect(&bootstrap.sdp)
            .unwrap()
            .iter()
            .all(|m| m.kind == client::Kind::Application)
    );
    let local = socket.local_address();
    let accepted = Connection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(SocketAddr::new(
                local.ip(),
                local.port(),
            ))],
            ..ConnectionConfig::default()
        },
        SdpOffer::new(bootstrap.sdp.clone()),
        Instant::now(),
        std::time::UNIX_EPOCH,
        ConnectionEntropy::new([37; 32]),
    )
    .unwrap();
    assert!(accepted.session.inbound_media.is_empty() && accepted.session.senders.is_empty());
    let server_profile = profile();
    let mut context = client::Context::new(
        &bootstrap.sdp,
        accepted.answer.as_str(),
        client_capabilities(&server_profile),
        3000,
        client::Limits::default(),
    )
    .unwrap();
    complete(
        &world,
        &peer,
        peer.set_remote_description(SessionDescription {
            kind: SessionDescriptionType::Answer,
            sdp: accepted.answer.as_str().into(),
        })
        .unwrap(),
    );
    let mut pump = Pump {
        world: &world,
        network: &network,
        socket: &socket,
        connection: accepted.connection,
        start: Instant::now(),
        origin: world.now(),
        echo_mids: BTreeMap::new(),
        echo_sequences: BTreeMap::new(),
        output_sources: BTreeMap::new(),
        received: vec![],
        data: vec![],
        feedback: 0,
        connected: false,
        drop_primary: None,
        dropped_primary: None,
        repair_packets: vec![],
    };
    pump.connection
        .set_rtp_capabilities(server_profile)
        .unwrap();
    pump.drive(500);
    assert!(pump.connected && channel.state() == DataChannelState::Open);
    assert_eq!(
        channel.send(DataChannelMessage::text("before media")),
        DataChannelSendResult::Sent
    );
    pump.drive(100);
    assert_eq!(pump.data.len(), 1);
    let source = factory.create_encoded_audio_source(1).unwrap();
    let track = factory
        .create_encoded_audio_track("dynamic-opus", &source)
        .unwrap();
    let transceiver = peer
        .add_audio_transceiver(&track, RtpTransceiverDirection::SendReceive)
        .unwrap();
    let mono = peer
        .audio_sender_capabilities()
        .unwrap()
        .into_iter()
        .find(|codec| {
            codec.name().eq_ignore_ascii_case("opus")
                && codec
                    .parameters()
                    .iter()
                    .any(|p| p.key == "stereo" && p.value == "0")
        })
        .unwrap();
    transceiver.set_audio_codec_preferences(&[mono]).unwrap();
    let offer = local_offer(&world, &peer);
    let media = client::inspect(&offer.sdp)
        .unwrap()
        .into_iter()
        .find(|m| m.kind == client::Kind::Audio)
        .unwrap();
    let selected = media
        .codecs
        .iter()
        .find(|c| c.parameters.name == "opus")
        .unwrap();
    assert_eq!(selected.parameters.fmtp, opus().fmtp);
    let owner = |key: &str| {
        Some(client::MediaOwner {
            key: client::MediaKey(key.into()),
            group: None,
        })
    };
    let candidate = context
        .prepare(
            &offer.sdp,
            &[client::Preparation {
                mid: media.mid.clone(),
                payload_type: selected.payload_type,
                publication: owner("microphone"),
                reception: owner("playback"),
            }],
        )
        .unwrap();
    complete(
        &world,
        &peer,
        peer.set_remote_description(SessionDescription {
            kind: SessionDescriptionType::Answer,
            sdp: candidate.answer().into(),
        })
        .unwrap(),
    );
    context
        .finish(candidate.id(), client::ApplyOutcome::Applied)
        .unwrap();
    let sink = transceiver.receiver().attach_encoded_audio_sink().unwrap();
    pump.audio(&source, 5);
    assert!(
        pump.received.is_empty(),
        "sender traffic before declaration is not admission"
    );
    let mut observed = context.bindings().unwrap().publications[0]
        .encodings
        .clone();
    observed[0].primary = transceiver.sender().get_parameters().encodings()[0]
        .ssrc()
        .and_then(NonZeroU32::new);
    context.observe_sender(&media.mid, &observed).unwrap();
    let configuration = configuration(context.bindings().unwrap());
    pump.connection
        .configure_rtp(configuration.clone())
        .unwrap();
    pump.echo_mids.insert(media.mid.clone(), media.mid.clone());
    pump.audio(&source, 10);
    assert!(pump.received.len() >= 10);
    assert!(pump.received.iter().all(|(mid, p)| mid == &media.mid
        && p.header.ssrc == configuration.inbound[0].encodings[0].primary.get()
        && p.header.payload_type == selected.payload_type
        && p.payload.as_ref() == [0xf8, 0xff, 0xfe]));
    let mut decoded = 0;
    while let Some(frame) = sink.try_next_frame() {
        assert_eq!(frame.data, [0xf8, 0xff, 0xfe]);
        decoded += 1;
    }
    assert!(decoded >= 10, "attributable native receive payloads");
    let mut invalid = configuration.clone();
    invalid.outbound[0].encodings[0].primary = invalid.inbound[0].encodings[0].primary;
    assert!(pump.connection.configure_rtp(invalid).is_err());
    assert_eq!(pump.connection.rtp_configuration(), Some(&configuration));
    pump.echo_mids.remove(&media.mid);
    let mut withdrawn = configuration.clone();
    withdrawn.outbound.clear();
    pump.connection.configure_rtp(withdrawn).unwrap();
    pump.drive(100);
    while sink.try_next_frame().is_some() {}
    let before = pump.received.len();
    pump.audio(&source, 5);
    assert!(pump.received.len() > before && sink.try_next_frame().is_none());
    assert_eq!(
        channel.send(DataChannelMessage::text("during withdrawal")),
        DataChannelSendResult::Sent
    );
    pump.drive(100);
    assert_eq!(pump.data.len(), 2);
    pump.connection
        .configure_rtp(configuration.clone())
        .unwrap();
    pump.echo_mids.insert(media.mid.clone(), media.mid.clone());
    pump.output_sources
        .insert(media.mid.clone(), vec![0xf8, 0xff, 0xfe, 0]);
    pump.audio(&source, 5);
    let mut switched = 0;
    while let Some(frame) = sink.try_next_frame() {
        assert_eq!(frame.data, [0xf8, 0xff, 0xfe, 0]);
        switched += 1;
    }
    assert!(
        switched > 0,
        "another compatible fixture source uses the same output binding"
    );
    assert_eq!(pump.connection.rtp_configuration(), Some(&configuration));
    assert!(pump.feedback > 0);
    let stats = pump.connection.stats();
    assert_eq!(stats.senders.len(), 1);
    assert_eq!(
        stats.senders[0].ssrc,
        configuration.outbound[0].encodings[0].primary.get()
    );
    assert!(stats.connection.rtp_bytes_in_flight < stats.connection.transmitted_rtp_bytes);
    assert_eq!(pump.data.len(), 2);
    let mut echoed = Vec::new();
    while let Some(event) = channel.try_next_event() {
        if let DataChannelEvent::Message(message) = event {
            echoed.push(message);
        }
    }
    assert_eq!(
        echoed,
        vec![
            DataChannelMessage::text("before media"),
            DataChannelMessage::text("during withdrawal")
        ]
    );
    let feedback_before_video = pump.feedback;
    video_cycles(
        &world,
        &peer,
        &factory,
        &mut context,
        &mut pump,
        &source,
        &configuration,
    );
    encoding_cycles(
        &world,
        &peer,
        &factory,
        &mut context,
        &mut pump,
        &source,
        &configuration,
    );
    assert!(pump.feedback > feedback_before_video);
    let final_message = DataChannelMessage::text("after encodings and repair");
    assert_eq!(
        channel.send(final_message.clone()),
        DataChannelSendResult::Sent
    );
    pump.drive(100);
    let mut echoed = Vec::new();
    while let Some(event) = channel.try_next_event() {
        if let DataChannelEvent::Message(message) = event {
            echoed.push(message);
        }
    }
    assert_eq!(echoed, vec![final_message]);
    assert_eq!(pump.data.len(), 3);
    let mut reference_frames = 0;
    while let Some(frame) = sink.try_next_frame() {
        assert_eq!(frame.data, [0xf8, 0xff, 0xfe, 0]);
        reference_frames += 1;
    }
    assert!(
        reference_frames > 0,
        "reference source continues through video and encoding updates"
    );
    peer.close().unwrap();
}

fn reconcile(
    world: &ControlledWorld,
    peer: &PeerConnection,
    context: &mut client::Context,
    pump: &mut Pump<'_>,
    offer: &str,
    requests: &[client::Preparation],
) -> client::Bindings {
    let candidate = context.prepare(offer, requests).unwrap();
    complete(
        world,
        peer,
        peer.set_remote_description(SessionDescription {
            kind: SessionDescriptionType::Answer,
            sdp: candidate.answer().into(),
        })
        .unwrap(),
    );
    context
        .finish(candidate.id(), client::ApplyOutcome::Applied)
        .unwrap();
    let bindings = context.bindings().unwrap().clone();
    pump.connection
        .configure_rtp(configuration(&bindings))
        .unwrap();
    bindings
}

fn video_frames(
    source: &VideoSource,
    audio: &EncodedAudioSource,
    pump: &mut Pump<'_>,
    value: u8,
    count: u32,
) {
    video_frames_sized(source, audio, pump, value, count, 64, 64);
}

fn video_frames_sized(
    source: &VideoSource,
    audio: &EncodedAudioSource,
    pump: &mut Pump<'_>,
    value: u8,
    count: u32,
    width: u32,
    height: u32,
) {
    for _ in 0..count {
        let area = usize::try_from(width * height).unwrap();
        let mut pixels = vec![value; area];
        pixels.resize(area * 3 / 2, 128);
        source
            .push_frame(
                &VideoFrame::i420(
                    width,
                    height,
                    pixels,
                    i64::try_from(pump.world.now().as_micros()).unwrap(),
                    u32::try_from(pump.world.now().as_millis())
                        .unwrap()
                        .wrapping_mul(90),
                )
                .unwrap(),
            )
            .unwrap();
        pump.audio(audio, 1);
        pump.drive(13);
    }
    pump.drive(200);
}

fn assert_pixels(sink: &VideoSink, value: u8) -> usize {
    let mut received = 0;
    while let Some(frame) = sink.try_next_frame() {
        let y = usize::try_from(frame.width * frame.height).unwrap();
        let pixels = frame.buffer.as_bytes();
        let mean =
            pixels.iter().take(y).map(|p| u64::from(*p)).sum::<u64>() / u64::try_from(y).unwrap();
        assert!(
            mean.abs_diff(u64::from(value)) <= 8,
            "decoded source attribution: expected {value}, got {mean}"
        );
        received += 1;
    }
    received
}

#[derive(Clone, Copy, Debug)]
struct WireRtp {
    payload_type: u8,
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
}
fn wire_rtp(payload: &[u8]) -> Option<WireRtp> {
    if payload.len() < 12 || payload[0] >> 6 != 2 {
        return None;
    }
    Some(WireRtp {
        payload_type: payload[1] & 127,
        ssrc: u32::from_be_bytes(payload[8..12].try_into().unwrap()),
        sequence: u16::from_be_bytes(payload[2..4].try_into().unwrap()),
        timestamp: u32::from_be_bytes(payload[4..8].try_into().unwrap()),
    })
}

fn isolated_frame_repair(
    source: &VideoSource,
    audio: &EncodedAudioSource,
    pump: &mut Pump<'_>,
    upstream: u32,
    output: &RtpBinding,
    sink: &VideoSink,
) -> WireRtp {
    let repair_ssrc = output.encodings[0].repair.unwrap().get();
    let repair_pt = output.codec.repair_payload_type.unwrap();
    let primary_ssrc = output.encodings[0].primary.get();
    pump.connection.request_keyframe(upstream).unwrap();
    pump.drive(20);
    let received_start = pump.received.len();
    let repair_start = pump.repair_packets.len();
    pump.dropped_primary = None;
    pump.drop_primary = Some((output.codec.payload_type, primary_ssrc));
    let mut noise = 0x9e37_79b9u32;
    let mut pixels: Vec<_> = (0..128 * 128)
        .map(|_| {
            noise ^= noise << 13;
            noise ^= noise >> 17;
            noise ^= noise << 5;
            (noise >> 24) as u8
        })
        .collect();
    pixels.resize(128 * 128 * 3 / 2, 128);
    source
        .push_frame(
            &VideoFrame::i420(
                128,
                128,
                pixels,
                i64::try_from(pump.world.now().as_micros()).unwrap(),
                u32::try_from(pump.world.now().as_millis())
                    .unwrap()
                    .wrapping_mul(90),
            )
            .unwrap(),
        )
        .unwrap();
    // No later video input is permitted before recovery. Delivering later
    // fragments of this same real keyframe exposes the missing first packet.
    let deadline = pump.world.now() + Duration::from_secs(2);
    let mut recovered = 0;
    while pump.world.now() < deadline && recovered == 0 {
        pump.audio(audio, 1);
        recovered += assert_pixels(sink, 128);
    }
    let dropped = pump
        .dropped_primary
        .expect("one real primary packet was lost");
    let packets: Vec<_> = pump.received[received_start..]
        .iter()
        .filter(|(mid, _)| mid == &output.mid)
        .collect();
    assert!(
        packets.len() > 1,
        "the isolated real frame must be fragmented: payloads={:?}",
        packets
            .iter()
            .map(|(_, packet)| packet.payload.len())
            .collect::<Vec<_>>()
    );
    assert!(
        packets
            .iter()
            .all(|(_, packet)| packet.header.timestamp == dropped.timestamp),
        "no later primary frame can substitute for loss recovery"
    );
    assert!(
        recovered > 0,
        "source-attributable native decode requires the dropped first fragment"
    );
    let repair = pump.repair_packets[repair_start..]
        .iter()
        .find(|packet| packet.ssrc == repair_ssrc)
        .expect("native NACK caused a real negotiated RTX repair, not padding");
    assert_eq!(repair.payload_type, repair_pt);
    assert_ne!(repair.ssrc, primary_ssrc);
    *repair
}

fn observed_encodings(
    peer: &PeerConnection,
    pump: &mut Pump<'_>,
    sender: &RtpSender,
    binding: &client::Binding,
) -> Vec<client::Encoding> {
    let parameters = sender.parameters().unwrap();
    let request = peer.request_sender_stats(sender).unwrap();
    for _ in 0..2_000 {
        pump.drive(1);
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::Stats(snapshot) = event {
                assert_eq!(snapshot.operation_id, request);
                let values: Vec<_> = snapshot
                    .report
                    .of_class(RtcStatsClass::OutboundRtpStreamStats)
                    .into_iter()
                    .filter_map(|entry| match entry.values() {
                        RtcStatsValues::OutboundRtpStreamStats(value) => Some(value),
                        _ => None,
                    })
                    .collect();
                return binding
                    .encodings
                    .iter()
                    .map(|encoding| {
                        let stats = values
                            .iter()
                            .find(|value| {
                                value.mid.as_deref() == Some(binding.mid.as_bytes())
                                    && value.rid.as_deref()
                                        == encoding.rid.as_deref().map(str::as_bytes)
                            })
                            .expect("actual native MID/RID outbound stats, not inferred SSRCs");
                        let runtime = parameters
                            .encodings
                            .iter()
                            .find(|value| {
                                (!value.rid.is_empty()).then_some(value.rid.as_str())
                                    == encoding.rid.as_deref()
                            })
                            .unwrap();
                        let mut observed = encoding.clone();
                        observed.primary = stats.ssrc.and_then(NonZeroU32::new);
                        observed.repair = if binding.codec.repair.is_some() {
                            stats.rtx_ssrc.and_then(NonZeroU32::new)
                        } else {
                            None
                        };
                        observed.runtime = client::RuntimeEncoding {
                            active: Some(runtime.active),
                            max_bitrate: runtime.max_bitrate_bps.map(u64::from),
                            max_framerate: runtime.max_framerate,
                            scalability_mode: runtime.scalability_mode.clone(),
                        };
                        assert!(observed.primary.is_some());
                        assert_eq!(
                            observed.repair.is_some(),
                            binding.codec.repair.is_some(),
                            "required actual RTX identity is unavailable"
                        );
                        observed
                    })
                    .collect();
            }
        }
    }
    panic!("sender-scoped stats did not complete");
}

fn publication_passage(pump: &Pump<'_>, start: usize, binding: &RtpBinding) {
    for encoding in &binding.encodings {
        let packets: Vec<_> = pump.received[start..]
            .iter()
            .filter(|(mid, packet)| {
                mid == &binding.mid && packet.header.ssrc == encoding.primary.get()
            })
            .collect();
        if encoding.active {
            assert!(
                !packets.is_empty(),
                "configured encoding has actual traffic: {} {:?}",
                binding.mid,
                encoding.rid
            );
            assert!(
                packets
                    .iter()
                    .all(|(_, packet)| packet.header.payload_type == binding.codec.payload_type)
            );
        } else {
            assert!(
                packets.is_empty(),
                "paused encoding has no admitted packets"
            );
        }
    }
}

fn encoding_cycles(
    world: &ControlledWorld,
    peer: &PeerConnection,
    factory: &PeerConnectionFactory,
    context: &mut client::Context,
    pump: &mut Pump<'_>,
    audio: &EncodedAudioSource,
    reference: &RtpConfiguration,
) {
    let codecs: Vec<_> = peer
        .video_sender_capabilities()
        .unwrap()
        .into_iter()
        .filter(|codec| {
            codec.format().name == "VP8" || codec.format().name.eq_ignore_ascii_case("rtx")
        })
        .collect();
    let mut sources = Vec::new();
    let mut transceivers = Vec::new();
    for index in 0..3 {
        let source = factory.create_video_source().unwrap();
        let track = factory
            .create_video_track(&format!("encoding-{index}"), &source)
            .unwrap();
        let rids = if index == 2 {
            vec!["f".into(), "h".into()]
        } else {
            vec![]
        };
        let transceiver = peer
            .add_video_transceiver_with_rids(&track, RtpTransceiverDirection::SendOnly, &rids)
            .unwrap();
        transceiver.set_codec_preferences(&codecs).unwrap();
        sources.push(source);
        transceivers.push(transceiver);
    }
    peer.set_bitrate(Some(3_000_000), Some(3_000_000), Some(5_000_000))
        .unwrap();
    let offer = local_offer(world, peer);
    let sections = client::inspect(&offer.sdp).unwrap();
    let requests: Vec<_> = transceivers
        .iter()
        .enumerate()
        .map(|(index, transceiver)| {
            let mid = transceiver.mid().unwrap();
            let section = sections.iter().find(|section| section.mid == mid).unwrap();
            client::Preparation {
                mid,
                payload_type: section
                    .codecs
                    .iter()
                    .find(|codec| codec.parameters.name == "vp8")
                    .unwrap()
                    .payload_type,
                publication: Some(client::MediaOwner {
                    key: client::MediaKey(format!("encoding-{index}")),
                    group: Some(client::MediaKey(
                        if index == 2 {
                            "rid-quality"
                        } else {
                            "independent-tiers"
                        }
                        .into(),
                    )),
                }),
                reception: None,
            }
        })
        .collect();
    let candidate = context.prepare(&offer.sdp, &requests).unwrap();
    let rid_mid = transceivers[2].mid().unwrap();
    let rid_binding = candidate
        .bindings()
        .publications
        .iter()
        .find(|binding| binding.mid == rid_mid)
        .unwrap()
        .clone();
    assert_eq!(rid_binding.encodings.len(), 2);
    assert!(
        !rid_binding.complete(),
        "RID preparation is not sender-identity admission"
    );
    complete(
        world,
        peer,
        peer.set_remote_description(SessionDescription {
            kind: SessionDescriptionType::Answer,
            sdp: candidate.answer().into(),
        })
        .unwrap(),
    );
    context
        .finish(candidate.id(), client::ApplyOutcome::Applied)
        .unwrap();
    let before_declaration = pump.received.len();
    for (index, source) in sources.iter().enumerate() {
        video_frames_sized(source, audio, pump, 70 + index as u8 * 50, 8, 640, 360);
    }
    assert!(
        pump.received[before_declaration..]
            .iter()
            .all(|(mid, _)| mid == &reference.inbound[0].mid),
        "native discovery traffic is not implicitly admitted"
    );
    let observations = observed_encodings(peer, pump, &transceivers[2].sender(), &rid_binding);
    context.observe_sender(&rid_mid, &observations).unwrap();
    let accepted = context.bindings().unwrap().clone();
    assert!(accepted.publications.iter().all(client::Binding::complete));
    let tiers: Vec<_> = accepted
        .publications
        .iter()
        .filter(|binding| binding.group == Some(client::MediaKey("independent-tiers".into())))
        .collect();
    assert_eq!(tiers.len(), 2);
    assert_ne!(tiers[0].encodings[0].primary, tiers[1].encodings[0].primary);
    let declared = configuration(&accepted);
    pump.connection.configure_rtp(declared.clone()).unwrap();
    let start = pump.received.len();
    for (index, source) in sources.iter().enumerate() {
        video_frames_sized(source, audio, pump, 70 + index as u8 * 50, 8, 640, 360);
    }
    for binding in declared
        .inbound
        .iter()
        .filter(|binding| binding.mid != reference.inbound[0].mid)
    {
        publication_passage(pump, start, binding);
    }
    let rid_config = declared
        .inbound
        .iter()
        .find(|binding| binding.mid == rid_mid)
        .unwrap();
    for encoding in &rid_config.encodings {
        assert!(
            pump.received[start..].iter().any(|(mid, packet)| {
                mid == &rid_mid
                    && packet.header.ssrc == encoding.primary.get()
                    && packet.extensions.iter().any(|extension| {
                        extension.uri.as_ref() == pulsebeam_rtc::RID_EXTENSION
                            && extension.value.as_ref() == encoding.rid.as_ref().unwrap().as_bytes()
                    })
            }),
            "actual negotiated RID extension matches its observed primary"
        );
    }
    let unchanged_descriptions = peer.descriptions().unwrap();
    for active in [false, true] {
        let sender = transceivers[2].sender();
        let mut parameters = sender.parameters().unwrap();
        parameters.encodings[0].active = active;
        parameters.encodings[0].max_bitrate_bps = Some(400_000);
        sender.set_parameters(parameters).unwrap();
        let current = context
            .bindings()
            .unwrap()
            .publications
            .iter()
            .find(|binding| binding.mid == rid_mid)
            .unwrap()
            .clone();
        let parameters = sender.parameters().unwrap();
        let mut observed = current.encodings.clone();
        for encoding in &mut observed {
            let runtime = parameters
                .encodings
                .iter()
                .find(|runtime| Some(runtime.rid.as_str()) == encoding.rid.as_deref())
                .unwrap();
            encoding.runtime.active = Some(runtime.active);
            encoding.runtime.max_bitrate = runtime.max_bitrate_bps.map(u64::from);
        }
        context.observe_sender(&rid_mid, &observed).unwrap();
        let current = configuration(context.bindings().unwrap());
        pump.connection.configure_rtp(current.clone()).unwrap();
        pump.audio(audio, 2);
        let start = pump.received.len();
        video_frames_sized(&sources[2], audio, pump, 170, 8, 640, 360);
        let rid = current
            .inbound
            .iter()
            .find(|binding| binding.mid == rid_mid)
            .unwrap();
        publication_passage(pump, start, rid);
        for (old, new) in rid_config.encodings.iter().zip(&rid.encodings) {
            assert_eq!(
                (old.primary, old.repair, &old.rid),
                (new.primary, new.repair, &new.rid)
            );
        }
        let descriptions = peer.descriptions().unwrap();
        assert_eq!(
            descriptions.current_local,
            unchanged_descriptions.current_local
        );
        assert_eq!(
            descriptions.current_remote,
            unchanged_descriptions.current_remote
        );
    }
    for transceiver in &transceivers {
        transceiver.stop().unwrap();
    }
    let offer = local_offer(world, peer);
    let retired = reconcile(world, peer, context, pump, &offer.sdp, &[]);
    assert_eq!(configuration(&retired), *reference);
}

fn video_cycles(
    world: &ControlledWorld,
    peer: &PeerConnection,
    factory: &PeerConnectionFactory,
    context: &mut client::Context,
    pump: &mut Pump<'_>,
    audio: &EncodedAudioSource,
    reference: &RtpConfiguration,
) {
    let mut previous_primary = None;
    for (index, name) in ["VP8", "H264", "VP9"].into_iter().enumerate() {
        let source = factory.create_video_source().unwrap();
        let track = factory
            .create_video_track(&format!("video-{index}"), &source)
            .unwrap();
        let transceiver = peer
            .add_video_transceiver(&track, RtpTransceiverDirection::SendReceive)
            .unwrap();
        let codecs: Vec<_> =
            peer.video_sender_capabilities()
                .unwrap()
                .into_iter()
                .filter(|codec| {
                    let format = codec.format();
                    format.name.eq_ignore_ascii_case("rtx")
                        || (format.name == name
                            && (name != "H264"
                                || format
                                    .parameters
                                    .iter()
                                    .any(|p| p.key == "packetization-mode" && p.value == "1")
                                    && format.parameters.iter().any(|p| {
                                        p.key == "profile-level-id" && p.value == "42e01f"
                                    }))
                            && (name != "VP9"
                                || format
                                    .parameters
                                    .iter()
                                    .any(|p| p.key == "profile-id" && p.value == "0")))
                })
                .collect();
        assert!(!codecs.is_empty(), "native {name} capability");
        transceiver.set_codec_preferences(&codecs).unwrap();
        let offer = local_offer(world, peer);
        let media = client::inspect(&offer.sdp)
            .unwrap()
            .into_iter()
            .find(|m| m.kind == client::Kind::Video && !m.stopped)
            .unwrap();
        let selected = media
            .codecs
            .iter()
            .find(|c| c.parameters.name.eq_ignore_ascii_case(name))
            .unwrap();
        let owner = |suffix: &str| {
            Some(client::MediaOwner {
                key: client::MediaKey(format!("video-{index}-{suffix}")),
                group: Some(client::MediaKey("logical-video".into())),
            })
        };
        let bindings = reconcile(
            world,
            peer,
            context,
            pump,
            &offer.sdp,
            &[client::Preparation {
                mid: media.mid.clone(),
                payload_type: selected.payload_type,
                publication: owner("publication"),
                reception: owner("output"),
            }],
        );
        let current = configuration(&bindings);
        assert_eq!(
            current.inbound[0], reference.inbound[0],
            "reference publication remains intact"
        );
        assert_eq!(
            current.outbound[0], reference.outbound[0],
            "reference output remains intact"
        );
        let output = current
            .outbound
            .iter()
            .find(|b| b.mid == media.mid)
            .unwrap();
        let primary = output.encodings[0].primary;
        assert_ne!(
            Some(primary),
            previous_primary,
            "fresh codec section must use fresh protection identity"
        );
        previous_primary = Some(primary);
        let video_sink = transceiver
            .receiver()
            .track()
            .unwrap()
            .attach_sink()
            .unwrap();
        pump.echo_mids.insert(media.mid.clone(), media.mid.clone());
        let value = 48 + u8::try_from(index).unwrap() * 64;
        let reference_before = pump
            .received
            .iter()
            .filter(|(mid, _)| mid == &reference.inbound[0].mid)
            .count();
        video_frames(&source, audio, pump, value, 8);
        assert!(
            assert_pixels(&video_sink, value) > 0,
            "real {name} primary passage through RTC and native decoding"
        );
        assert!(pump.received.iter().any(|(mid, packet)| mid == &media.mid
            && packet.header.payload_type == selected.payload_type));
        assert!(
            pump.received
                .iter()
                .filter(|(mid, _)| mid == &reference.inbound[0].mid)
                .count()
                > reference_before
        );
        if index == 0 {
            let upstream = current
                .inbound
                .iter()
                .find(|binding| binding.mid == media.mid)
                .unwrap()
                .encodings[0]
                .primary
                .get();
            let first_repair =
                isolated_frame_repair(&source, audio, pump, upstream, output, &video_sink);
            transceiver
                .set_direction(RtpTransceiverDirection::Inactive)
                .unwrap();
            let offer = local_offer(world, peer);
            reconcile(world, peer, context, pump, &offer.sdp, &[]);
            pump.echo_mids.remove(&media.mid);
            pump.drive(100);
            while video_sink.try_next_frame().is_some() {}
            pump.audio(audio, 3);
            assert!(video_sink.try_next_frame().is_none());
            transceiver
                .set_direction(RtpTransceiverDirection::SendOnly)
                .unwrap();
            let offer = local_offer(world, peer);
            reconcile(world, peer, context, pump, &offer.sdp, &[]);
            let before = pump.received.len();
            video_frames(&source, audio, pump, value, 3);
            assert!(pump.received.len() > before && video_sink.try_next_frame().is_none());
            transceiver
                .set_direction(RtpTransceiverDirection::SendReceive)
                .unwrap();
            let offer = local_offer(world, peer);
            let readded = reconcile(world, peer, context, pump, &offer.sdp, &[]);
            assert_eq!(
                readded
                    .receptions
                    .iter()
                    .find(|b| b.mid == media.mid)
                    .unwrap()
                    .encodings[0]
                    .primary,
                Some(primary)
            );
            pump.echo_mids.insert(media.mid.clone(), media.mid.clone());
            let publication = readded
                .publications
                .iter()
                .find(|b| b.mid == media.mid)
                .unwrap();
            pump.connection
                .request_keyframe(publication.encodings[0].primary.unwrap().get())
                .unwrap();
            pump.drive(20);
            video_frames(&source, audio, pump, value, 4);
            assert!(
                assert_pixels(&video_sink, value) > 0,
                "duplex re-admission retains native receive continuity"
            );
            let next_repair =
                isolated_frame_repair(&source, audio, pump, upstream, output, &video_sink);
            let repair_delta = next_repair.sequence.wrapping_sub(first_repair.sequence);
            assert!(
                repair_delta > 0 && repair_delta < 32768,
                "exact-binding re-admission retains independent RTX numbering"
            );
        }
        transceiver.stop().unwrap();
        let offer = local_offer(world, peer);
        let stopped = reconcile(world, peer, context, pump, &offer.sdp, &[]);
        assert!(
            !stopped
                .publications
                .iter()
                .chain(&stopped.receptions)
                .any(|b| b.mid == media.mid)
        );
        pump.echo_mids.remove(&media.mid);
        pump.audio(audio, 2);
    }
}
