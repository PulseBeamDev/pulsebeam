//! Real PCM capture -> selected stock audio codec -> NetEq/raw playout with caller-owned progress.
use pulsebeam_webrtc_sys::*;
use serde::Serialize;
use std::{
    cell::RefCell,
    collections::HashMap,
    net::IpAddr,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

#[path = "pcm_periodic.rs"]
mod periodic;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct EncodedFrameObservation {
    ssrc: u32,
    rtp_timestamp: u32,
    payload_type: u8,
    data: Vec<u8>,
}

#[derive(Default)]
struct EncodedAudit {
    frames: Mutex<Vec<EncodedFrameObservation>>,
}

impl FrameTransformerProvider for EncodedAudit {
    fn transform(&self, frame: TransformableMediaFrame, completion: FrameTransformCompletion) {
        assert!(matches!(&frame, TransformableMediaFrame::Audio(_)));
        assert_eq!(frame.direction(), TransformableFrameDirection::Sender);
        self.frames.lock().unwrap().push(EncodedFrameObservation {
            ssrc: frame.ssrc(),
            rtp_timestamp: frame.rtp_timestamp(),
            payload_type: frame.payload_type(),
            data: frame.data().to_vec(),
        });
        // Observe the actual plaintext encoder output, then return the same
        // owning SDK frame without changing its data, metadata or callback.
        assert_eq!(
            completion.complete(frame).unwrap(),
            FrameTransformCompletionOutcome::DeliveredToNative
        );
    }
}

struct Call<R: SessionRuntime> {
    sessions: [Session<R>; 2],
    peers: [SessionPeerId; 2],
    operations: HashMap<(usize, OperationId), Option<SessionDescription>>,
    stats: HashMap<(usize, OperationId), PeerStatsSnapshot>,
    gathered: [bool; 2],
    connected: [bool; 2],
    receivers: [Option<SessionReceiverId>; 2],
    answer_transceiver: Option<SessionTransceiverId>,
    encoded: [Arc<EncodedAudit>; 2],
    transformers: [Option<SessionFrameTransformerId>; 2],
    events: Vec<String>,
}

impl<R: SessionRuntime> Call<R> {
    fn new(
        runtimes: [R; 2],
        processing: Option<&AudioProcessorConfig>,
        reports: Option<&Arc<periodic::Audit>>,
    ) -> Self {
        let mut index = 0;
        let mut sessions = runtimes.map(|runtime| {
            let mut builder = Session::with_runtime(runtime, SessionConfig::default());
            if let Some(config) = processing {
                builder = builder.audio_processor_builder(
                    BuiltinAudioProcessorBuilder::new().config(config.clone()),
                );
            }
            if let Some(audit) = reports {
                builder = builder.event_log_factory(periodic::Factory {
                    peer: index,
                    audit: audit.clone(),
                });
            }
            index += 1;
            builder.build().unwrap()
        });
        let peers = sessions
            .each_mut()
            .map(|session| session.create_peer(PeerConfiguration::default()).unwrap());
        Self {
            sessions,
            peers,
            operations: HashMap::new(),
            stats: HashMap::new(),
            gathered: [false; 2],
            connected: [false; 2],
            receivers: [None; 2],
            answer_transceiver: None,
            encoded: std::array::from_fn(|_| Arc::new(EncodedAudit::default())),
            transformers: [None; 2],
            events: Vec::new(),
        }
    }

    fn observe_sender(&mut self, index: usize, sender: SessionSenderId) {
        let transformer = self.sessions[index]
            .create_frame_transformer(self.encoded[index].clone(), usize::MAX)
            .unwrap();
        self.sessions[index]
            .set_sender_frame_transformer(sender, Some(transformer))
            .unwrap();
        assert!(self.transformers[index].replace(transformer).is_none());
    }

    fn drain(&mut self) {
        for index in 0..2 {
            while let Some(event) = self.sessions[index]
                .try_peer_event(self.peers[index])
                .unwrap()
            {
                match event {
                    SessionEvent::OperationComplete(done) => assert!(
                        self.operations
                            .insert((index, done.operation_id), done.result.unwrap())
                            .is_none()
                    ),
                    SessionEvent::Stats(snapshot) => {
                        assert!(
                            self.stats
                                .insert((index, snapshot.operation_id), snapshot)
                                .is_none()
                        );
                    }
                    SessionEvent::IceGatheringStateChanged(state) => {
                        self.events.push(format!("{index}:gather:{state:?}"));
                        self.gathered[index] = state == IceGatheringState::Complete;
                    }
                    SessionEvent::ConnectionStateChanged(state) => {
                        self.events.push(format!("{index}:connection:{state:?}"));
                        self.connected[index] = state == ConnectionState::Connected;
                    }
                    SessionEvent::Track {
                        receiver,
                        transceiver,
                    } => {
                        assert_eq!(
                            self.sessions[index].receiver_media_type(receiver).unwrap(),
                            MediaType::Audio
                        );
                        assert!(self.receivers[index].replace(receiver).is_none());
                        if index == 1 {
                            assert!(self.answer_transceiver.replace(transceiver).is_none());
                        }
                        self.events.push(format!("{index}:audio-track"));
                    }
                    _ => {}
                }
            }
        }
    }

    fn finish(
        &mut self,
        index: usize,
        operation: OperationId,
        progress: &mut impl FnMut(Duration),
    ) -> Option<SessionDescription> {
        for _ in 0..10_000 {
            self.drain();
            if let Some(description) = self.operations.remove(&(index, operation)) {
                return description;
            }
            progress(Duration::from_millis(1));
        }
        panic!("native operation stalled: peer {index}");
    }

    fn retire_graph(mut self) -> Vec<String> {
        for (index, receiver) in self.receivers.map(Option::unwrap).iter().enumerate() {
            self.sessions[index].close_media_sink(*receiver).unwrap();
            self.sessions[index].close_peer(self.peers[index]).unwrap();
            self.sessions[index].shutdown().unwrap();
        }
        let events = std::mem::take(&mut self.events);
        // Final native peer/factory/Call destruction is part of this operation,
        // not deferred to the caller after the execution context is restored.
        drop(self);
        events
    }

    fn snapshot(&mut self, index: usize, progress: &mut impl FnMut(Duration)) -> periodic::Stats {
        let operation = self.sessions[index]
            .request_stats(self.peers[index])
            .unwrap();
        for _ in 0..10_000 {
            self.drain();
            if let Some(snapshot) = self.stats.remove(&(index, operation)) {
                return periodic::Stats::from_native(index, snapshot);
            }
            progress(Duration::from_millis(1));
        }
        panic!("native stats stalled: peer {index}");
    }

    fn gather(
        &mut self,
        index: usize,
        description: SessionDescription,
        progress: &mut impl FnMut(Duration),
    ) -> SessionDescription {
        let operation = self.sessions[index]
            .set_local_description(self.peers[index], description)
            .unwrap();
        self.finish(index, operation, progress);
        for _ in 0..10_000 {
            self.drain();
            if self.gathered[index] {
                let descriptions = self.sessions[index]
                    .descriptions(self.peers[index])
                    .unwrap();
                return descriptions
                    .current_local
                    .or(descriptions.pending_local)
                    .unwrap();
            }
            progress(Duration::from_millis(1));
        }
        panic!("native gathering stalled: peer {index}");
    }

    fn negotiate(
        &mut self,
        codec_name: &str,
        answer_source: SessionSourceId,
        progress: &mut impl FnMut(Duration),
    ) -> (u8, String) {
        let operation = self.sessions[0].create_offer(self.peers[0], false).unwrap();
        let offer = self.finish(0, operation, progress).unwrap();
        let (payload_type, codec_rtpmap) = offer
            .sdp
            .lines()
            .find_map(|line| {
                let (payload_type, codec) = line.strip_prefix("a=rtpmap:")?.split_once(' ')?;
                let codec = codec.trim();
                let (name, _) = codec.split_once('/')?;
                name.eq_ignore_ascii_case(codec_name)
                    .then(|| (payload_type.parse::<u8>().unwrap(), codec.to_owned()))
            })
            .unwrap_or_else(|| panic!("stock codec must be negotiated: {codec_name}"));
        let offer = self.gather(0, offer, progress);
        let operation = self.sessions[1]
            .set_remote_description(self.peers[1], offer)
            .unwrap();
        self.finish(1, operation, progress);
        // Attach the return source to the transceiver established by the
        // offer, rather than pre-creating a separate, unassociated m-line.
        let transceiver = self.answer_transceiver.unwrap();
        let sender = self.sessions[1].sender(transceiver).unwrap();
        self.sessions[1]
            .replace_audio_source(sender, Some(answer_source), "pcm-tone-1")
            .unwrap();
        self.sessions[1]
            .set_direction(transceiver, RtpTransceiverDirection::SendReceive)
            .unwrap();
        self.observe_sender(1, sender);
        let operation = self.sessions[1].create_answer(self.peers[1]).unwrap();
        let answer = self.finish(1, operation, progress).unwrap();
        assert!(
            answer
                .sdp
                .contains(&format!("a=rtpmap:{payload_type} {codec_rtpmap}"))
        );
        let answer = self.gather(1, answer, progress);
        let operation = self.sessions[0]
            .set_remote_description(self.peers[0], answer)
            .unwrap();
        self.finish(0, operation, progress);
        for _ in 0..10_000 {
            self.drain();
            if self.connected == [true; 2] && self.receivers.iter().all(Option::is_some) {
                return (payload_type, codec_rtpmap);
            }
            progress(Duration::from_millis(1));
        }
        panic!(
            "native audio negotiation incomplete: connected={:?}, receivers={:?}, events={:?}",
            self.connected, self.receivers, self.events
        );
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct FrameObservation {
    observed_at_us: u128,
    sample_rate_hz: u32,
    channels: u8,
    samples_per_channel: u32,
    capture_time_ms: Option<i64>,
    capture_time_us: Option<i64>,
    samples: Vec<i16>,
}

#[derive(Debug, Serialize)]
struct WirePacket {
    id: u64,
    kind: String,
    source: String,
    destination: String,
    sender: usize,
    deadline_us: u128,
    observed_at_us: u128,
    payload: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<&'static str>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct PacketObservation {
    id: u64,
    kind: String,
    source: String,
    destination: String,
    sender: usize,
    deadline_us: u128,
    observed_at_us: u128,
    rtp_header: Option<Vec<u8>>,
    rtp_packet_size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct Trace {
    observation: Observation,
    // Keep the actual bytes, including variable crypto, without normalizing
    // them into the replay observation. Crypto identity is outside the contract.
    wire: Vec<WirePacket>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct QueueTeardown {
    queue_name: Vec<u8>,
    started_at_us: u128,
    returned_at_us: u128,
    entered_current: bool,
    exited_current: bool,
    caller_current_after: bool,
    pumps: usize,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct Observation {
    events: Vec<String>,
    codec_rtpmap: String,
    payload_type: u8,
    encoded: [Vec<EncodedFrameObservation>; 2],
    captures_at_us: Vec<u128>,
    frames: [Vec<FrameObservation>; 2],
    tone_frames: [usize; 2],
    media_elapsed_us: u128,
    packets: Vec<PacketObservation>,
    rtp_packets: [usize; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    periodic: Option<periodic::Observation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_teardown: Option<QueueTeardown>,
}

fn has_tone(samples: &[i16], period: usize) -> bool {
    let correlate = |lag: usize| {
        samples.iter().zip(samples.iter().skip(lag)).fold(
            (0i64, 0i64),
            |(dot, energy), (&a, &b)| {
                (
                    dot + i64::from(a) * i64::from(b),
                    energy + i64::from(a) * i64::from(a),
                )
            },
        )
    };
    let (dot, energy) = correlate(period);
    let (opposite, half_energy) = correlate(period / 2);
    energy > 1_000_000 && dot > energy / 2 && opposite < -half_energy / 2
}

fn application<R: SessionRuntime>(
    runtimes: [R; 2],
    codec_name: &str,
    decoded_sample_rate_hz: u32,
    processing: Option<&AudioProcessorConfig>,
    capture_blocks: usize,
    reports: Option<&Arc<periodic::Audit>>,
    finish: impl FnOnce(Call<R>) -> Vec<String>,
    mut progress: impl FnMut(Duration),
    mut now: impl FnMut() -> Duration,
) -> Observation {
    let mut call = Call::new(runtimes, processing, reports);
    if reports.is_some() {
        for index in 0..2 {
            call.sessions[index]
                .start_peer_event_log(
                    call.peers[index],
                    EventLogOutput::from_sink(periodic::Output),
                    Some(EventLogOutputPeriod::Immediate),
                )
                .unwrap();
        }
    }
    let sources = call
        .sessions
        .each_mut()
        .map(|session| session.create_pcm_source().unwrap());
    let transceiver = call.sessions[0]
        .publish_pcm(
            call.peers[0],
            sources[0],
            "pcm-tone-0",
            RtpTransceiverDirection::SendReceive,
            None,
        )
        .unwrap();
    let codec = call.sessions[0]
        .get_rtp_sender_capabilities(MediaType::Audio)
        .unwrap()
        .codecs()
        .into_iter()
        .find(|codec| {
            codec
                .base_codec()
                .name_bytes()
                .eq_ignore_ascii_case(codec_name.as_bytes())
        })
        .unwrap_or_else(|| panic!("stock encoder capability missing: {codec_name}"));
    call.sessions[0]
        .set_rtp_codec_preferences(transceiver, &[codec])
        .unwrap();
    let sender = call.sessions[0].sender(transceiver).unwrap();
    call.observe_sender(0, sender);
    let (payload_type, codec_rtpmap) = call.negotiate(codec_name, sources[1], &mut progress);
    let receivers = call.receivers.map(Option::unwrap);
    for (index, receiver) in receivers.iter().enumerate() {
        call.sessions[index].attach_pcm(*receiver).unwrap();
    }
    let tones = [1000, 1500].map(|frequency| {
        let samples = (0..480)
            .map(|index| {
                if index * frequency * 2 / 48000 % 2 == 0 {
                    12000
                } else {
                    -12000
                }
            })
            .collect();
        AudioPcmFrame::i16_interleaved(48000, 1, 0, samples).unwrap()
    });
    let mut frames: [Vec<FrameObservation>; 2] = [Vec::new(), Vec::new()];
    let mut tone_frames = [0; 2];
    let mut captures_at_us = Vec::new();
    let before = reports.map(|_| std::array::from_fn(|index| call.snapshot(index, &mut progress)));
    let started_at = now();
    for step in 0..capture_blocks + 100 {
        if step < capture_blocks {
            let before_push = now();
            captures_at_us.push(before_push.as_micros());
            for index in 0..2 {
                let capture = call.sessions[index]
                    .pcm_capture_time_now(sources[index])
                    .unwrap();
                call.sessions[index]
                    .push_pcm_at(sources[index], &tones[index], capture)
                    .unwrap();
            }
            assert_eq!(now(), before_push, "PCM push advanced virtual time");
        }
        progress(Duration::from_millis(10));
        call.drain();
        // Poll exactly once per receiver per 10ms step. An empty live sink can
        // drive headless playout, so an unbounded drain would itself drive audio.
        for index in 0..2 {
            if let Some(frame) = call.sessions[index]
                .try_pcm_frame(receivers[index])
                .unwrap()
            {
                // Native callbacks include both codec-rate and 48kHz output.
                // Keep every returned format and sample block without local
                // resampling or discarding startup/concealment.
                assert!([48000, decoded_sample_rate_hz].contains(&frame.sample_rate_hz));
                assert_eq!(frame.channels, 1);
                assert_eq!(frame.samples_per_channel, frame.sample_rate_hz / 100);
                assert_eq!(frame.samples.len(), (frame.sample_rate_hz / 100) as usize);
                let frequency = [1000, 1500][1 - index];
                if has_tone(&frame.samples, (frame.sample_rate_hz / frequency) as usize) {
                    tone_frames[index] += 1;
                }
                // Retain silence and any concealment, not just selected tones.
                frames[index].push(FrameObservation {
                    observed_at_us: now().as_micros(),
                    sample_rate_hz: frame.sample_rate_hz,
                    channels: frame.channels,
                    samples_per_channel: frame.samples_per_channel,
                    capture_time_ms: frame.capture_time_ms,
                    capture_time_us: frame.capture_time_us,
                    samples: frame.samples,
                });
            }
        }
        if step + 1 >= capture_blocks && tone_frames.iter().all(|&count| count >= 3) {
            break;
        }
    }
    assert_eq!(captures_at_us.len(), capture_blocks);
    assert!(
        captures_at_us
            .windows(2)
            .all(|pair| pair[1] - pair[0] == 10_000),
        "PCM captures must be paced by explicit virtual 10ms steps"
    );
    assert!(
        tone_frames.iter().all(|&count| count >= 3),
        "stock PCM/{codec_name}/NetEq did not deliver three source-correlated tone blocks per receiver: {tone_frames:?}"
    );
    let media_elapsed_us = now().as_micros() - started_at.as_micros();
    assert!(media_elapsed_us >= (capture_blocks as u128) * 10_000);
    let periodic = before.map(|before| periodic::Observation {
        media_started_at_us: started_at.as_micros(),
        before,
        after: std::array::from_fn(|index| call.snapshot(index, &mut progress)),
        reports: periodic::Reports::default(),
    });
    let before_close = now();
    if reports.is_some() {
        for index in 0..2 {
            call.sessions[index]
                .stop_peer_event_log(call.peers[index])
                .unwrap();
        }
    }
    let encoded = call.encoded.clone();
    for index in 0..2 {
        let stats = call.sessions[index]
            .frame_transformer_statistics(call.transformers[index].unwrap())
            .unwrap();
        assert_eq!(stats.in_flight, 0);
        assert_eq!(stats.delivered, stats.completed);
        assert_eq!(
            stats.delivered as usize,
            encoded[index].frames.lock().unwrap().len()
        );
        assert_eq!(
            [
                stats.abandoned,
                stats.late_completions,
                stats.capacity_drops,
                stats.wrong_frame,
                stats.callback_panics
            ],
            [0; 5],
            "plaintext observation must not drop, bypass or corrupt encoded frames"
        );
    }
    let events = finish(call);
    assert_eq!(now(), before_close, "teardown advanced virtual time");
    let encoded = encoded.map(|audit| audit.frames.lock().unwrap().clone());
    for frames in &encoded {
        assert!(frames.len() >= 3);
        assert!(
            frames
                .iter()
                .all(|frame| frame.payload_type == payload_type && !frame.data.is_empty())
        );
    }
    Observation {
        events,
        codec_rtpmap,
        payload_type,
        encoded,
        captures_at_us,
        frames,
        tone_frames,
        media_elapsed_us,
        packets: Vec::new(),
        rtp_packets: [0; 2],
        periodic,
        queue_teardown: None,
    }
}

fn audio_rtp_header(bytes: &[u8], payload_type: u8) -> Option<Vec<u8>> {
    // SRTP retains the RTP header in the clear. Classify only the negotiated
    // audio PT, excluding STUN/DTLS and the RTCP type range.
    if bytes.len() < 12
        || bytes[0] >> 6 != 2
        || (192..=223).contains(&bytes[1])
        || bytes[1] & 0x7f != payload_type
    {
        return None;
    }
    let mut length = 12 + usize::from(bytes[0] & 0x0f) * 4;
    assert!(bytes.len() >= length);
    if bytes[0] & 0x10 != 0 {
        assert!(bytes.len() >= length + 4);
        let words = u16::from_be_bytes([bytes[length + 2], bytes[length + 3]]);
        length += 4 + usize::from(words) * 4;
    }
    assert!(bytes.len() >= length);
    Some(bytes[..length].to_vec())
}

fn run_case(
    processing: Option<&AudioProcessorConfig>,
    capture_blocks: usize,
    observe_periodic: bool,
    impaired: bool,
    queued_teardown: bool,
) -> Trace {
    run_codec_case(
        "opus",
        48000,
        processing,
        capture_blocks,
        observe_periodic,
        impaired,
        queued_teardown,
    )
}

fn run_codec_case(
    codec_name: &str,
    decoded_sample_rate_hz: u32,
    processing: Option<&AudioProcessorConfig>,
    capture_blocks: usize,
    observe_periodic: bool,
    impaired: bool,
    queued_teardown: bool,
) -> Trace {
    eprintln!("CONTROLLED_MEDIA_BEGIN");
    let observation = {
        let world = ControlledWorld::acquire(0x50434d, Duration::from_secs(7)).unwrap();
        let network = world.create_network().unwrap();
        let ips = ["192.0.2.1", "192.0.2.2"].map(|ip| ip.parse::<IpAddr>().unwrap());
        let endpoints = ips.map(|ip| network.register_endpoint(ip).unwrap());
        let runtimes = endpoints
            .each_ref()
            .map(|endpoint| world.runtime().unwrap().with_endpoint(endpoint).unwrap());
        let packets = RefCell::new(Vec::new());
        let mut sent_rtp = [0usize; 2];
        let reports = observe_periodic.then(|| Arc::new(periodic::Audit::default()));
        let queue = queued_teardown.then(|| {
            Rc::new(
                world
                    .create_queue("stock-audio-close", QueuePriority::Normal)
                    .unwrap(),
            )
        });
        let mut queue_teardown = None;
        let mut observation = application(
            runtimes,
            codec_name,
            decoded_sample_rate_hz,
            processing,
            capture_blocks,
            reports.as_ref(),
            |call| {
                let Some(queue) = queue else {
                    return call.retire_graph();
                };
                assert!(!queue.is_current());
                let started_at_us = world.now().as_micros();
                let result = Rc::new(RefCell::new(None));
                let completed = result.clone();
                let callback_queue = queue.clone();
                assert!(
                    queue
                        .post_local(move || {
                            let entered_current = callback_queue.is_current();
                            assert!(entered_current);
                            let events = call.retire_graph();
                            let exited_current = callback_queue.is_current();
                            assert!(exited_current);
                            *completed.borrow_mut() =
                                Some((events, entered_current, exited_current));
                        })
                        .unwrap()
                );
                for pumps in 1..=10_000 {
                    assert!(!world.pump(256).blocked);
                    if let Some((events, entered_current, exited_current)) =
                        result.borrow_mut().take()
                    {
                        assert!(!queue.is_current());
                        assert!(!queue.has_failed());
                        queue_teardown = Some(QueueTeardown {
                            queue_name: queue.name_bytes(),
                            started_at_us,
                            returned_at_us: world.now().as_micros(),
                            entered_current,
                            exited_current,
                            caller_current_after: queue.is_current(),
                            pumps,
                        });
                        return events;
                    }
                }
                panic!("stock audio teardown from caller queue stalled");
            },
            |step| {
                assert!(!world.pump(256).blocked);
                while let Some(packet) = network.next_packet() {
                    let sender = ips.iter().position(|&ip| ip == packet.source.ip()).unwrap();
                    // The application selects one stock codec. The outer caller
                    // drops every tenth RTP packet, never STUN/DTLS/RTCP.
                    let rtp = packet.payload.len() >= 12
                        && packet.payload[0] >> 6 == 2
                        && !(192..=223).contains(&packet.payload[1]);
                    if rtp {
                        sent_rtp[sender] += 1;
                    }
                    let drop = impaired && rtp && sent_rtp[sender] % 10 == 5;
                    packets.borrow_mut().push(WirePacket {
                        id: packet.id,
                        kind: format!("{:?}", packet.kind),
                        source: format!("{}:{}", packet.source.ip(), packet.source.port()),
                        destination: format!(
                            "{}:{}",
                            packet.destination.ip(),
                            packet.destination.port()
                        ),
                        sender,
                        deadline_us: packet.deadline.as_micros(),
                        observed_at_us: world.now().as_micros(),
                        payload: packet.payload.clone(),
                        decision: observe_periodic.then_some(if drop { "drop" } else { "deliver" }),
                    });
                    if drop {
                        network.drop_packet(packet.id).unwrap();
                    } else {
                        network.deliver(packet.id).unwrap();
                    }
                }
                world.advance(step).unwrap();
            },
            || world.now(),
        );
        if let Some(audit) = reports {
            observation.periodic.as_mut().unwrap().reports = audit.snapshot();
        }
        observation.queue_teardown = queue_teardown;
        let wire = packets.into_inner();
        for packet in &wire {
            let header = audio_rtp_header(&packet.payload, observation.payload_type);
            let rtp_packet_size = header.as_ref().map(|_| packet.payload.len());
            if header.is_some() {
                observation.rtp_packets[packet.sender] += 1;
            }
            observation.packets.push(PacketObservation {
                id: packet.id,
                kind: packet.kind.clone(),
                source: packet.source.clone(),
                destination: packet.destination.clone(),
                sender: packet.sender,
                deadline_us: packet.deadline_us,
                observed_at_us: packet.observed_at_us,
                rtp_header: header,
                rtp_packet_size,
                decision: packet.decision,
            });
        }
        assert!(
            observation.rtp_packets.iter().all(|&count| count >= 3),
            "PCM inputs must produce at least three native {codec_name} RTP packets per sender: {:?}",
            observation.rtp_packets
        );
        Trace { observation, wire }
    };
    eprintln!("CONTROLLED_MEDIA_END");
    observation
}

pub fn check_replay(processing: Option<&AudioProcessorConfig>, trace_variable: &str) {
    check_short_replay(processing, trace_variable, false);
}

pub fn check_queued_teardown_replay(trace_variable: &str) {
    check_short_replay(None, trace_variable, true);
}

fn check_short_replay(
    processing: Option<&AudioProcessorConfig>,
    trace_variable: &str,
    queued_teardown: bool,
) {
    let reference = processing.map(|_| run_case(None, 6, false, false, false));
    let first_trace = run_case(processing, 6, false, false, queued_teardown);
    let second_trace = run_case(processing, 6, false, false, queued_teardown);
    // Write only after every complete native lifecycle, including on replay
    // failure. Retain each run's unmodified wire, not only the replay subset.
    if let Some(path) = std::env::var_os(trace_variable) {
        let path = std::path::PathBuf::from(path);
        std::fs::write(&path, serde_json::to_vec(&first_trace).unwrap()).unwrap();
        std::fs::write(
            path.with_extension("second.json"),
            serde_json::to_vec(&second_trace).unwrap(),
        )
        .unwrap();
        if let Some(reference) = &reference {
            std::fs::write(
                path.with_extension("reference.json"),
                serde_json::to_vec(reference).unwrap(),
            )
            .unwrap();
        }
    }
    if queued_teardown {
        for trace in [&first_trace, &second_trace] {
            let q = trace.observation.queue_teardown.as_ref().unwrap();
            assert_eq!(q.queue_name, b"stock-audio-close");
            assert!(q.entered_current && q.exited_current && !q.caller_current_after);
            assert_eq!(q.started_at_us, q.returned_at_us);
            assert!(q.pumps > 0);
        }
    }
    if let Some(reference) = reference {
        // This variant selects the stock 2x pre-amplifier, not a custom DSP.
        // Compare actual decoded PCM to the same unprocessed application;
        // configuration readback alone cannot prove installed processing.
        let energy = |frames: &[FrameObservation]| {
            frames
                .iter()
                .flat_map(|frame| &frame.samples)
                .map(|&sample| {
                    let sample = i64::from(sample);
                    (sample * sample) as u64
                })
                .sum::<u64>()
        };
        assert_eq!(
            reference.observation.media_elapsed_us, first_trace.observation.media_elapsed_us,
            "processing comparison must cover the same virtual media duration"
        );
        for index in 0..2 {
            assert_eq!(
                reference.observation.frames[index].len(),
                first_trace.observation.frames[index].len(),
                "processing comparison must use equal numbers of returned PCM blocks"
            );
            let plain = energy(&reference.observation.frames[index]);
            let processed = energy(&first_trace.observation.frames[index]);
            assert!(plain > 0);
            assert!(
                processed > plain * 2,
                "builtin pre-amplifier did not affect received PCM: receiver={index}, plain_energy={plain}, processed_energy={processed}"
            );
            assert_ne!(
                reference.observation.frames[index],
                first_trace.observation.frames[index]
            );
        }
    }
    let first = first_trace.observation;
    let second = second_trace.observation;
    let packet_difference = first
        .packets
        .iter()
        .zip(&second.packets)
        .position(|(a, b)| a != b);
    assert!(
        first == second,
        "native PCM replay differs: events_equal={}, packets_equal={}, frames_equal={}, tones={:?}, packets={:?}, elapsed_us={:?}, first_packet_difference={packet_difference:?}, packet_counts={:?}",
        first.events == second.events,
        first.packets == second.packets,
        first.frames == second.frames,
        [first.tone_frames, second.tone_frames],
        [first.rtp_packets, second.rtp_packets],
        [first.media_elapsed_us, second.media_elapsed_us],
        [first.packets.len(), second.packets.len()],
    );
}

pub fn check_sustained_replay(impaired: bool, trace_variable: &str) {
    check_codec_sustained_replay("opus", 48000, impaired, trace_variable);
}

pub fn check_codec_sustained_replay(
    codec_name: &str,
    decoded_sample_rate_hz: u32,
    impaired: bool,
    trace_variable: &str,
) {
    let first = run_codec_case(
        codec_name,
        decoded_sample_rate_hz,
        None,
        300,
        true,
        impaired,
        false,
    );
    let second = run_codec_case(
        codec_name,
        decoded_sample_rate_hz,
        None,
        300,
        true,
        impaired,
        false,
    );
    if let Some(path) = std::env::var_os(trace_variable) {
        let path = std::path::PathBuf::from(path);
        std::fs::write(&path, serde_json::to_vec(&first).unwrap()).unwrap();
        std::fs::write(
            path.with_extension("second.json"),
            serde_json::to_vec(&second).unwrap(),
        )
        .unwrap();
    }
    for trace in [&first, &second] {
        let v = &trace.observation;
        assert_eq!(v.captures_at_us.len(), 300);
        assert_eq!(v.media_elapsed_us, 3_000_000);
        assert!(v.rtp_packets.iter().all(|&count| count >= 100));
        assert!(v.tone_frames.iter().all(|&count| count >= 100));
        let periodic = v.periodic.as_ref().unwrap();
        assert_eq!(periodic.reports.created.len(), 2);
        assert_eq!(periodic.reports.started.len(), 2);
        assert_eq!(periodic.reports.stopped.len(), 2);
        assert_eq!(periodic.reports.dropped.len(), 2);
        for index in 0..2 {
            let before = &periodic.before[index];
            let after = &periodic.after[index];
            assert!(after.timestamp_us > before.timestamp_us);
            let inbound = after
                .inbound
                .iter()
                .find(|v| v.kind.as_deref() == Some(b"audio"))
                .expect("actual native inbound audio stats");
            let outbound = after
                .outbound
                .iter()
                .find(|v| v.kind.as_deref() == Some(b"audio"))
                .expect("actual native outbound audio stats");
            assert!(inbound.packets_received.unwrap() >= 100);
            assert!(outbound.packets_sent.unwrap() >= 100);
            let lost = inbound.packets_lost.unwrap();
            if impaired {
                assert!(
                    lost > 0,
                    "explicit RTP drops were not visible in native loss stats"
                );
            } else {
                assert_eq!(lost, 0);
            }
            let created_at = periodic
                .reports
                .created
                .iter()
                .find(|&&(peer, _)| peer == index)
                .unwrap()
                .1;
            for incoming in [false, true] {
                assert!(
                    periodic.reports.packets.iter().any(|report| {
                        report.peer == index
                            && report.incoming == incoming
                            && report.observed_at_us >= created_at + 2_500_000
                            && report.observed_at_us as u128
                                >= periodic.media_started_at_us + 1_000_000
                            && periodic::packet_types(&report.bytes)
                                .iter()
                                .any(|&kind| kind == 200 || kind == 201)
                    }),
                    "missing actual late native SR/RR: peer={index}, incoming={incoming}"
                );
            }
        }
        let dropped = v
            .packets
            .iter()
            .filter(|p| p.decision == Some("drop"))
            .count();
        if impaired {
            assert!(dropped > 0);
            assert!(
                v.packets
                    .iter()
                    .filter(|p| p.decision == Some("drop"))
                    .all(|p| p.rtp_header.is_some())
            );
        } else {
            assert_eq!(dropped, 0);
        }
    }
    assert!(
        first.observation == second.observation,
        "sustained PCM replay differs: codec={codec_name}, impaired={impaired}, events_equal={}, frames_equal={}, packets_equal={}, periodic_equal={}",
        first.observation.events == second.observation.events,
        first.observation.frames == second.observation.frames,
        first.observation.packets == second.observation.packets,
        first.observation.periodic == second.observation.periodic,
    );
}
