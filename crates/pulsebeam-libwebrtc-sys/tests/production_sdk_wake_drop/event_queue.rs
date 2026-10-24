//! Final actor ownership from an actual SDK-internal rtc_event_log queue.
use super::*;
use std::sync::mpsc;

#[derive(Default)]
struct OutputObserved {
    bytes: Mutex<Vec<u8>>,
    queue: Mutex<Vec<u8>>,
    dropped_queue: Mutex<Vec<u8>>,
    callback_returning: AtomicBool,
    dropped: AtomicBool,
    drop_after_callback: AtomicBool,
}
struct DropFromOutput {
    owner: Arc<Mutex<Option<ProductionSession>>>,
    observed: Arc<OutputObserved>,
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
impl EventLogOutputSink for DropFromOutput {
    fn is_active(&self) -> bool {
        true
    }
    fn write(&mut self, bytes: &[u8]) -> bool {
        self.observed.bytes.lock().unwrap().extend_from_slice(bytes);
        let owner = self.owner.lock().unwrap().take();
        if let Some(owner) = owner {
            let queue = TaskQueueContext::current()
                .expect("native log output did not run on its real SDK queue");
            *self.observed.queue.lock().unwrap() = queue.name_bytes().to_vec();
            drop(owner);
            // Hold this actual output borrow open after final-owner handoff.
            // Teardown needs this same queue and must not reclaim its sink.
            self.entered.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(10)).unwrap();
            self.observed
                .callback_returning
                .store(true, Ordering::SeqCst);
        }
        true
    }
}
impl Drop for DropFromOutput {
    fn drop(&mut self) {
        *self.observed.dropped_queue.lock().unwrap() = TaskQueueContext::current()
            .map(|queue| queue.name_bytes().to_vec())
            .unwrap_or_default();
        self.observed.drop_after_callback.store(
            self.observed.callback_returning.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        self.observed.dropped.store(true, Ordering::SeqCst);
    }
}

#[test]
fn actual_internal_log_queue_final_owner_drop_waits_for_output_callback_and_retires_on_role() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let released = Arc::new(AtomicBool::new(false));
    let marker = Arc::new(Marker(released.clone()));
    let weak = Arc::downgrade(&marker);
    let mut actor = ProductionSession::builder(Default::default())
        .field_trials(Trials(marker))
        .event_logging(true)
        .build()
        .unwrap();
    let peer = actor.create_peer(Default::default()).unwrap();
    let owner = Arc::new(Mutex::new(Some(actor)));
    let observed = Arc::new(OutputObserved::default());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    {
        let mut guard = owner.lock().unwrap();
        // The pinned SDK StartLogging posts EncodeLogStart/WriteToOutput on
        // rtc_event_log and returns without waiting for that asynchronous write.
        guard
            .as_mut()
            .unwrap()
            .start_peer_event_log(
                peer,
                EventLogOutput::from_sink(DropFromOutput {
                    owner: owner.clone(),
                    observed: observed.clone(),
                    entered: entered_tx,
                    release: release_rx,
                }),
                Some(EventLogOutputPeriod::Immediate),
            )
            .unwrap();
    }
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("final actor Drop self-joined its SDK-internal log queue");
    assert!(owner.lock().unwrap().is_none());
    assert_eq!(*observed.queue.lock().unwrap(), b"rtc_event_log");
    assert!(
        !observed.dropped.load(Ordering::SeqCst),
        "native teardown reclaimed a still-executing output borrow"
    );
    assert!(
        !observed.bytes.lock().unwrap().is_empty(),
        "the real logger produced no encoded start event"
    );
    assert!(
        ControlledWorld::acquire(949, Duration::from_secs(3)).is_err(),
        "role/production lease released before internal callback returned"
    );
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !observed.dropped.load(Ordering::SeqCst) || !released.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "internal SDK queue retirement retained its sink/environment"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(observed.drop_after_callback.load(Ordering::SeqCst));
    assert_eq!(
        *observed.dropped_queue.lock().unwrap(),
        b"rtc_event_log",
        "selected output must be released by the same SDK queue after its active callback"
    );
    assert!(weak.upgrade().is_none());
    loop {
        if let Ok(world) = ControlledWorld::acquire(950, Duration::from_secs(3)) {
            drop(world);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "internal queue retirement did not release its SDK roles/production lease"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
