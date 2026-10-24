use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Trials {
    live: Arc<AtomicUsize>,
}
impl Drop for Trials {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}
impl FieldTrialsView for Trials {
    fn lookup(&self, key: &str) -> String {
        if key == "Peer-Owned" {
            "EnabledOverride".into()
        } else {
            String::new()
        }
    }
    fn create_copy(&self) -> Arc<dyn FieldTrialsView> {
        self.live.fetch_add(1, Ordering::SeqCst);
        Arc::new(Self {
            live: self.live.clone(),
        })
    }
}
struct Observe(Arc<Mutex<Vec<FieldTrialsSnapshot>>>);
struct Noop;
impl RtcEventLogger for Noop {
    fn start(&self, _output: EventLogOutput, _period: EventLogOutputPeriod) -> bool {
        false
    }
    fn stop(&self) {}
    fn log(&self, _event: RtcEvent) {}
}
impl RtcEventLogFactory for Observe {
    fn create(&self, environment: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        self.0.lock().unwrap().push(environment.field_trials());
        Box::new(Noop)
    }
}
#[test]
fn peer_field_trials_override_native_environment_without_changing_factory_defaults() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<PeerConnectionDependencies>();
    let views = Arc::new(Mutex::new(Vec::new()));
    let live = Arc::new(AtomicUsize::new(1));
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .field_trials(FieldTrials::parse("Factory-Owned/EnabledDefault/").unwrap())
        .event_log_factory(Observe(views.clone()))
        .build()
        .unwrap();
    session.create_peer(PeerConfiguration::default()).unwrap();
    let dependencies =
        PeerConnectionDependencies::default().field_trials(Trials { live: live.clone() });
    let mut session = std::thread::spawn(move || {
        session
            .create_peer_with_dependencies(RtcConfiguration::default(), dependencies)
            .unwrap();
        session.create_peer(PeerConfiguration::default()).unwrap();
        assert_eq!(
            session.field_trials().unwrap().lookup("Factory-Owned"),
            "EnabledDefault"
        );
        session.shutdown().unwrap();
        session
    })
    .join()
    .unwrap();
    session.shutdown().unwrap();
    drop(session);
    let mut snapshots = std::mem::take(&mut *views.lock().unwrap());
    assert_eq!(snapshots.len(), 3);
    for index in [0, 2] {
        assert_eq!(snapshots[index].lookup("Factory-Owned"), "EnabledDefault");
        assert_eq!(snapshots[index].lookup("Peer-Owned"), "");
    }
    let override_view = snapshots.remove(1);
    assert_eq!(override_view.lookup("Peer-Owned"), "EnabledOverride");
    assert_eq!(override_view.lookup("Factory-Owned"), "");
    drop(snapshots);
    assert_eq!(
        live.load(Ordering::SeqCst),
        1,
        "only the independently copied view should remain"
    );
    std::thread::spawn(move || assert!(override_view.is_enabled("Peer-Owned")))
        .join()
        .unwrap();
    assert_eq!(live.load(Ordering::SeqCst), 0);
}
