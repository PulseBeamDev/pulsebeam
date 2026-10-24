use pulsebeam_webrtc_sys::{
    Environment, EventLogEnvironment, EventLogOutput, EventLogOutputPeriod, FieldTrials,
    ManualClock, PeerConfiguration, PeerConnectionFactory, ProductionSession,
    ProductionSessionConfig, RandomnessLease, RandomnessLeaseError, RtcEvent, RtcEventLogFactory,
    RtcEventLogger,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

static RANDOMNESS: Mutex<()> = Mutex::new(());

struct Factory(Arc<Mutex<Vec<EventLogEnvironment>>>);
struct Logger;
impl RtcEventLogFactory for Factory {
    fn create(&self, environment: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        self.0.lock().unwrap().push(environment);
        Box::new(Logger)
    }
}
impl RtcEventLogger for Logger {
    fn start(&self, _: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        false
    }
    fn stop(&self) {}
    fn log(&self, _: RtcEvent) {}
}

#[test]
fn installed_creation_context_retains_rust_leases_after_native_peer_retirement() {
    let _exclusive = RANDOMNESS.lock().unwrap();
    let seed = 404;
    let lease = RandomnessLease::acquire(seed).unwrap();
    let clock = ManualClock::new(Duration::from_secs(10)).unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let environment = Environment::builder()
        .clock(&clock)
        .randomness(&lease)
        .field_trials(FieldTrials::parse("Provider-Context/Enabled/").unwrap())
        .build()
        .unwrap();
    let factory = PeerConnectionFactory::builder()
        .environment(environment)
        .event_log_factory(Factory(contexts.clone()))
        .build()
        .unwrap();
    let mut peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    assert_eq!(
        contexts.lock().unwrap().len(),
        1,
        "selected factory was not called"
    );
    peer.close().unwrap();
    drop(peer);
    drop(factory);
    drop(lease);

    assert_eq!(
        RandomnessLease::acquire(seed).err(),
        Some(RandomnessLeaseError::AlreadyAcquired)
    );
    let context = contexts.lock().unwrap().pop().unwrap();
    let alias = context.clone();
    let environment = context.environment().clone();
    drop(context);
    clock.advance(Duration::from_micros(7)).unwrap();
    std::thread::spawn(move || {
        assert_eq!(alias.timestamp().microseconds().unwrap(), 10_000_007);
        assert!(alias.field_trials().is_enabled("Provider-Context"));
        assert_eq!(
            alias.environment().clock_time_bounds(),
            environment.clock_time_bounds()
        );
        drop(alias);
        assert_eq!(
            RandomnessLease::acquire(seed).err(),
            Some(RandomnessLeaseError::AlreadyAcquired)
        );
        drop(environment);
    })
    .join()
    .unwrap();
    // Only the final context/Environment alias, not clock metadata or a hidden
    // global registry, keeps the exclusive lease installed.
    drop(RandomnessLease::acquire(seed).unwrap());
}

#[test]
fn actor_selection_retains_randomness_through_migration_and_final_utility_release() {
    let _exclusive = RANDOMNESS.lock().unwrap();
    let seed = 405;
    let lease = RandomnessLease::acquire(seed).unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let builder = ProductionSession::builder(ProductionSessionConfig::default())
        .randomness(&lease)
        .event_log_factory(Factory(contexts.clone()));
    drop(lease);
    assert_eq!(
        RandomnessLease::acquire(seed).err(),
        Some(RandomnessLeaseError::AlreadyAcquired)
    );
    // Build from owned registration inputs, then migrate the native actor graph.
    let mut actor = std::thread::spawn(move || {
        let mut actor = builder.build().unwrap();
        actor.create_peer(PeerConfiguration::default()).unwrap();
        actor
    })
    .join()
    .unwrap();
    actor.create_peer(PeerConfiguration::default()).unwrap();
    actor.shutdown().unwrap();
    drop(actor);

    let mut captured = std::mem::take(&mut *contexts.lock().unwrap());
    assert_eq!(
        captured.len(),
        2,
        "both peers must invoke the actual selected native factory"
    );
    let mut utility = captured.pop().unwrap().environment().event_log().unwrap();
    drop(captured);
    assert_eq!(
        RandomnessLease::acquire(seed).err(),
        Some(RandomnessLeaseError::AlreadyAcquired)
    );
    utility.stop().unwrap();
    utility.close();
    drop(RandomnessLease::acquire(seed).unwrap());
}
