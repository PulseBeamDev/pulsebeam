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
                        snapshots.len() < 8,
                        "fixture only submits eight actual source frames"
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
                                assert!(snapshots.len() < 8);
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

    // Above VGA: host-derived cores can select native VPx worker pthreads.
    // Do not let a tiny source make an uncontrolled host default look safe.
    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 720;
    let y = (WIDTH * HEIGHT) as usize;
    let mut pixels = vec![128; y * 3 / 2];
    for row in 0..HEIGHT as usize {
        for col in 0..WIDTH as usize {
            pixels[row * WIDTH as usize + col] = if (row / 90 + col / 160) % 2 == 0 {
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
        if submitted < 8 && current >= next_capture {
            let frame =
                VideoFrame::i420(WIDTH, HEIGHT, pixels.clone(), current.as_micros() as i64, 0)
                    .unwrap();
            call.sessions[0].push_raw_video(source, &frame).unwrap();
            submitted += 1;
            next_capture += Duration::from_millis(33);
        }
        progress();
        call.drain();
        if dual_observation && (!encoded_overflow || encoded_callbacks.lock().unwrap().len() >= 6) {
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
        if (!overflow || callbacks.lock().unwrap().len() >= 6)
            && let Some(frame) = call.sessions[1].try_raw_video_frame(receiver).unwrap()
            && received.len() < 2
        {
            assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT));
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
            assert!(encoded_callbacks.lock().unwrap().len() >= 6);
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
