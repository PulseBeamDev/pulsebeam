//! Final actor ownership from the SDK's actual VideoEncoderQueue callback.
use super::*;
use std::{collections::HashMap, sync::mpsc};

#[derive(Default)]
struct Call {
    operations: HashMap<(SessionPeerId, OperationId), Option<SessionDescription>>,
    gathered: [bool; 2],
    connected: [bool; 2],
}
impl Call {
    fn drain(&mut self, actor: &mut ProductionSession, peers: [SessionPeerId; 2]) {
        for (index, peer) in peers.into_iter().enumerate() {
            while let Some(event) = actor.try_peer_event(peer).unwrap() {
                match event {
                    SessionEvent::OperationComplete(done) => {
                        self.operations
                            .insert((peer, done.operation_id), done.result.unwrap());
                    }
                    SessionEvent::IceGatheringStateChanged(IceGatheringState::Complete) => {
                        self.gathered[index] = true;
                    }
                    SessionEvent::ConnectionStateChanged(ConnectionState::Connected) => {
                        self.connected[index] = true;
                    }
                    _ => {}
                }
            }
        }
    }
    fn finish(
        &mut self,
        actor: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        peer: SessionPeerId,
        operation: OperationId,
    ) -> Option<SessionDescription> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.drain(actor, peers);
            if let Some(result) = self.operations.remove(&(peer, operation)) {
                return result;
            }
            assert!(
                Instant::now() < deadline,
                "native description operation timed out"
            );
            std::thread::yield_now();
        }
    }
    fn gather(
        &mut self,
        actor: &mut ProductionSession,
        peers: [SessionPeerId; 2],
        index: usize,
        description: SessionDescription,
    ) -> SessionDescription {
        let operation = actor
            .set_local_description(peers[index], description)
            .unwrap();
        self.finish(actor, peers, peers[index], operation);
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.gathered[index] {
            self.drain(actor, peers);
            assert!(Instant::now() < deadline, "native ICE gathering timed out");
            std::thread::yield_now();
        }
        let descriptions = actor.descriptions(peers[index]).unwrap();
        descriptions
            .current_local
            .or(descriptions.pending_local)
            .unwrap()
    }
    fn negotiate(&mut self, actor: &mut ProductionSession, peers: [SessionPeerId; 2]) {
        let operation = actor.create_offer(peers[0], false).unwrap();
        let offer = self.finish(actor, peers, peers[0], operation).unwrap();
        let offer = self.gather(actor, peers, 0, offer);
        let operation = actor.set_remote_description(peers[1], offer).unwrap();
        self.finish(actor, peers, peers[1], operation);
        let operation = actor.create_answer(peers[1]).unwrap();
        let answer = self.finish(actor, peers, peers[1], operation).unwrap();
        let answer = self.gather(actor, peers, 1, answer);
        let operation = actor.set_remote_description(peers[0], answer).unwrap();
        self.finish(actor, peers, peers[0], operation);
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.connected != [true; 2] {
            self.drain(actor, peers);
            assert!(
                Instant::now() < deadline,
                "real native video peers did not connect"
            );
            std::thread::yield_now();
        }
    }
}

#[derive(Default)]
struct Observed {
    queue: Mutex<Vec<u8>>,
    codec: Mutex<Vec<u8>>,
    callback_returning: AtomicBool,
    selector_dropped: AtomicBool,
    drop_after_callback: AtomicBool,
}
struct DropFromSelector {
    owner: Option<mpsc::Receiver<ProductionSession>>,
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    observed: Arc<Observed>,
}
impl VideoEncoderSelectorProvider for DropFromSelector {
    fn on_current_encoder(&mut self, format: SdpVideoFormat) {
        let Some(receiver) = self.owner.take() else {
            return;
        };
        let queue = TaskQueueContext::current()
            .expect("installed selector did not run on an actual SDK queue");
        *self.observed.queue.lock().unwrap() = queue.name_bytes().to_vec();
        *self.observed.codec.lock().unwrap() = format.fields().name;
        // The frame-cadence adapter posts OnFrame to VideoEncoderQueue; it
        // never waits for encoding. The initiating push therefore returns
        // before the caller publishes its final actor owner here. This channel
        // is a bounded test-only ownership gate, not an actor command executor.
        let actor = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(actor);
        self.entered.send(()).unwrap();
        // Hold the real selector callback open after final-owner retirement.
        // Closing the native send stream must wait for this queue to return.
        self.release.recv_timeout(Duration::from_secs(10)).unwrap();
        self.observed
            .callback_returning
            .store(true, Ordering::SeqCst);
    }
    fn on_available_bitrate(&mut self, _: DataRate) -> Option<SdpVideoFormat> {
        None
    }
    fn on_encoder_broken(&mut self) -> Option<SdpVideoFormat> {
        None
    }
}
impl Drop for DropFromSelector {
    fn drop(&mut self) {
        self.observed.drop_after_callback.store(
            self.observed.callback_returning.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        self.observed.selector_dropped.store(true, Ordering::SeqCst);
    }
}

#[test]
fn installed_selector_drops_final_actor_on_actual_codec_queue_without_self_join() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let released = Arc::new(AtomicBool::new(false));
    let marker = Arc::new(Marker(released.clone()));
    let weak = Arc::downgrade(&marker);
    let encoder = VideoEncoderFactoryHandle::builtin().unwrap();
    let decoder = VideoDecoderFactoryHandle::builtin_vp8().unwrap();
    let actor = ProductionSession::builder(Default::default())
        .field_trials(Trials(marker))
        .video_encoder_factory(&encoder)
        .video_decoder_factory(&decoder)
        .build()
        .unwrap();
    drop((encoder, decoder));
    let observed = Arc::new(Observed::default());
    let (owner_tx, owner_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let selector_observed = observed.clone();
    // Negotiate and publish after migrating the Send actor. No local native
    // product/peer handle is moved or made Send to manufacture a queue origin.
    let failure = std::thread::spawn(move || {
        let mut actor = actor;
        let peers = [
            actor.create_peer(Default::default()).unwrap(),
            actor.create_peer(Default::default()).unwrap(),
        ];
        let raw = actor.create_raw_video_source().unwrap();
        let transceiver = actor
            .publish_video(
                peers[0],
                raw,
                "codec-retirement",
                RtpTransceiverDirection::SendOnly,
                &[],
            )
            .unwrap();
        let sender = actor.sender(transceiver).unwrap();
        Call::default().negotiate(&mut actor, peers);
        let selector = OwnedVideoEncoderSelector::from_provider(DropFromSelector {
            owner: Some(owner_rx),
            entered: entered_tx,
            release: release_rx,
            observed: selector_observed,
        });
        let failure = selector.failure_observer();
        actor
            .set_sender_owned_encoder_selector(sender, Some(selector))
            .unwrap();
        // No frame was published before selector installation. The pinned SDK
        // defers encoder creation until it knows the first frame's dimensions.
        let timestamp = actor.clock_timestamp().unwrap().microseconds().unwrap();
        let mut pixels = vec![128; 64 * 48 * 3 / 2];
        pixels[..64 * 48].fill(40);
        let frame = VideoFrame::i420(64, 48, pixels, timestamp, 0).unwrap();
        actor.push_raw_video(raw, &frame).unwrap();
        owner_tx.send(actor).unwrap();
        failure
    })
    .join()
    .unwrap();
    entered_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("final actor Drop self-joined its actual SDK codec callback");
    assert_eq!(*observed.queue.lock().unwrap(), b"VideoEncoderQueue");
    assert_eq!(*observed.codec.lock().unwrap(), b"VP8");
    assert!(!observed.selector_dropped.load(Ordering::SeqCst));
    assert!(!released.load(Ordering::SeqCst));
    assert!(
        ControlledWorld::acquire(951, Duration::from_secs(3)).is_err(),
        "production lease released before the actual codec callback returned"
    );
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !observed.selector_dropped.load(Ordering::SeqCst) || !released.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "codec callback retirement retained its selected graph"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(observed.drop_after_callback.load(Ordering::SeqCst));
    // Selector destruction is permitted on another thread, unlike encoded
    // event-log output destruction. Do not invent encoder-queue affinity here.
    assert_eq!(failure.failure(), None);
    assert!(weak.upgrade().is_none());
    loop {
        if let Ok(world) = ControlledWorld::acquire(952, Duration::from_secs(3)) {
            drop(world);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "codec retirement retained its SDK roles/production lease"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
