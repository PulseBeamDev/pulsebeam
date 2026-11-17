//! Native simulcast uplink into one production receiver-only RTC connection.
use super::*;
use pulsebeam_rtc as rtc;

#[path = "rtp_video.rs"]
mod rtp_video;

struct Uplink {
    peer: PeerConnection,
    source: VideoSource,
    sender: RtpSender,
    connection: Option<rtc::RtcConnection>,
    socket: SimulatedUdpSocket,
    local: SocketAddr,
    network: ControlledSimulatedNetwork,
    operations: Vec<OperationCompletion>,
    stats: Vec<PeerStatsSnapshot>,
    gathered: bool,
    connected: bool,
    incoming: Vec<(String, rtc::RtpPacket)>,
    mid: String,
    negotiation: Vec<String>,
    control_outputs: u64,
    link: Link<u64>,
    epoch_ms: u64,
    origin: Instant,
    scream_feedback: std::sync::mpsc::Receiver<()>,
    _track: VideoTrack,
    _factory: PeerConnectionFactory,
    _endpoints: Vec<NetworkEndpoint>,
    world: ControlledWorld,
}

impl Uplink {
    #[cfg(test)]
    fn new(controller: NativeController) -> Self {
        Self::with_start(controller, 1, 2_000_000)
    }
    fn with_start(controller: NativeController, seed: u64, initial_bps: u32) -> Self {
        let world = ControlledWorld::acquire(seed, Duration::from_secs(10)).unwrap();
        let network = world.create_network().unwrap();
        let endpoints: Vec<_> = [Ipv4Addr::new(10, 79, 0, 1), Ipv4Addr::new(10, 79, 0, 2)]
            .into_iter()
            .map(|ip| network.register_endpoint(ip.into()).unwrap())
            .collect();
        let socket = endpoints[1].bind_udp(41000).unwrap();
        let local = SocketAddr::new(socket.local_address().ip(), socket.local_address().port());
        let (log, scream_feedback) = ControllerLog::channel();
        let factory = world
            .peer_factory_builder()
            .unwrap()
            .network_manager(endpoints[0].network_manager().unwrap())
            .packet_socket_factory(endpoints[0].packet_socket_factory().unwrap())
            .video_encoder_factory(VideoEncoderFactoryHandle::builtin().unwrap())
            .video_decoder_factory(VideoDecoderFactoryHandle::builtin().unwrap())
            .event_log_factory(log)
            .build()
            .unwrap();
        let peer = factory
            .create_peer_connection_with_dependencies(
                PeerConfiguration::default().into(),
                PeerConnectionDependencies::default().field_trials(NativeTrials {
                    base: factory.field_trials(),
                    controller,
                    disable_frame_dropper: false,
                }),
            )
            .unwrap();
        peer.set_bitrate(Some(50_000), Some(initial_bps), Some(2_200_000))
            .unwrap();
        let source = factory.create_video_source().unwrap();
        let track = factory.create_video_track("uplink", &source).unwrap();
        let transceiver = peer
            .add_video_transceiver_with_rids(
                &track,
                RtpTransceiverDirection::SendOnly,
                &["q".into(), "h".into(), "f".into()],
            )
            .unwrap();
        let codecs: Vec<_> = peer
            .video_sender_capabilities()
            .unwrap()
            .into_iter()
            .filter(|codec| codec.format().name == "H264")
            .collect();
        assert!(!codecs.is_empty());
        transceiver.set_codec_preferences(&codecs).unwrap();
        let sender = transceiver.sender();
        let mut parameters = sender.parameters().unwrap();
        assert_eq!(parameters.encodings.len(), 3);
        for (encoding, (scale, cap)) in
            parameters
                .encodings
                .iter_mut()
                .zip([(4.0, 200_000), (2.0, 600_000), (1.0, 1_400_000)])
        {
            encoding.scale_resolution_down_by = Some(scale);
            encoding.max_bitrate_bps = Some(cap);
            encoding.max_framerate = Some(f64::from(corpus::FPS));
        }
        sender.set_parameters(parameters).unwrap();
        let mut value = Self {
            peer,
            source,
            sender,
            connection: None,
            socket,
            local,
            network,
            operations: Vec::new(),
            stats: Vec::new(),
            gathered: false,
            connected: false,
            incoming: Vec::new(),
            mid: String::new(),
            negotiation: Vec::new(),
            control_outputs: 0,
            link: Link::new(SCENARIOS[0], 1, u64::MAX / 4),
            epoch_ms: 0,
            origin: Instant::now(),
            scream_feedback,
            _track: track,
            _factory: factory,
            _endpoints: endpoints,
            world,
        };
        let op = value.peer.create_offer().unwrap();
        let offer = value.operation(op).unwrap();
        let op = value.peer.set_local_description(offer).unwrap();
        value.operation(op);
        for _ in 0..2000 {
            value.tick();
            if value.gathered {
                break;
            }
        }
        assert!(value.gathered);
        let offer = value.peer.descriptions().unwrap().pending_local.unwrap();
        let accepted = rtc::RtcConnection::accept(
            rtc::ConnectionConfig {
                local_candidates: vec![rtc::LocalCandidate::Udp(local)],
                ..Default::default()
            },
            rtc::SdpOffer::new(offer.sdp),
            value.now(),
            std::time::UNIX_EPOCH,
            rtc::ConnectionEntropy::new([41; 32]),
        )
        .unwrap();
        assert!(
            accepted.session.senders.is_empty(),
            "receiver must have no media sender"
        );
        // SessionInfo.feedback describes RTC's outbound media controller.
        assert_eq!(accepted.session.feedback, None);
        assert!(accepted.answer.as_str().contains("transport-cc"));
        assert!(
            accepted.answer.as_str().contains(
                "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
            )
        );
        assert_eq!(accepted.session.inbound_media.len(), 1);
        let media = &accepted.session.inbound_media[0];
        assert_eq!(
            media.rids.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
            ["q", "h", "f"]
        );
        value.mid = media.mid.to_string();
        value.negotiation = accepted
            .answer
            .as_str()
            .lines()
            .filter(|line| {
                [
                    "a=rtpmap:",
                    "a=fmtp:",
                    "a=rtcp-fb:",
                    "a=extmap:",
                    "a=simulcast:",
                    "a=rid:",
                ]
                .iter()
                .any(|prefix| line.starts_with(prefix))
            })
            .map(str::to_owned)
            .collect();
        value.connection = Some(accepted.connection);
        let op = value
            .peer
            .set_remote_description(SessionDescription {
                kind: SessionDescriptionType::Answer,
                sdp: accepted.answer.as_str().into(),
            })
            .unwrap();
        value.operation(op);
        for _ in 0..2000 {
            value.tick();
            if value.connected {
                break;
            }
        }
        assert!(value.connected);
        value
    }

    fn now(&self) -> Instant {
        self.origin + self.world.now()
    }

    fn tick(&mut self) {
        let at = u64::try_from(self.world.now().as_millis()).unwrap() - self.epoch_ms;
        let now = self.now();
        let mut settled = false;
        for _ in 0..64 {
            let progress = self.world.pump(512);
            assert!(!progress.blocked);
            while let Some((_, id)) = self.link.pop(at) {
                self.network.deliver(id).unwrap();
            }
            while let Some(packet) = self.socket.try_receive() {
                self.connection
                    .as_mut()
                    .unwrap()
                    .receive(
                        now,
                        now,
                        rtc::NetworkInput::Udp {
                            local: self.local,
                            remote: SocketAddr::new(packet.source.ip(), packet.source.port()),
                            ecn: None,
                            payload: packet.payload.into(),
                        },
                    )
                    .unwrap();
            }
            if let Some(connection) = &mut self.connection {
                let mut idle = false;
                for _ in 0..2048 {
                    match connection.poll(now) {
                        rtc::Output::Transmit(output) => {
                            assert_eq!(
                                output.kind,
                                rtc::TransmitKind::Control,
                                "receiver-only output"
                            );
                            let rtc::TransmitTarget::Udp { remote, .. } = output.target else {
                                panic!("unexpected TCP");
                            };
                            self.socket
                                .send_to(
                                    NetworkAddress::new(remote.ip(), remote.port()).unwrap(),
                                    output.payload.to_vec(),
                                )
                                .unwrap();
                            self.control_outputs += 1;
                        }
                        rtc::Output::Event(rtc::Event::Rtp { mid, rid, packet }) => {
                            assert_eq!(mid.as_ref(), self.mid);
                            self.incoming
                                .push((rid.expect("authenticated RID").to_string(), packet));
                        }
                        rtc::Output::Event(_) => {}
                        rtc::Output::Idle { .. } => {
                            idle = true;
                            break;
                        }
                        other => panic!("RTC output: {other:?}"),
                    }
                }
                assert!(idle, "receiver continuation exhausted");
            }
            let mut emitted = false;
            while let Some(packet) = self.network.next_packet() {
                assert_eq!(packet.kind, OutboundKind::Udp);
                let from = usize::from(packet.source.ip() != Ipv4Addr::new(10, 79, 0, 1));
                if let Err(id) = self
                    .link
                    .push(at, from, packet.payload.len() + 28, packet.id)
                {
                    self.network.drop_packet(id).unwrap();
                }
                emitted = true;
            }
            while let Some(event) = self.peer.try_next_event() {
                match event {
                    PeerConnectionEvent::OperationComplete(result) => self.operations.push(result),
                    PeerConnectionEvent::Stats(stats) => self.stats.push(stats),
                    PeerConnectionEvent::IceGatheringStateChanged(state) => {
                        self.gathered = state == IceGatheringState::Complete;
                    }
                    PeerConnectionEvent::ConnectionStateChanged(state) => {
                        self.connected = state == ConnectionState::Connected;
                    }
                    _ => {}
                }
            }
            if !progress.ready && !emitted {
                settled = true;
                break;
            }
        }
        assert!(settled, "uplink continuation exhausted");
        self.world.advance(Duration::from_millis(1)).unwrap();
    }

    fn operation(&mut self, id: OperationId) -> Option<SessionDescription> {
        for _ in 0..2000 {
            self.tick();
            if let Some(at) = self.operations.iter().position(|op| op.operation_id == id) {
                return self.operations.remove(at).result.unwrap();
            }
        }
        panic!("native operation timeout");
    }
}
impl Drop for Uplink {
    fn drop(&mut self) {
        self.source.close().unwrap();
        self.peer.close().unwrap();
    }
}

pub(super) fn run(
    corpus: &corpus::Corpus,
    scenario: Scenario,
    seed: u64,
    controller: NativeController,
) -> serde_json::Value {
    use serde_json::json;
    let mut native = Uplink::with_start(controller, seed, 300_000);
    let epoch = u64::try_from(native.world.now().as_millis()).unwrap();
    native.link.scenario = scenario;
    native
        .link
        .begin_measurement(epoch, comparison::HORIZON_MS, seed);
    native.epoch_ms = epoch;
    let mut receiver = receiver::Receiver::new(corpus, measurement::CodecClock::NativeMilliseconds);
    let mut assemblies: [_; 3] = std::array::from_fn(|_| rtp_video::Assembly::default());
    let mut ssrcs = BTreeMap::new();
    let mut last_request = [None; 3];
    let mut next = 0;
    let mut budgets = Vec::new();
    for elapsed in 0..comparison::CUTOFF_MS {
        if next < corpus.len() && corpus.offered(next).at_ns.div_ceil(1_000_000) <= elapsed {
            let frame = corpus.frame(next);
            native
                .source
                .push_frame(
                    &VideoFrame::i420(
                        1280,
                        720,
                        frame.full,
                        i64::try_from(epoch * 1000 + frame.offered.at_ns / 1000).unwrap(),
                        u32::try_from(next).unwrap() * corpus::RTP_STEP,
                    )
                    .unwrap(),
                )
                .unwrap();
            next += 1;
        }
        if elapsed.is_multiple_of(1000) {
            native.peer.request_sender_stats(&native.sender).unwrap();
        }
        native.tick();
        let ready_ns = u64::try_from(native.world.now().as_nanos()).unwrap() - epoch * 1_000_000;
        for (rid, packet) in native.incoming.drain(..) {
            let layer = match rid.as_str() {
                "q" => 0,
                "h" => 1,
                "f" => 2,
                _ => panic!("unnegotiated RID"),
            };
            if let Some(previous) = ssrcs.insert(layer, packet.header.ssrc) {
                assert_eq!(previous, packet.header.ssrc);
            }
            if let Some(unit) = assemblies[layer].push(&packet) {
                receiver.unit(
                    layer,
                    &unit.data,
                    unit.timestamp,
                    unit.key,
                    unit.missing_before,
                    ready_ns,
                );
                if receiver.waiting[layer] {
                    assemblies[layer].invalidate();
                }
            }
        }
        receiver.collect(ready_ns);
        for (layer, last_request) in last_request.iter_mut().enumerate() {
            if receiver.waiting[layer]
                && receiver.complete[layer] > 0
                && last_request.is_none_or(|last| elapsed.saturating_sub(last) >= 200)
                && let Some(ssrc) = ssrcs.get(&layer)
            {
                native
                    .connection
                    .as_mut()
                    .unwrap()
                    .request_keyframe(*ssrc)
                    .unwrap();
                *last_request = Some(elapsed);
            }
        }
        for stats in native.stats.drain(..) {
            let targets: Vec<_> = stats
                .records
                .into_iter()
                .filter_map(|record| match record {
                    PeerStatsRecord::OutboundRtp(out) => out.target_bitrate,
                    _ => None,
                })
                .collect();
            if !targets.is_empty() {
                budgets.push(
                    json!({"at_ms": elapsed, "observed_encoding_target_sum_bps": targets.iter().sum::<f64>(), "encoding_targets_observed": targets.len()}),
                );
            }
        }
    }
    let observed = native.scream_feedback.try_recv().is_ok();
    let controller_qualified = observed == (controller == NativeController::ScreamV2);
    let result = json!({"summary": comparison::summary(corpus, &receiver,
        u64::try_from(native.world.now().as_nanos()).unwrap() - epoch * 1_000_000, controller_qualified),
        "wire": comparison::wire(&native.link), "inner_cc": "none", "outer_cc": controller.label(),
        "negotiation": {"receiver_only": true, "twcc": true, "rids": ["q", "h", "f"], "answer_media_attributes": native.negotiation,
            "observed_ssrc_by_layer": ssrcs.iter().map(|(layer, ssrc)| json!({"layer": layer, "ssrc": ssrc})).collect::<Vec<_>>()},
        "native_scream_feedback_observed": observed, "observed_encoding_target_samples": budgets,
        "source": {"raw_offered_captures": next, "produced": null, "omissions_by_reason": null,
            "reason": "raw native API does not expose per-capture encoded output or omission reasons"},
        "lifecycle": {"incomplete_received_units_by_layer": assemblies.each_ref().map(|assembly| assembly.incomplete),
            "native_input_pending": null, "native_input_pending_reason": "raw-source input ownership is not exposed",
            "encoded_terminal_dispositions": null, "encoded_terminal_reason": "no per-representation native encoded-output tap"},
        "failure": if controller_qualified { None } else { Some("native controller observation mismatch") }});
    while let Some((_, _, id)) = native.link.pending.pop_front() {
        native.network.drop_packet(id).unwrap();
    }
    result
}

#[test]
fn native_three_rid_receiver_only_capability() {
    let corpus = corpus::Corpus::new(pulsebeam_testdata::QualityVideoSource::Zero, 7);
    for controller in [NativeController::ScreamV2, NativeController::Gcc] {
        let mut native = Uplink::new(controller);
        let epoch = u64::try_from(native.world.now().as_millis()).unwrap();
        native.link.scenario = *SCENARIOS
            .iter()
            .find(|s| s.name == "capacity_step")
            .unwrap();
        native.link.begin_measurement(epoch, 6000, 1);
        native.epoch_ms = epoch;
        let mut receivers: BTreeMap<_, _> = ["q", "h", "f"]
            .into_iter()
            .map(|rid| {
                (
                    rid.to_owned(),
                    (rtp_video::Assembly::default(), decoder::Decoder::new()),
                )
            })
            .collect();
        let mut observations = measurement::Observations::new(&corpus);
        let mut identities: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
        let mut ssrcs = BTreeMap::new();
        let mut anchors: BTreeMap<String, (usize, u32)> = BTreeMap::new();
        let mut keyframes: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut budgets = Vec::new();
        let mut pli_snapshots = Vec::new();
        let mut last_request = BTreeMap::new();
        let mut dependency_discarded = 0u64;
        let mut requested = false;
        let mut next_capture = 0usize;
        for elapsed in 0..6500u64 {
            if next_capture < corpus.len()
                && corpus.offered(next_capture).at_ns.div_ceil(1_000_000) <= elapsed
            {
                let capture = corpus.frame(next_capture);
                native
                    .source
                    .push_frame(
                        &VideoFrame::i420(
                            1280,
                            720,
                            capture.full,
                            i64::try_from(epoch * 1000 + capture.offered.at_ns / 1000).unwrap(),
                            u32::try_from(next_capture).unwrap() * corpus::RTP_STEP,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                next_capture += 1;
            }
            if elapsed == 1200 {
                assert_eq!(
                    ssrcs.len(),
                    3,
                    "all actual rungs must arrive before repair proof"
                );
                for &ssrc in ssrcs.values() {
                    native
                        .connection
                        .as_mut()
                        .unwrap()
                        .request_keyframe(ssrc)
                        .unwrap();
                }
                requested = true;
            }
            if [1100, 1900, 3900, 5900].contains(&elapsed) {
                native.peer.request_sender_stats(&native.sender).unwrap();
            }
            native.tick();
            for stats in native.stats.drain(..) {
                let counts: BTreeMap<_, _> = stats
                    .report
                    .of_class(RtcStatsClass::OutboundRtpStreamStats)
                    .into_iter()
                    .filter_map(|entry| match entry.values() {
                        RtcStatsValues::OutboundRtpStreamStats(value) => {
                            Some((value.ssrc?, value.pli_count?))
                        }
                        _ => None,
                    })
                    .collect();
                pli_snapshots.push(counts);
                let budget: f64 = stats
                    .records
                    .into_iter()
                    .filter_map(|record| match record {
                        PeerStatsRecord::OutboundRtp(out) => out.target_bitrate,
                        _ => None,
                    })
                    .sum();
                budgets.push(budget);
            }
            for (rid, packet) in native.incoming.drain(..) {
                if let Some(old) = ssrcs.insert(rid.clone(), packet.header.ssrc) {
                    assert_eq!(old, packet.header.ssrc);
                }
                let (assembly, decoder) = receivers.get_mut(&rid).expect("negotiated rung");
                let Some(unit) = assembly.push(&packet) else {
                    continue;
                };
                // Do not submit dependent pictures after missing encoded input.
                // Capture omissions alone do not set this RTP dependency fence.
                if unit.missing_before && !unit.key {
                    dependency_discarded += 1;
                    let previous = last_request.entry(rid.clone()).or_insert(0u64);
                    if elapsed.saturating_sub(*previous) >= 200 {
                        native
                            .connection
                            .as_mut()
                            .unwrap()
                            .request_keyframe(packet.header.ssrc)
                            .unwrap();
                        *previous = elapsed;
                    }
                    continue;
                }
                let (width, height) = match rid.as_str() {
                    "q" => (320, 180),
                    "h" => (640, 360),
                    "f" => (1280, 720),
                    _ => panic!("unnegotiated rung"),
                };
                let mut image = EncodedImage::default();
                image.set_buffer(&EncodedVideoBuffer::from_bytes(&unit.data));
                let mut metadata = image.metadata();
                metadata.width = width;
                metadata.height = height;
                metadata.rtp_timestamp = unit.timestamp;
                metadata.frame_type = if unit.key {
                    EncodedImageFrameType::Key
                } else {
                    EncodedImageFrameType::Delta
                };
                image.set_metadata(&metadata).unwrap();
                if !decoder.decode_image(&image, false, i64::try_from(elapsed).unwrap()) {
                    assembly.invalidate();
                }
                while let Ok(frame) = decoder.output.try_recv() {
                    assert_eq!((frame.width, frame.height), (width, height));
                    let id =
                        corpus::identify(&frame, 0, 7).expect("decoded scope/capture identity");
                    let capture = usize::try_from(id.capture).unwrap();
                    assert!(capture < corpus.len());
                    assert_eq!(id, corpus.offered(capture).id);
                    let (base_capture, base_timestamp) = *anchors
                        .entry(rid.clone())
                        .or_insert((capture, frame.rtp_timestamp));
                    assert_eq!(
                        frame.rtp_timestamp.wrapping_sub(base_timestamp),
                        u32::try_from(
                            (corpus.offered(capture).at_ns / 1_000_000)
                                - (corpus.offered(base_capture).at_ns / 1_000_000)
                        )
                        .unwrap()
                            * 90,
                        "{controller:?} {rid}: capture={capture}, anchor={base_capture}/{base_timestamp}, elapsed={elapsed}, timestamp={}",
                        frame.rtp_timestamp
                    );
                    observations.decoded(
                        &frame,
                        match rid.as_str() {
                            "q" => 0,
                            "h" => 1,
                            "f" => 2,
                            _ => panic!("unknown rung"),
                        },
                        u64::try_from(native.world.now().as_nanos()).unwrap() - epoch * 1_000_000,
                        measurement::CodecClock::NativeMilliseconds,
                    );
                    identities.entry(rid.clone()).or_default().insert(capture);
                    if unit.key {
                        keyframes.entry(rid.clone()).or_default().push(capture);
                    }
                }
            }
        }
        let report = observations.finish(
            &corpus,
            u64::try_from(native.world.now().as_nanos()).unwrap() - epoch * 1_000_000,
            true,
        );
        assert!(report.timeline.qualified);
        assert_eq!(report.timeline.decode.len(), corpus.len());
        assert!(report.timeline.presented_ns > 0 && report.timeline.blank_ns > 0);
        assert_eq!(
            identities
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["q", "h", "f"])
        );
        assert_eq!(ssrcs.values().copied().collect::<BTreeSet<_>>().len(), 3);
        assert!(
            dependency_discarded > 0,
            "capacity drop must exercise the decoder dependency fence"
        );
        assert_eq!(budgets.len(), 4);
        for (rid, ssrc) in &ssrcs {
            assert!(
                pli_snapshots[1][ssrc] > pli_snapshots[0][ssrc],
                "native must authenticate PLI for {rid}: {pli_snapshots:?}"
            );
        }
        assert!(
            requested
                && keyframes.values().all(|frames| frames
                    .iter()
                    .any(|capture| corpus.offered(*capture).at_ns > 1_200_000_000)),
            "real PLI must be followed by identified keyframes: {keyframes:?}"
        );
        assert!(
            budgets[2] < budgets[1],
            "{controller:?}: actual aggregate native targets must respond to the controlled capacity drop: {budgets:?}"
        );
        assert!(native.control_outputs > 0);
        assert_eq!(
            native.scream_feedback.try_recv().is_ok(),
            controller == NativeController::ScreamV2
        );
        assert!(
            native
                .connection
                .as_ref()
                .unwrap()
                .stats()
                .senders
                .is_empty()
        );
    }
}
