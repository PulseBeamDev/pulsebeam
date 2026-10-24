use std::sync::{Arc, Mutex, Weak};

use pulsebeam_webrtc_sys::{Environment, FieldTrialsSnapshot, FieldTrialsView};

#[derive(Clone)]
struct NestedLookup {
    snapshot: Arc<Mutex<Weak<FieldTrialsSnapshot>>>,
}

impl FieldTrialsView for NestedLookup {
    fn lookup(&self, key: &str) -> String {
        match key {
            "Leaf" => "EnabledNested".into(),
            "Parent" => {
                let snapshot = self.snapshot.lock().unwrap().upgrade().unwrap();
                snapshot.lookup("Leaf")
            }
            _ => String::new(),
        }
    }

    fn create_copy(&self) -> Arc<dyn FieldTrialsView> {
        Arc::new(self.clone())
    }
}

#[test]
fn immutable_nested_lookup_does_not_hold_the_native_cache_lock() {
    let target = Arc::new(Mutex::new(Weak::new()));
    let environment = Environment::builder()
        .field_trials(NestedLookup {
            snapshot: target.clone(),
        })
        .build()
        .unwrap();
    let snapshot = Arc::new(environment.field_trials());
    *target.lock().unwrap() = Arc::downgrade(&snapshot);
    // The environment and snapshot copies share one native memoization cache.
    let copy = snapshot.as_ref().clone();
    drop(environment);
    assert_eq!(copy.lookup("Parent"), "EnabledNested");
    assert_eq!(snapshot.lookup("Parent"), "EnabledNested");
    assert_eq!(snapshot.lookup("Leaf"), "EnabledNested");
    // Provider copies retain only a weak snapshot reference, not an owner cycle.
    drop(copy);
    drop(snapshot);
    assert!(target.lock().unwrap().upgrade().is_none());
}
