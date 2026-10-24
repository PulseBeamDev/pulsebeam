//! SDK aliases are owning, thread-safe values, not Rc driver owners.
use pulsebeam_webrtc_sys::*;
use std::{
    net::IpAddr,
    sync::{Arc, Mutex},
    thread::{self, ThreadId},
    time::Duration,
};

struct ProviderLifetime {
    drops: Arc<Mutex<Vec<ThreadId>>>,
    panic_on_drop: bool,
}
impl Drop for ProviderLifetime {
    fn drop(&mut self) {
        self.drops.lock().unwrap().push(thread::current().id());
        assert!(!self.panic_on_drop, "injected native dependency drop panic");
    }
}
struct Trials(Arc<ProviderLifetime>);
impl FieldTrialsView for Trials {
    fn lookup(&self, _: &str) -> String {
        String::new()
    }
    fn create_copy(&self) -> Arc<dyn FieldTrialsView> {
        Arc::new(Self(self.0.clone()))
    }
}
struct Capture(Arc<Mutex<Option<Environment>>>);
impl RtcEventLogFactory for Capture {
    fn create(&self, context: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        *self.0.lock().unwrap() = Some(context.environment().clone());
        Box::new(NoOutput)
    }
}
struct NoOutput;
impl RtcEventLogger for NoOutput {
    fn start(&self, _: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        false
    }
    fn stop(&self) {}
    fn log(&self, _: RtcEvent) {}
}
struct Output {
    bytes: Arc<Mutex<Vec<u8>>>,
    drops: Arc<Mutex<Vec<ThreadId>>>,
}
impl EventLogOutputSink for Output {
    fn is_active(&self) -> bool {
        true
    }
    fn write(&mut self, bytes: &[u8]) -> bool {
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        true
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        self.drops.lock().unwrap().push(thread::current().id());
    }
}

fn exercise_native_alias_teardown(pending_stop: bool, panic_on_drop: bool) {
    let creator = thread::current().id();
    let provider_drops = Arc::new(Mutex::new(Vec::new()));
    let output_drops = Arc::new(Mutex::new(Vec::new()));
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::new(Mutex::new(None));
    let world = ControlledWorld::acquire(923, Duration::from_secs(7)).unwrap();
    let network = world.create_network().unwrap();
    let endpoint = network
        .register_endpoint("192.0.2.2".parse::<IpAddr>().unwrap())
        .unwrap();
    let runtime = world.runtime().unwrap().with_endpoint(&endpoint).unwrap();
    let mut session = Session::with_runtime(runtime, SessionConfig::default())
        .field_trials(Trials(Arc::new(ProviderLifetime {
            drops: provider_drops.clone(),
            panic_on_drop,
        })))
        .event_log_factory(Capture(captured.clone()))
        .build()
        .unwrap();
    session.create_peer(PeerConfiguration::default()).unwrap();
    let environment = captured
        .lock()
        .unwrap()
        .take()
        .expect("installed SDK factory context");
    let mut logger = RtcEventLogFactoryHandle::builtin()
        .create(&environment)
        .unwrap();
    logger
        .start(
            EventLogOutput::from_sink(Output {
                bytes: bytes.clone(),
                drops: output_drops.clone(),
            }),
            EventLogOutputPeriod::Immediate,
        )
        .unwrap();
    logger
        .log(RtcEvent::from_data(RtcEventData::AlrState { in_alr: true }).unwrap())
        .unwrap();
    let mut factory = EnvironmentFactory::from_environment(&environment).unwrap();
    factory.set_event_log_product(logger).unwrap();
    let alias = factory.create().unwrap();
    let mut stop = if pending_stop {
        let mut utility = alias.event_log().unwrap();
        let completion = utility.stop_async().unwrap();
        // The real SDK callback retains its creation context until output
        // shutdown. Dropping this view must not cancel or fabricate completion.
        drop(utility);
        Some(completion)
    } else {
        None
    };
    drop(factory);
    drop(environment);
    session.shutdown().unwrap();
    drop(session);
    drop(endpoint);
    drop(network);
    assert!(provider_drops.lock().unwrap().is_empty());

    // This thread is an explicit test caller, not a library executor. It only
    // releases an SDK-supported Send+Sync value; it never pumps managed work.
    thread::spawn(move || drop(alias)).join().unwrap();
    assert!(provider_drops.lock().unwrap().is_empty());
    assert!(output_drops.lock().unwrap().is_empty());
    if pending_stop {
        assert_eq!(stop.as_mut().unwrap().try_take(), None);
        // No preparatory pump: the final creator scope must make finite native
        // lifecycle progress, deliver completion and retire its dependencies.
        drop(world);
        assert_eq!(stop.as_mut().unwrap().try_take(), Some(Ok(())));
    } else {
        world.pump(1024);
        assert_eq!(world.now(), Duration::from_secs(7));
        drop(world);
    }
    assert_eq!(&*provider_drops.lock().unwrap(), &[creator]);
    assert_eq!(&*output_drops.lock().unwrap(), &[creator]);
    assert!(!bytes.lock().unwrap().is_empty(), "real SDK log output");

    // Final native-dependent teardown must restore the process hooks.
    let next = ControlledWorld::acquire(924, Duration::from_secs(9)).unwrap();
    assert_eq!(next.now(), Duration::from_secs(9));
}

#[test]
fn last_foreign_sdk_alias_retires_stock_logger_and_dependencies_on_creator() {
    exercise_native_alias_teardown(false, false);
    exercise_native_alias_teardown(true, false);
    exercise_native_alias_teardown(true, true);
}
