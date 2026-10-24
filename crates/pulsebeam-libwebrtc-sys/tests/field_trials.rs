use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use pulsebeam_webrtc_sys::{
    Environment, FieldTrials, FieldTrialsError, FieldTrialsView, KeyExchangeGroup,
    KeyExchangeGroups,
};

#[test]
fn native_parser_and_owned_edits_preserve_open_values() {
    assert_eq!(FieldTrials::parse("").unwrap(), FieldTrials::default());
    for invalid in [
        "A/Enabled",
        "/Enabled/",
        "A//",
        "A/Enabled/A/Disabled/",
        "A/Enabled//",
    ] {
        assert_eq!(
            FieldTrials::parse(invalid),
            Err(FieldTrialsError::InvalidConfiguration),
            "{invalid}"
        );
    }
    let mut trials = FieldTrials::parse(
        "Unregistered-Open-Token/Enabled100kbps/Unregistered-Open-Token/Enabled100kbps/",
    )
    .unwrap();
    assert!(trials.is_enabled("Unregistered-Open-Token"));
    assert_eq!(trials.lookup("missing"), "");
    trials
        .set("Unregistered-Open-Token", "DisabledButConfigured")
        .unwrap();
    assert!(trials.is_disabled("Unregistered-Open-Token"));
    let prior = trials.clone();
    assert_eq!(
        trials.set("", "Enabled"),
        Err(FieldTrialsError::InvalidEntry)
    );
    assert_eq!(
        trials.set("A/B", "Enabled"),
        Err(FieldTrialsError::InvalidEntry)
    );
    assert_eq!(
        trials.set("A", "Enabled/"),
        Err(FieldTrialsError::InvalidEntry)
    );
    assert_eq!(trials, prior);
    trials
        .merge(&FieldTrials::parse("Unregistered-Open-Token/Replacement/New/OpenValue/").unwrap());
    assert_eq!(trials.lookup("Unregistered-Open-Token"), "Replacement");
    trials.set("Unregistered-Open-Token", "").unwrap();
    assert_eq!(trials.lookup("Unregistered-Open-Token"), "");
    assert_eq!(trials.lookup("New"), "OpenValue");
}

#[test]
fn native_view_outlives_environment_and_is_concurrently_readable() {
    let trials =
        FieldTrials::parse("Open/EnabledWithParameters/Off/DisabledWithParameters/").unwrap();
    let environment = Environment::builder().field_trials(trials).build().unwrap();
    let snapshot = environment.field_trials();
    let copy = snapshot.clone();
    drop(environment);
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let copy = copy.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    assert_eq!(copy.lookup("Open"), "EnabledWithParameters");
                    assert!(copy.is_enabled("Open"));
                    assert!(copy.is_disabled("Off"));
                    assert_eq!(copy.lookup("missing"), "");
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(snapshot.is_enabled("Open"));
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<FieldTrials>();
    assert_send_sync::<pulsebeam_webrtc_sys::FieldTrialsSnapshot>();
}

struct Provider {
    live: Arc<AtomicUsize>,
    copies: Arc<AtomicUsize>,
    lookups: Arc<AtomicUsize>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}
impl FieldTrialsView for Provider {
    fn lookup(&self, key: &str) -> String {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        if key == "Open" {
            "EnabledCustom".into()
        } else {
            String::new()
        }
    }
    fn create_copy(&self) -> Arc<dyn FieldTrialsView> {
        self.copies.fetch_add(1, Ordering::SeqCst);
        self.live.fetch_add(1, Ordering::SeqCst);
        Arc::new(Self {
            live: self.live.clone(),
            copies: self.copies.clone(),
            lookups: self.lookups.clone(),
        })
    }
}

#[test]
fn custom_provider_is_retained_copied_and_retired_after_native_owners() {
    let live = Arc::new(AtomicUsize::new(1));
    let copies = Arc::new(AtomicUsize::new(0));
    let lookups = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(Provider {
        live: live.clone(),
        copies: copies.clone(),
        lookups: lookups.clone(),
    });
    let environment = Environment::builder()
        .field_trials(provider.clone())
        .build()
        .unwrap();
    drop(provider);
    let environment_copy = environment.clone();
    let view = environment.field_trials();
    assert_eq!(copies.load(Ordering::SeqCst), 1);
    assert_eq!(view.lookup("Open"), "EnabledCustom");
    let copy = view.clone();
    assert_eq!(copies.load(Ordering::SeqCst), 2);
    drop(environment);
    assert_eq!(live.load(Ordering::SeqCst), 3);
    drop(environment_copy);
    assert_eq!(live.load(Ordering::SeqCst), 2);
    drop(view);
    assert_eq!(live.load(Ordering::SeqCst), 1);
    let copied_lookups = lookups.clone();
    std::thread::spawn(move || {
        assert!(copy.is_enabled("Open"));
        assert_eq!(
            copied_lookups.load(Ordering::SeqCst),
            1,
            "native immutable copies share previously observed groups"
        );
        assert!(copy.lookup("Uncached").is_empty());
        assert!(copy.lookup("Uncached").is_empty());
    })
    .join()
    .unwrap();
    assert_eq!(live.load(Ordering::SeqCst), 0);
    // Both the previously observed group and the newly observed empty group
    // are cached: exactly one provider lookup for each distinct key.
    assert_eq!(lookups.load(Ordering::SeqCst), 2);
}

#[test]
fn actor_field_trials_survive_migration_and_snapshot_retirement() {
    use pulsebeam_webrtc_sys::{PeerErrorKind, ProductionSession, ProductionSessionConfig};
    let session = ProductionSession::new_with_field_trials(
        ProductionSessionConfig::default(),
        FieldTrials::parse("Actor-Open/EnabledOwned/").unwrap(),
    )
    .unwrap();
    let mut session = std::thread::spawn(move || {
        assert!(session.field_trials().unwrap().is_enabled("Actor-Open"));
        session
    })
    .join()
    .unwrap();
    let snapshot = session.field_trials().unwrap();
    session.shutdown().unwrap();
    assert_eq!(
        session.field_trials().err().unwrap().kind,
        PeerErrorKind::Closed
    );
    drop(session);
    std::thread::spawn(move || assert_eq!(snapshot.lookup("Actor-Open"), "EnabledOwned"))
        .join()
        .unwrap();
}

struct PanickingProvider;
impl FieldTrialsView for PanickingProvider {
    fn lookup(&self, _: &str) -> String {
        panic!("provider lookup panic")
    }
    fn create_copy(&self) -> Arc<dyn FieldTrialsView> {
        panic!("provider copy panic")
    }
}

#[test]
fn provider_panics_do_not_unwind_through_native_frames() {
    let environment = Environment::builder()
        .field_trials(PanickingProvider)
        .build()
        .unwrap();
    let mut groups = KeyExchangeGroups::new(Vec::new());
    groups.update(&environment, &[]);
    assert!(groups.enabled().contains(&KeyExchangeGroup::X25519));
    let view = environment.field_trials();
    assert_eq!(view.lookup("any"), "");
    assert!(!view.is_enabled("any"));
}
