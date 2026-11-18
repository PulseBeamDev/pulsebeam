//! Shared high-resolution stock-software-video application and outer harness.
//! Codec preferences are explicit test inputs, not runtime admission rules.
use pulsebeam_webrtc_sys::*;
use serde::Serialize;
use std::{
    collections::{BTreeSet, HashMap},
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Call<R: SessionRuntime> {
    sessions: [Session<R>; 2],
    peers: [SessionPeerId; 2],
    operations: HashMap<(usize, OperationId), Option<SessionDescription>>,
    gathered: [bool; 2],
    connected: [bool; 2],
    receiver: Option<SessionReceiverId>,
    events: Vec<String>,
}
impl<R: SessionRuntime> Call<R> {
    fn new(runtimes: [R; 2], resources: Option<ExecutionResources>) -> Self {
        let mut sessions = runtimes.map(|runtime| {
            let mut builder = Session::with_runtime(runtime, SessionConfig::default());
            if let Some(resources) = resources {
                builder = builder.execution_resources(resources);
            }
            builder.build().unwrap()
        });
        let peers = sessions
            .each_mut()
            .map(|session| session.create_peer(PeerConfiguration::default()).unwrap());
        Self {
            sessions,
            peers,
            operations: HashMap::new(),
            gathered: [false; 2],
            connected: [false; 2],
            receiver: None,
            events: Vec::new(),
        }
    }
    fn drain(&mut self) {
        for index in 0..2 {
            while let Some(event) = self.sessions[index]
                .try_peer_event(self.peers[index])
                .unwrap()
            {
                match event {
                    SessionEvent::OperationComplete(done) => {
                        assert!(
                            self.operations
                                .insert((index, done.operation_id), done.result.unwrap())
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
                    SessionEvent::Track { receiver, .. } if index == 1 => {
                        assert_eq!(
                            self.sessions[index].receiver_media_type(receiver).unwrap(),
                            MediaType::Video
                        );
                        assert!(self.receiver.replace(receiver).is_none());
                        self.events.push("1:video-track".into());
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
        progress: &mut impl FnMut(),
    ) -> Option<SessionDescription> {
        for _ in 0..10_000 {
            self.drain();
            if let Some(description) = self.operations.remove(&(index, operation)) {
                return description;
            }
            progress();
        }
        panic!("native operation stalled: peer {index}");
    }
    fn gather(
        &mut self,
        index: usize,
        description: SessionDescription,
        progress: &mut impl FnMut(),
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
            progress();
        }
        panic!("native gathering stalled: peer {index}");
    }
    fn negotiate(&mut self, codec_name: &str, progress: &mut impl FnMut()) {
        let operation = self.sessions[0].create_offer(self.peers[0], false).unwrap();
        let offer = self.finish(0, operation, progress).unwrap();
        assert!(offer.sdp.contains(&format!("{codec_name}/90000")));
        for other in ["VP8", "VP9", "H264", "AV1"] {
            if other != codec_name {
                assert!(!offer.sdp.contains(&format!("{other}/90000")));
            }
        }
        let offer = self.gather(0, offer, progress);
        let operation = self.sessions[1]
            .set_remote_description(self.peers[1], offer)
            .unwrap();
        self.finish(1, operation, progress);
        let operation = self.sessions[1].create_answer(self.peers[1]).unwrap();
        let answer = self.finish(1, operation, progress).unwrap();
        let answer = self.gather(1, answer, progress);
        let operation = self.sessions[0]
            .set_remote_description(self.peers[0], answer)
            .unwrap();
        self.finish(0, operation, progress);
        for _ in 0..10_000 {
            self.drain();
            if self.connected == [true; 2] && self.receiver.is_some() {
                return;
            }
            progress();
        }
        panic!("native peers did not connect: {:?}", self.events);
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct FrameObservation {
    width: u32,
    height: u32,
    timestamp_us: i64,
    rtp_timestamp: u32,
    visible_i420: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObservationMode {
    RawOnly,
    DualCloseSink,
    DualClosePeer,
    DualShutdown,
    CallbackCloseSink,
    CallbackClosePeer,
    CallbackShutdown,
    CallbackOverflow,
    CallbackPanic,
    EncodedCallbackCloseSink,
    EncodedCallbackClosePeer,
    EncodedCallbackShutdown,
    EncodedCallbackOverflow,
    EncodedCallbackPanic,
}

struct ObserverDrop(Arc<AtomicBool>);
impl Drop for ObserverDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct Observation {
    events: Vec<String>,
    frames: Vec<FrameObservation>,
    encoded: Vec<(u32, Vec<u8>)>,
    callbacks: Vec<FrameObservation>,
    encoded_callbacks: Vec<(u32, Vec<u8>)>,
    observed_at_us: u128,
    media_elapsed_us: u128,
}

fn application<R: SessionRuntime>(
    codec_name: &str,
    parameters: &[(&str, &str)],
    runtimes: [R; 2],
    resources: Option<ExecutionResources>,
    mode: ObservationMode,
    mut progress: impl FnMut(),
    mut now: impl FnMut() -> Duration,
    mut after_shutdown: impl FnMut(),
) -> Observation {
    let dual_observation = mode != ObservationMode::RawOnly;
    let encoded_callback_mode = matches!(
        mode,
        ObservationMode::EncodedCallbackCloseSink
            | ObservationMode::EncodedCallbackClosePeer
            | ObservationMode::EncodedCallbackShutdown
            | ObservationMode::EncodedCallbackOverflow
            | ObservationMode::EncodedCallbackPanic
    );
    let callback_mode = encoded_callback_mode
        || matches!(
            mode,
            ObservationMode::CallbackCloseSink
                | ObservationMode::CallbackClosePeer
                | ObservationMode::CallbackShutdown
                | ObservationMode::CallbackOverflow
                | ObservationMode::CallbackPanic
        );
    let encoded_overflow = mode == ObservationMode::EncodedCallbackOverflow;
    let overflow = encoded_overflow || mode == ObservationMode::CallbackOverflow;
    let frame_limit = if encoded_overflow { 68 } else { 8 };
    let callbacks = Arc::new(Mutex::new(Vec::<FrameObservation>::new()));
    let encoded_callbacks = Arc::new(Mutex::new(Vec::new()));
    let observer_dropped = Arc::new(AtomicBool::new(false));
    let encoded_observer_dropped = Arc::new(AtomicBool::new(false));
    let mut call = Call::new(runtimes, resources);
    let source = call.sessions[0].create_raw_video_source().unwrap();
    let transceiver = call.sessions[0]
        .publish_video(
            call.peers[0],
            source,
            &format!("raw-{}-720p", codec_name.to_ascii_lowercase()),
            RtpTransceiverDirection::SendOnly,
            &[],
        )
        .unwrap();
    let codec = call.sessions[0]
        .get_rtp_sender_capabilities(MediaType::Video)
        .unwrap()
        .codecs()
        .into_iter()
        .find(|codec| {
            let base = codec.base_codec();
            base.name_bytes() == codec_name.as_bytes()
                && parameters.iter().all(|(key, value)| {
                    base.parameters().iter().any(|parameter| {
                        parameter.key == key.as_bytes() && parameter.value == value.as_bytes()
                    })
                })
        })
        .expect("stock sender capability must contain the requested codec/profile");
    call.sessions[0]
        .set_rtp_codec_preferences(transceiver, &[codec])
        .unwrap();
    call.sessions[0]
        .set_peer_bitrate(
            call.peers[0],
            Some(2_000_000),
            Some(3_000_000),
            Some(4_000_000),
        )
        .unwrap();
    call.negotiate(codec_name, &mut progress);
    let receiver = call.receiver.unwrap();
    if dual_observation {
        if callback_mode {
            {
                let snapshots = callbacks.clone();
                let guard = ObserverDrop(observer_dropped.clone());
                let mut last_native_time = None;
                let callback = move |observation: DecodedVideoFrameObservation| {
                    let _ = &guard;
                    assert!(
                        (7_000_000..17_000_000).contains(&observation.native_observed_at_us),
                        "callback must report actual controlled native clock, not host clock"
                    );
                    assert!(
                        last_native_time
                            .is_none_or(|last| last <= observation.native_observed_at_us)
                    );
                    last_native_time = Some(observation.native_observed_at_us);
                    if mode == ObservationMode::CallbackPanic {
                        panic!("intentional decoded observer panic");
                    }
                    let frame = observation.frame.expect("actual native I420 snapshot");
                    let mut snapshots = snapshots.lock().unwrap();
                    assert!(
                        snapshots.len() < frame_limit,
                        "callbacks cannot exceed submitted source frames"
                    );
                    snapshots.push(FrameObservation {
                        width: frame.width,
                        height: frame.height,
                        timestamp_us: frame.timestamp_us,
                        rtp_timestamp: frame.rtp_timestamp,
                        visible_i420: frame.buffer.as_bytes().to_vec(),
                    });
                };
                if encoded_callback_mode {
                    let snapshots = encoded_callbacks.clone();
                    let guard = ObserverDrop(encoded_observer_dropped.clone());
                    call.sessions[1]
                        .attach_raw_and_encoded_video_with_observers(
                            receiver,
                            callback,
                            move |observation| {
                                let _ = &guard;
                                if mode == ObservationMode::EncodedCallbackPanic {
                                    panic!("intentional encoded observer panic");
                                }
                                let EncodedVideoFrameObservation::Frame(frame) = observation else {
                                    panic!("lossless fixture must retain native ordered metadata");
                                };
                                assert!(frame.mime_type.eq_ignore_ascii_case("video/h264"));
                                assert!(!frame.data.is_empty());
                                let mut snapshots = snapshots.lock().unwrap();
                                assert!(snapshots.len() < frame_limit);
                                if snapshots.is_empty() {
                                    assert!(frame.key_frame);
                                }
                                snapshots.push((frame.rtp_timestamp, frame.data));
                            },
                        )
                        .unwrap();
                } else {
                    call.sessions[1]
                        .attach_raw_and_encoded_video_with_observer(receiver, callback)
                        .unwrap();
                }
            }
        } else {
            call.sessions[1]
                .attach_raw_and_encoded_video(receiver)
                .unwrap();
        }
        assert!(call.sessions[1].attach_raw_video(receiver).is_err());
        assert!(call.sessions[1].attach_encoded_video(receiver).is_err());
        assert!(
            call.sessions[1]
                .raw_video_sink_wants(receiver)
                .unwrap()
                .is_some()
        );
        call.sessions[1]
            .update_raw_video_sink_wants(receiver, &VideoSinkWants::default())
            .unwrap();
    } else {
        call.sessions[1].attach_raw_video(receiver).unwrap();
    }

    // Keep the codec-worker proofs above VGA. The encoded-overflow proof needs
    // more frames, not more pixels: its smaller source reduces total codec work.
    let (width, height) = if encoded_overflow {
        (320, 180)
    } else {
        (1280, 720)
    };
    let release_after = if encoded_overflow { 66 } else { 6 };
    let y = (width * height) as usize;
    let mut pixels = vec![128; y * 3 / 2];
    for row in 0..height as usize {
        for col in 0..width as usize {
            pixels[row * width as usize + col] = if (row / 90 + col / 160) % 2 == 0 {
                40
            } else {
                180
            };
        }
    }
    let media_started_at = now();
    let mut next_capture = media_started_at;
    let mut submitted = 0;
    let mut received = Vec::new();
    let mut encoded_timestamps = BTreeSet::new();
    let mut encoded = Vec::new();
    let mut held_encoded = None;
    let mut held_raw = None;
    for _ in 0..10_000 {
        let current = now();
        if submitted < frame_limit && current >= next_capture {
            let frame =
                VideoFrame::i420(width, height, pixels.clone(), current.as_micros() as i64, 0)
                    .unwrap();
            call.sessions[0].push_raw_video(source, &frame).unwrap();
            submitted += 1;
            next_capture += Duration::from_millis(33);
        }
        progress();
        call.drain();
        if dual_observation
            && (!encoded_overflow || encoded_callbacks.lock().unwrap().len() >= release_after)
        {
            while let Some(frame) = call.sessions[1].try_video_frame(receiver).unwrap() {
                assert!(frame.mime_type.eq_ignore_ascii_case("video/h264"));
                assert!(!frame.data.is_empty());
                encoded_timestamps.insert(frame.rtp_timestamp);
                encoded.push((frame.rtp_timestamp, frame.data.clone()));
                if held_encoded.is_none() {
                    if !encoded_overflow {
                        assert!(frame.key_frame);
                    }
                    held_encoded = Some(frame);
                }
            }
        }
        if (!overflow || callbacks.lock().unwrap().len() >= release_after)
            && let Some(frame) = call.sessions[1].try_raw_video_frame(receiver).unwrap()
            && received.len() < 2
        {
            assert_eq!((frame.width, frame.height), (width, height));
            let visible_i420 = frame.buffer.as_bytes().to_vec();
            assert_eq!(visible_i420.len(), y * 3 / 2);
            assert!(visible_i420[..y].iter().any(|&sample| sample < 80));
            assert!(visible_i420[..y].iter().any(|&sample| sample > 140));
            received.push(FrameObservation {
                width: frame.width,
                height: frame.height,
                timestamp_us: frame.timestamp_us,
                rtp_timestamp: frame.rtp_timestamp,
                visible_i420,
            });
            if dual_observation && held_raw.is_none() {
                held_raw = Some(frame);
            }
        }
        if received.len() == 2
            && (!dual_observation
                || received
                    .iter()
                    .all(|frame| encoded_timestamps.contains(&frame.rtp_timestamp)))
        {
            break;
        }
    }
    if dual_observation {
        assert!(
            received
                .iter()
                .all(|frame| encoded_timestamps.contains(&frame.rtp_timestamp)),
            "encoded and native decoded output must include the same actual RTP frames on this receiver"
        );
        if overflow {
            assert!(
                call.sessions[1].dropped_raw_video_frames(receiver).unwrap() > 0,
                "complete callbacks must survive actual polling retention loss"
            );
        } else {
            assert_eq!(call.sessions[1].dropped_frames(receiver).unwrap(), 0);
            assert_eq!(
                call.sessions[1].dropped_raw_video_frames(receiver).unwrap(),
                0
            );
        }
        assert_eq!(
            call.sessions[1]
                .raw_video_observer_failed(receiver)
                .unwrap(),
            mode == ObservationMode::CallbackPanic
        );
        if callback_mode && mode != ObservationMode::CallbackPanic {
            let snapshots = callbacks.lock().unwrap();
            assert!(
                received.iter().all(|frame| snapshots.contains(frame)),
                "callback pixels and RTP identity must match actual native polling output"
            );
        }
        let encoded_drops = call.sessions[1]
            .dropped_encoded_video_frames(receiver)
            .unwrap();
        if encoded_overflow {
            assert!(
                encoded_drops > 0,
                "native encoded callbacks must survive actual polling retention loss"
            );
            assert!(encoded_callbacks.lock().unwrap().len() >= release_after);
        } else {
            assert_eq!(encoded_drops, 0);
        }
        assert_eq!(
            call.sessions[1]
                .encoded_video_observer_failed(receiver)
                .unwrap(),
            mode == ObservationMode::EncodedCallbackPanic
        );
        if encoded_callback_mode && mode != ObservationMode::EncodedCallbackPanic {
            let snapshots = encoded_callbacks.lock().unwrap();
            assert!(
                encoded.iter().all(|frame| snapshots.contains(frame)),
                "callback bytes and RTP identity must match actual native polling output"
            );
        }
        let _ = call.sessions[1].try_raw_video_sink_event(receiver).unwrap();
        assert_eq!(
            call.sessions[1]
                .dropped_raw_video_sink_events(receiver)
                .unwrap(),
            0
        );
    }
    if matches!(
        mode,
        ObservationMode::DualCloseSink
            | ObservationMode::CallbackCloseSink
            | ObservationMode::CallbackOverflow
            | ObservationMode::CallbackPanic
            | ObservationMode::EncodedCallbackCloseSink
            | ObservationMode::EncodedCallbackOverflow
            | ObservationMode::EncodedCallbackPanic
    ) {
        call.sessions[1].close_media_sink(receiver).unwrap();
        assert!(call.sessions[1].try_raw_video_frame(receiver).is_err());
        assert!(call.sessions[1].try_video_frame(receiver).is_err());
        assert!(call.sessions[1].dropped_raw_video_frames(receiver).is_err());
        assert!(
            call.sessions[1]
                .dropped_encoded_video_frames(receiver)
                .is_err()
        );
        // Combined attachment must preserve the native encoded receiver-lifetime
        // reservation. A failed reattachment must also release its raw sink.
        assert!(
            call.sessions[1]
                .attach_raw_and_encoded_video(receiver)
                .is_err()
        );
        call.sessions[1].attach_raw_video(receiver).unwrap();
        call.sessions[1].close_media_sink(receiver).unwrap();
        assert!(call.sessions[1].attach_encoded_video(receiver).is_err());
    }
    assert_eq!(
        received.len(),
        2,
        "native sustained encode/transport/decode did not produce two distinct frames"
    );
    assert!(received[0].timestamp_us < received[1].timestamp_us);
    assert_ne!(received[0].rtp_timestamp, received[1].rtp_timestamp);
    let observed_at_us = now().as_micros();
    let media_elapsed_us = observed_at_us - media_started_at.as_micros();
    assert!(media_elapsed_us >= 33_000);
    for index in 0..2 {
        // Both other combined cases leave the sinks ATTACHED through peer
        // retirement or direct shutdown, exercising dependent cleanup too.
        if index != 1
            || !matches!(
                mode,
                ObservationMode::DualShutdown
                    | ObservationMode::CallbackShutdown
                    | ObservationMode::EncodedCallbackShutdown
            )
        {
            call.sessions[index].close_peer(call.peers[index]).unwrap();
        }
        call.sessions[index].shutdown().unwrap();
    }
    if dual_observation {
        assert!(!held_encoded.unwrap().data.is_empty());
        assert_eq!(
            held_raw.unwrap().buffer.as_bytes(),
            received[0].visible_i420.as_slice()
        );
    }
    if callback_mode {
        assert!(
            observer_dropped.load(Ordering::Acquire),
            "native close/shutdown must release callback owner"
        );
        if encoded_callback_mode {
            assert!(
                encoded_observer_dropped.load(Ordering::Acquire),
                "native close/shutdown must release encoded callback owner"
            );
        }
        let encoded_count = encoded_callbacks.lock().unwrap().len();
        let count = callbacks.lock().unwrap().len();
        after_shutdown();
        assert_eq!(
            encoded_callbacks.lock().unwrap().len(),
            encoded_count,
            "no encoded callback after native removal/shutdown"
        );
        assert_eq!(
            callbacks.lock().unwrap().len(),
            count,
            "no callback after native removal/shutdown"
        );
    }
    let callbacks = callbacks.lock().unwrap().clone();
    let encoded_callbacks = encoded_callbacks.lock().unwrap().clone();
    Observation {
        events: call.events,
        frames: received,
        encoded,
        callbacks,
        encoded_callbacks,
        observed_at_us,
        media_elapsed_us,
    }
}

fn run_observed(
    codec_name: &str,
    parameters: &[(&str, &str)],
    resources: Option<ExecutionResources>,
    mode: ObservationMode,
) -> Observation {
    eprintln!("CONTROLLED_MEDIA_BEGIN");
    let observation = {
        let world = ControlledWorld::acquire(0x7208, Duration::from_secs(7)).unwrap();
        let network = world.create_network().unwrap();
        let endpoints = ["192.0.2.1", "192.0.2.2"].map(|ip| {
            network
                .register_endpoint(ip.parse::<IpAddr>().unwrap())
                .unwrap()
        });
        let runtimes = endpoints
            .each_ref()
            .map(|endpoint| world.runtime().unwrap().with_endpoint(endpoint).unwrap());
        application(
            codec_name,
            parameters,
            runtimes,
            resources,
            mode,
            || {
                assert!(!world.pump(256).blocked);
                while let Some(packet) = network.next_packet() {
                    network.deliver(packet.id).unwrap();
                }
                // The outer caller explicitly chooses every advance and delivery.
                world.advance(Duration::from_millis(1)).unwrap();
            },
            || world.now(),
            || {
                // Endpoints may already be retired. Pump native work only:
                // delivering old packets to removed destinations is unrelated
                // to decoded callback lifetime and is correctly rejected.
                for _ in 0..10 {
                    world.advance(Duration::from_millis(1)).unwrap();
                    assert!(!world.pump(256).blocked);
                }
            },
        )
    };
    eprintln!("CONTROLLED_MEDIA_END");
    observation
}

fn run(
    codec_name: &str,
    parameters: &[(&str, &str)],
    resources: Option<ExecutionResources>,
) -> Observation {
    run_observed(codec_name, parameters, resources, ObservationMode::RawOnly)
}

/// Composition proof for the existing native encoder, ordered encoded
/// observation, and native I420 decoder. Prerecorded input is separately
/// retained and qualified by the consumer's encoded-source scenarios.
#[allow(dead_code)]
pub fn check_dual_observation() {
    let parameters = &[("packetization-mode", "1"), ("profile-level-id", "42e01f")];
    for mode in [
        ObservationMode::DualCloseSink,
        ObservationMode::DualClosePeer,
        ObservationMode::DualShutdown,
    ] {
        let first = run_observed("H264", parameters, None, mode);
        let second = run_observed("H264", parameters, None, mode);
        assert_eq!(
            first, second,
            "same-receiver dual native observation must replay: {mode:?}"
        );
    }
}

#[allow(dead_code)]
pub fn check_decoded_callbacks() {
    let parameters = &[("packetization-mode", "1"), ("profile-level-id", "42e01f")];
    for mode in [
        ObservationMode::CallbackCloseSink,
        ObservationMode::CallbackClosePeer,
        ObservationMode::CallbackShutdown,
        ObservationMode::CallbackOverflow,
        ObservationMode::CallbackPanic,
    ] {
        let first = run_observed("H264", parameters, None, mode);
        let second = run_observed("H264", parameters, None, mode);
        assert_eq!(
            first, second,
            "actual decoded callback observation must replay: {mode:?}"
        );
    }
}

#[allow(dead_code)]
pub fn check_encoded_callbacks() {
    let parameters = &[("packetization-mode", "1"), ("profile-level-id", "42e01f")];
    for mode in [
        ObservationMode::EncodedCallbackCloseSink,
        ObservationMode::EncodedCallbackClosePeer,
        ObservationMode::EncodedCallbackShutdown,
        ObservationMode::EncodedCallbackOverflow,
        ObservationMode::EncodedCallbackPanic,
    ] {
        let first = run_observed("H264", parameters, None, mode);
        let second = run_observed("H264", parameters, None, mode);
        assert_eq!(
            first, second,
            "native encoded callbacks must replay: {mode:?}"
        );
    }
}

#[allow(dead_code)]
pub fn check_multicore_replay(trace_path_variable: &str) {
    let resources = ExecutionResources {
        codec_core_budget: Some(pulsebeam_webrtc_sys::CodecCoreBudget::new(8).unwrap()),
    };
    let first = run("VP8", &[], Some(resources));
    let second = run("VP8", &[], Some(resources));
    assert!(
        first == second,
        "stock VP8 cores8 native replay differs: events_equal={}, frames_equal={}, observed_us={:?}",
        first.events == second.events,
        first.frames == second.frames,
        [first.observed_at_us, second.observed_at_us],
    );
    if let Some(path) = std::env::var_os(trace_path_variable) {
        std::fs::write(path, serde_json::to_vec(&first).unwrap()).unwrap();
    }
}

#[allow(dead_code)]
pub fn check_replay(
    codec_name: &str,
    parameters: &[(&str, &str)],
    host_probe_variable: &str,
    trace_path_variable: &str,
) {
    // Optional diagnostic negative control deliberately selects native host
    // resources, whose worker behavior is NOT qualified by this default policy.
    let observation = if std::env::var_os(host_probe_variable).is_some() {
        run(codec_name, parameters, Some(ExecutionResources::default()))
    } else {
        let first = run(codec_name, parameters, None);
        let second = run(codec_name, parameters, None);
        assert!(
            first == second,
            "native {codec_name} replay differs: events_equal={}, frames_equal={}, capture_us={:?}, observed_us={:?}, rtp={:?}",
            first.events == second.events,
            first.frames == second.frames,
            [
                first
                    .frames
                    .iter()
                    .map(|frame| frame.timestamp_us)
                    .collect::<Vec<_>>(),
                second
                    .frames
                    .iter()
                    .map(|frame| frame.timestamp_us)
                    .collect::<Vec<_>>(),
            ],
            [
                (first.observed_at_us, first.media_elapsed_us),
                (second.observed_at_us, second.media_elapsed_us),
            ],
            [
                first
                    .frames
                    .iter()
                    .map(|frame| frame.rtp_timestamp)
                    .collect::<Vec<_>>(),
                second
                    .frames
                    .iter()
                    .map(|frame| frame.rtp_timestamp)
                    .collect::<Vec<_>>(),
            ],
        );
        first
    };
    if let Some(path) = std::env::var_os(trace_path_variable) {
        // File I/O is outside native lifecycle markers, never native progress.
        std::fs::write(path, serde_json::to_vec(&observation).unwrap()).unwrap();
    }
}

#[derive(Debug)]
struct MetadataAuditResult {
    timestamp: u32,
    unchanged_write: Result<(), TransformableFrameError>,
    mismatched_variant: Result<(), TransformableFrameError>,
    mismatched_codec: Result<(), TransformableFrameError>,
    unchanged_after_write: bool,
    unchanged_after_variant: bool,
    unchanged_after_codec: bool,
    completion: Result<FrameTransformCompletionOutcome, FrameTransformCompletionError>,
}

#[derive(Default)]
struct MetadataAudit {
    started: AtomicBool,
    result: Mutex<Option<MetadataAuditResult>>,
}

impl FrameTransformerProvider for MetadataAudit {
    fn transform(&self, mut frame: TransformableMediaFrame, completion: FrameTransformCompletion) {
        if self.started.swap(true, Ordering::AcqRel) {
            completion.complete(frame).unwrap();
            return;
        }
        let timestamp = frame.rtp_timestamp();
        let video = match &mut frame {
            TransformableMediaFrame::Video(video) => video,
            _ => panic!("expected a video sender frame"),
        };
        assert_eq!(video.direction(), TransformableFrameDirection::Sender);
        let original = video.metadata().unwrap();
        let values = original.values();
        let specifics = original.codec_specifics();
        let mime = video.mime_type().unwrap();
        assert_eq!(values.codec, VideoCodecType::Vp8);
        assert!(mime.eq_ignore_ascii_case(b"video/vp8"));
        assert!(matches!(specifics, RtpVideoHeaderCodecSpecifics::Vp8(_)));

        let unchanged_write = video.set_metadata(&original);
        let unchanged = |video: &TransformableVideoFrame| {
            let actual = video.metadata().unwrap();
            actual.values() == values
                && actual.codec_specifics() == specifics
                && video.mime_type().unwrap() == mime
        };
        let unchanged_after_write = unchanged(video);
        let mut proposed = original.clone();
        proposed
            .set_codec_specifics(&RtpVideoHeaderCodecSpecifics::H264(Default::default()))
            .unwrap();
        let mismatched_variant = video.set_metadata(&proposed);
        let unchanged_after_variant = unchanged(video);
        let mut proposed_values = proposed.values();
        proposed_values.codec = VideoCodecType::H264;
        proposed.set_values(&proposed_values).unwrap();
        let mismatched_codec = video.set_metadata(&proposed);
        let unchanged_after_codec = unchanged(video);
        // Restore on an old implementation so the finding does not send a
        // wrongly typed header through the real packetizer.
        if !unchanged_after_codec || !unchanged_after_variant {
            video.set_metadata(&original).unwrap();
        }
        let completion = completion.complete(frame);
        *self.result.lock().unwrap() = Some(MetadataAuditResult {
            timestamp,
            unchanged_write,
            mismatched_variant,
            mismatched_codec,
            unchanged_after_write,
            unchanged_after_variant,
            unchanged_after_codec,
            completion,
        });
    }
}

#[derive(Debug)]
struct ReceiverMetadataAuditResult {
    timestamp: u32,
    rejection: Result<(), TransformableFrameError>,
    unchanged: bool,
    clone_direction: TransformableFrameDirection,
    clone_timestamp: u32,
    completion: Result<FrameTransformCompletionOutcome, FrameTransformCompletionError>,
}

#[derive(Default)]
struct ReceiverMetadataAudit {
    started: AtomicBool,
    result: Mutex<Option<ReceiverMetadataAuditResult>>,
}

impl FrameTransformerProvider for ReceiverMetadataAudit {
    fn transform(&self, mut frame: TransformableMediaFrame, completion: FrameTransformCompletion) {
        if self.started.swap(true, Ordering::AcqRel) {
            completion.complete(frame).unwrap();
            return;
        }
        let timestamp = frame.rtp_timestamp();
        let video = match &mut frame {
            TransformableMediaFrame::Video(video) => video,
            _ => panic!("expected a video receiver frame"),
        };
        assert_eq!(video.direction(), TransformableFrameDirection::Receiver);
        let original = video.metadata().unwrap();
        let mut proposed = original.clone();
        let mut dependencies = original.frame_dependencies();
        dependencies.push(i64::MAX);
        proposed.set_frame_dependencies(&dependencies);
        let rejection = video.set_metadata(&proposed);
        let actual = video.metadata().unwrap();
        let unchanged = actual.values() == original.values()
            && actual.codec_specifics() == original.codec_specifics()
            && actual.dependencies() == original.dependencies();
        let clone = frame.sdk_clone().unwrap();
        let clone_direction = clone.direction();
        let clone_timestamp = clone.rtp_timestamp();
        let completion = completion.complete(clone);
        *self.result.lock().unwrap() = Some(ReceiverMetadataAuditResult {
            timestamp,
            rejection,
            unchanged,
            clone_direction,
            clone_timestamp,
            completion,
        });
    }
}

#[allow(dead_code)]
pub fn check_sender_metadata_packetizer_codec() {
    let world = ControlledWorld::acquire(0x7210, Duration::from_secs(7)).unwrap();
    let network = world.create_network().unwrap();
    let endpoints = ["192.0.2.1", "192.0.2.2"].map(|ip| {
        network
            .register_endpoint(ip.parse::<IpAddr>().unwrap())
            .unwrap()
    });
    let runtimes = endpoints
        .each_ref()
        .map(|endpoint| world.runtime().unwrap().with_endpoint(endpoint).unwrap());
    let mut call = Call::new(runtimes, None);
    let source = call.sessions[0].create_raw_video_source().unwrap();
    let transceiver = call.sessions[0]
        .publish_video(
            call.peers[0],
            source,
            "metadata-vp8",
            RtpTransceiverDirection::SendOnly,
            &[],
        )
        .unwrap();
    let codec = call.sessions[0]
        .get_rtp_sender_capabilities(MediaType::Video)
        .unwrap()
        .codecs()
        .into_iter()
        .find(|codec| codec.base_codec().name_bytes() == b"VP8")
        .unwrap();
    call.sessions[0]
        .set_rtp_codec_preferences(transceiver, &[codec])
        .unwrap();
    let audit = Arc::new(MetadataAudit::default());
    let transformer = call.sessions[0]
        .create_frame_transformer(audit.clone(), 1)
        .unwrap();
    let sender = call.sessions[0].sender(transceiver).unwrap();
    call.sessions[0]
        .set_sender_frame_transformer(sender, Some(transformer))
        .unwrap();
    let mut progress = || {
        assert!(!world.pump(256).blocked);
        while let Some(packet) = network.next_packet() {
            network.deliver(packet.id).unwrap();
        }
        world.advance(Duration::from_millis(1)).unwrap();
    };
    call.negotiate("VP8", &mut progress);
    let receiver = call.receiver.unwrap();
    let receiver_audit = Arc::new(ReceiverMetadataAudit::default());
    let receiver_transformer = call.sessions[1]
        .create_frame_transformer(receiver_audit.clone(), 1)
        .unwrap();
    call.sessions[1]
        .set_receiver_frame_transformer(receiver, Some(receiver_transformer))
        .unwrap();
    call.sessions[1].attach_raw_video(receiver).unwrap();
    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 240;
    let pixels = vec![128; (WIDTH * HEIGHT * 3 / 2) as usize];
    let mut submitted = 0;
    let mut next_capture = world.now();
    let mut received = Vec::new();
    for _ in 0..10_000 {
        if submitted < 8 && world.now() >= next_capture {
            let frame = VideoFrame::i420(
                WIDTH,
                HEIGHT,
                pixels.clone(),
                world.now().as_micros() as i64,
                0,
            )
            .unwrap();
            call.sessions[0].push_raw_video(source, &frame).unwrap();
            submitted += 1;
            next_capture += Duration::from_millis(33);
        }
        progress();
        call.drain();
        while let Some(frame) = call.sessions[1].try_raw_video_frame(receiver).unwrap() {
            received.push(frame.rtp_timestamp);
        }
        if let (Some(sender), Some(receiver)) = (
            audit.result.lock().unwrap().as_ref(),
            receiver_audit.result.lock().unwrap().as_ref(),
        ) && received.contains(&sender.timestamp)
            && received.contains(&receiver.timestamp)
        {
            break;
        }
    }
    let result = audit
        .result
        .lock()
        .unwrap()
        .take()
        .expect("no SDK sender frame captured");
    assert_eq!(result.unchanged_write, Ok(()));
    assert!(
        result.unchanged_after_write,
        "unchanged write altered metadata or MIME"
    );
    assert_eq!(
        result.mismatched_variant,
        Err(TransformableFrameError::CodecVariantAssociation)
    );
    assert!(
        result.unchanged_after_variant,
        "variant rejection altered metadata or MIME"
    );
    assert!(
        result.mismatched_codec.is_err(),
        "packetizer codec mismatch accepted: {result:?}"
    );
    assert!(
        result.unchanged_after_codec,
        "codec rejection altered metadata or MIME"
    );
    assert_eq!(
        result.completion,
        Ok(FrameTransformCompletionOutcome::DeliveredToNative)
    );
    assert!(
        received.contains(&result.timestamp),
        "completed sender frame did not reach the receiver"
    );
    let receiver_result = receiver_audit
        .result
        .lock()
        .unwrap()
        .take()
        .expect("no SDK receiver frame captured");
    assert_eq!(
        receiver_result.rejection,
        Err(TransformableFrameError::UnsupportedInstalledDependencyMutation)
    );
    assert!(
        receiver_result.unchanged,
        "rejection altered receiver metadata"
    );
    assert_eq!(
        receiver_result.clone_direction,
        TransformableFrameDirection::Sender
    );
    assert_eq!(receiver_result.clone_timestamp, receiver_result.timestamp);
    assert_eq!(
        receiver_result.completion,
        Ok(FrameTransformCompletionOutcome::DeliveredToNative)
    );
    assert!(
        received.contains(&receiver_result.timestamp),
        "receiver SDK clone did not reach the decoder"
    );
    for index in 0..2 {
        call.sessions[index].close_peer(call.peers[index]).unwrap();
        call.sessions[index].shutdown().unwrap();
    }
}
