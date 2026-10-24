use pulsebeam_webrtc_sys::{
    EventLogEnvironment, EventLogError, EventLogOutput, EventLogOutputPeriod, EventLogOutputSink,
    PeerConfiguration, ProductionSession, ProductionSessionConfig, RtcEvent, RtcEventLogFactory,
    RtcEventLogger,
};
use std::{
    panic::panic_any,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

// Catching the original panic is insufficient if its payload's destructor
// can unwind out of the generated Rust/C++ trampoline a second time.
struct PanicPayload;
impl Drop for PanicPayload {
    fn drop(&mut self) {
        panic!("untrusted payload destructor was invoked");
    }
}
struct Sink {
    drops: Arc<AtomicUsize>,
    panic_write: bool,
}
impl EventLogOutputSink for Sink {
    fn is_active(&self) -> bool {
        true
    }
    fn write(&mut self, _: &[u8]) -> bool {
        if self.panic_write {
            panic_any(PanicPayload);
        }
        true
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        panic_any(PanicPayload);
    }
}

#[test]
fn output_callback_and_destructor_panics_do_not_cross_native_boundary() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut output = EventLogOutput::from_sink(Sink {
        drops: drops.clone(),
        panic_write: true,
    });
    assert!(output.is_active());
    assert!(!output.write(b"first write"));
    assert!(!output.is_active());
    assert!(!output.write(b"deactivated"));
    output.flush();
    drop(output);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[derive(Default)]
struct Counts {
    factory_drops: AtomicUsize,
    logger_drops: AtomicUsize,
    stops: AtomicUsize,
    sink_drops: Arc<AtomicUsize>,
}
struct Factory(Arc<Counts>);
impl RtcEventLogFactory for Factory {
    fn create(&self, _: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        Box::new(Logger {
            counts: self.0.clone(),
            output: Mutex::new(None),
        })
    }
}
impl Drop for Factory {
    fn drop(&mut self) {
        self.0.factory_drops.fetch_add(1, Ordering::SeqCst);
        panic_any(PanicPayload);
    }
}
struct Logger {
    counts: Arc<Counts>,
    output: Mutex<Option<EventLogOutput>>,
}
impl RtcEventLogger for Logger {
    fn start(&self, output: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        *self.output.lock().unwrap() = Some(output);
        true
    }
    fn stop(&self) {
        self.counts.stops.fetch_add(1, Ordering::SeqCst);
        // The sink destructor also panics. Its native boundary must contain
        // that panic before this cleanup callback deliberately panics.
        drop(self.output.lock().unwrap().take());
        panic_any(PanicPayload);
    }
    fn log(&self, _: RtcEvent) {}
}
impl Drop for Logger {
    fn drop(&mut self) {
        self.counts.logger_drops.fetch_add(1, Ordering::SeqCst);
        panic_any(PanicPayload);
    }
}

#[test]
fn installed_logger_cleanup_and_factory_destruction_survive_actor_migration() {
    let counts = Arc::new(Counts::default());
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .event_log_factory(Factory(counts.clone()))
        .build()
        .unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    session
        .start_peer_event_log(
            peer,
            EventLogOutput::from_sink(Sink {
                drops: counts.sink_drops.clone(),
                panic_write: false,
            }),
            Some(EventLogOutputPeriod::Immediate),
        )
        .unwrap();
    let observed = counts.clone();
    std::thread::spawn(move || {
        assert_eq!(
            session.stop_peer_event_log(peer),
            Err(EventLogError::ProviderFailed)
        );
        assert!(observed.stops.load(Ordering::SeqCst) >= 1);
        assert_eq!(observed.sink_drops.load(Ordering::SeqCst), 1);
        // A cleanup panic latches logger failure, not a fabricated successful
        // subsequent start. Rejection still destroys the consumed output.
        assert_eq!(
            session.start_peer_event_log(
                peer,
                EventLogOutput::from_sink(Sink {
                    drops: observed.sink_drops.clone(),
                    panic_write: false,
                }),
                Some(EventLogOutputPeriod::Immediate)
            ),
            Err(EventLogError::ProviderFailed)
        );
        assert_eq!(observed.sink_drops.load(Ordering::SeqCst), 2);
        session.shutdown().unwrap();
        drop(session);
    })
    .join()
    .unwrap();
    assert_eq!(counts.factory_drops.load(Ordering::SeqCst), 1);
    assert_eq!(counts.logger_drops.load(Ordering::SeqCst), 1);
}
