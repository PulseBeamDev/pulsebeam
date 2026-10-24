use pulsebeam_webrtc_sys::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Wake, Waker},
    time::{Duration, Instant},
};
static SERIAL: Mutex<()> = Mutex::new(());
#[path = "production_sdk_wake_drop/codec_queue.rs"]
mod codec_queue;
#[path = "production_sdk_wake_drop/event_queue.rs"]
mod event_queue;

struct Marker(Arc<AtomicBool>);
impl Drop for Marker {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
#[derive(Clone)]
struct Trials(Arc<Marker>);
impl FieldTrialsView for Trials {
    fn lookup(&self, _: &str) -> String {
        let _ = &self.0;
        String::new()
    }
    fn create_copy(&self) -> Arc<dyn FieldTrialsView> {
        Arc::new(self.clone())
    }
}
struct DropOwner {
    owner: Arc<Mutex<Option<ProductionSession>>>,
    caller: std::thread::ThreadId,
    sdk_wake: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}
impl Wake for DropOwner {
    fn wake(self: Arc<Self>) {
        if let Some(owner) = self.owner.lock().unwrap().take() {
            self.sdk_wake
                .store(std::thread::current().id() != self.caller, Ordering::SeqCst);
            drop(owner);
            self.finished.store(true, Ordering::SeqCst);
        }
    }
}

// SDK roles cannot be synchronously retired by their own callback. Exercise
// an actual native signaling fence, not a caller-thread notification.
#[test]
fn final_session_owner_can_drop_from_the_actual_sdk_readiness_callback() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let released = Arc::new(AtomicBool::new(false));
    let marker = Arc::new(Marker(released.clone()));
    let weak = Arc::downgrade(&marker);
    let mut session = ProductionSession::builder(Default::default())
        .pcm_audio()
        .field_trials(Trials(marker))
        .build()
        .unwrap();
    let peer = session.create_peer(Default::default()).unwrap();
    // Populate the expanded main resource graph before the final-owner SDK
    // callback. A handoff that moves only the older peer/channel graph would
    // drop these affine aliases on that callback stack or retain the factory.
    let pcm = session.create_pcm_source().unwrap();
    let raw = session.create_raw_video_source().unwrap();
    let audio_track = session.create_source_track(pcm, "retiring-audio").unwrap();
    let video_track = session.create_source_track(raw, "retiring-video").unwrap();
    let _stream = session.create_media_stream("retiring-stream").unwrap();
    let _audio_view = session.audio_source_view(pcm).unwrap();
    let video_view = session.video_source_view(raw).unwrap();
    let _source_sink = session.attach_video_source_sink(video_view).unwrap();
    let _track_sink = session.attach_video_track_sink(video_track).unwrap();
    let _buffer = session.create_writable_i420_buffer(2, 2).unwrap();
    // Keep the audio track ID live in the actor without publishing a media leg
    // or adding physical input/output. These are owning graph resources only.
    let _ = audio_track;
    #[cfg(all(feature = "native", target_os = "linux"))]
    {
        let camera = session.create_camera_options().unwrap();
        session.clone_camera_options(camera).unwrap();
        let desktop = session.create_desktop_options(false, true).unwrap();
        session.clone_desktop_options(desktop).unwrap();
    }
    // Retire setup tasks and consume a real SDK result before registering the
    // final-owner waker. The next legacy result is delivered by the SDK fence,
    // after its synchronous initiating proxy call has returned to the caller.
    let warmup = session
        .request_legacy_stats(peer, LegacyStatsOutputLevel::Standard)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if matches!(session.try_peer_event(peer).unwrap(),Some(SessionEvent::LegacyStats(value)) if value.operation_id == warmup)
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    while session.try_peer_event(peer).unwrap().is_some() {}
    let owner = Arc::new(Mutex::new(Some(session)));
    let finished = Arc::new(AtomicBool::new(false));
    let sdk_wake = Arc::new(AtomicBool::new(false));
    let waker = Waker::from(Arc::new(DropOwner {
        owner: owner.clone(),
        caller: std::thread::current().id(),
        sdk_wake: sdk_wake.clone(),
        finished: finished.clone(),
    }));
    {
        let mut guard = owner.lock().unwrap();
        let session = guard.as_mut().unwrap();
        while session
            .poll_ready(&mut Context::from_waker(&waker))
            .is_ready()
        {}
        session
            .request_legacy_stats(peer, LegacyStatsOutputLevel::Debug)
            .unwrap();
    }
    while !finished.load(Ordering::SeqCst) || !released.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "SDK-callback cleanup did not release its origin environment"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        sdk_wake.load(Ordering::SeqCst),
        "fixture did not enter the SDK callback thread"
    );
    assert!(owner.lock().unwrap().is_none());
    assert!(weak.upgrade().is_none());
    // The production lease drops after native factory, role threads and
    // providers. Reacquisition proves deferred teardown actually finished.
    loop {
        match ControlledWorld::acquire(944, Duration::from_secs(4)) {
            Ok(world) => {
                assert_eq!(world.now(), Duration::from_secs(4));
                break;
            }
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "owned SDK roles were retained after cleanup"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

#[derive(Default)]
struct ForeignStop {
    output: Mutex<Option<EventLogOutput>>,
    completion: Mutex<Option<EventLogStopCompletion>>,
}
struct ForeignLogger(Arc<ForeignStop>);
struct ForeignFactory(Arc<ForeignStop>);
impl RtcEventLogFactory for ForeignFactory {
    fn create(&self, _: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        Box::new(ForeignLogger(self.0.clone()))
    }
}
impl RtcEventLogger for ForeignLogger {
    fn start(&self, output: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        *self.0.output.lock().unwrap() = Some(output);
        true
    }
    fn stop(&self) {
        drop(self.0.output.lock().unwrap().take());
    }
    fn stop_async(&self, completion: EventLogStopCompletion) {
        self.stop();
        *self.0.completion.lock().unwrap() = Some(completion);
    }
    fn log(&self, _: RtcEvent) {}
}
struct UnwrittenOutput;
impl EventLogOutputSink for UnwrittenOutput {
    fn is_active(&self) -> bool {
        true
    }
    fn write(&mut self, _: &[u8]) -> bool {
        false
    }
}

#[test]
fn foreign_native_stop_completion_wake_retires_peerless_and_closed_actor_owners() {
    use std::{future::Future, pin::Pin, task::Poll};
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    for closed in [false, true] {
        let released = Arc::new(AtomicBool::new(false));
        let marker = Arc::new(Marker(released.clone()));
        let weak = Arc::downgrade(&marker);
        let mut actor = ProductionSession::builder(Default::default())
            .field_trials(Trials(marker))
            .build()
            .unwrap();
        if closed {
            actor.shutdown().unwrap();
        }
        let owner = Arc::new(Mutex::new(Some(actor)));
        let finished = Arc::new(AtomicBool::new(false));
        let foreign_wake = Arc::new(AtomicBool::new(false));
        let waker = Waker::from(Arc::new(DropOwner {
            owner: owner.clone(),
            caller: std::thread::current().id(),
            sdk_wake: foreign_wake.clone(),
            finished: finished.clone(),
        }));
        // This independent selected logger supplies a real native async-stop
        // completion. Its producer is foreign to all actor-owned SDK queues;
        // no synthetic Readiness::notify or callback-scope probe is used.
        let environment = Environment::builder().build().unwrap();
        let pending = Arc::new(ForeignStop::default());
        let factory = RtcEventLogFactoryHandle::from_provider(ForeignFactory(pending.clone()));
        let mut logger = factory.create(&environment).unwrap();
        logger
            .start(
                EventLogOutput::from_sink(UnwrittenOutput),
                EventLogOutputPeriod::Immediate,
            )
            .unwrap();
        let mut stop = logger.stop_async().unwrap();
        assert_eq!(
            Pin::new(&mut stop).poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        );
        let completion = pending
            .completion
            .lock()
            .unwrap()
            .take()
            .expect("native logger did not supply its actual stop completion");
        std::thread::spawn(move || {
            assert!(
                TaskQueueContext::current().is_none(),
                "not a foreign producer thread"
            );
            completion.complete();
        })
        .join()
        .unwrap();
        assert!(finished.load(Ordering::SeqCst) && foreign_wake.load(Ordering::SeqCst));
        assert!(owner.lock().unwrap().is_none());
        assert_eq!(stop.try_take(), Some(Ok(())));
        drop((stop, logger, factory, environment, pending, waker));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !released.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "foreign callback retained the retired actor environment"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(weak.upgrade().is_none());
        loop {
            if let Ok(world) = ControlledWorld::acquire(946, Duration::from_secs(4)) {
                drop(world);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "foreign wake retirement retained SDK role threads/production lease"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
