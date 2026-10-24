#[path = "common/panic_payload.rs"]
mod panic_payload;

use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn explicit_environment_close_releases_its_leases_but_not_retained_aliases() {
    let lease = RandomnessLease::acquire(0x404).unwrap();
    let mut environment = Environment::builder().randomness(&lease).build().unwrap();
    let mut retained = environment.clone();
    drop(lease);
    environment.close();
    assert!(matches!(
        RandomnessLease::acquire(0x405),
        Err(RandomnessLeaseError::AlreadyAcquired)
    ));
    retained.close();
    let replacement = RandomnessLease::acquire(0x405)
        .expect("closed environment handles must release their own Rust-only leases");
    environment.close();
    retained.close();
    drop(replacement);
}

struct PanicClock {
    read: bool,
    dropped: Arc<AtomicUsize>,
}
impl ClockProvider for PanicClock {
    fn current_time(&self) -> FiniteTimestamp {
        if self.read {
            panic_payload::raise();
        }
        FiniteTimestamp::from_microseconds(321).unwrap()
    }
    fn convert_timestamp_to_ntp_time(&self, _: Timestamp) -> NtpTime {
        panic_payload::raise()
    }
}
impl Drop for PanicClock {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
        panic_payload::raise();
    }
}

#[test]
fn native_clock_callbacks_and_last_retained_owner_contain_hostile_payloads() {
    for read in [true, false] {
        let dropped = Arc::new(AtomicUsize::new(0));
        let clock = Clock::from_provider(PanicClock {
            read,
            dropped: dropped.clone(),
        });
        let mut environment = Environment::builder().clock_handle(&clock).build().unwrap();
        if read {
            assert_eq!(clock.time_in_microseconds(), 0);
        } else {
            assert_eq!(clock.time_in_microseconds(), 321);
            assert!(
                !clock
                    .convert_timestamp_to_ntp_time(Timestamp::from_microseconds(321).unwrap())
                    .unwrap()
                    .is_valid()
            );
        }
        assert!(clock.has_failed());
        let mut retained = environment.clone();
        drop(clock);
        environment.close();
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        assert_eq!(
            retained.timestamp().microseconds().unwrap(),
            if read { 0 } else { 321 }
        );
        retained.close();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}

struct PanicTrials {
    dropped: Arc<AtomicUsize>,
    copies: Arc<AtomicUsize>,
    fail_copy: bool,
}
impl FieldTrialsView for PanicTrials {
    fn lookup(&self, _: &str) -> String {
        panic_payload::raise()
    }
    fn create_copy(&self) -> Arc<dyn FieldTrialsView> {
        if self.fail_copy {
            panic_payload::raise();
        }
        self.copies.fetch_add(1, Ordering::SeqCst);
        Arc::new(Self {
            dropped: self.dropped.clone(),
            copies: self.copies.clone(),
            fail_copy: false,
        })
    }
}
impl Drop for PanicTrials {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
        panic_payload::raise();
    }
}

#[test]
fn native_trial_lookup_copy_and_owned_view_teardown_contain_hostile_payloads() {
    for fail_copy in [true, false] {
        let dropped = Arc::new(AtomicUsize::new(0));
        let copies = Arc::new(AtomicUsize::new(0));
        let environment = Environment::builder()
            .field_trials(PanicTrials {
                dropped: dropped.clone(),
                copies: copies.clone(),
                fail_copy,
            })
            .build()
            .unwrap();
        let view = environment.field_trials();
        assert!(view.lookup("Provider-Panic").is_empty());
        drop(environment);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        drop(view);
        assert_eq!(copies.load(Ordering::SeqCst), usize::from(!fail_copy));
        assert_eq!(dropped.load(Ordering::SeqCst), 1 + usize::from(!fail_copy));
    }
}

#[derive(Default)]
struct QueueState {
    tasks: Mutex<Vec<TaskQueueTask>>,
    deleted: AtomicUsize,
    queue_dropped: AtomicUsize,
    factory_dropped: AtomicUsize,
    capture_dropped: AtomicUsize,
}
struct Factory(Arc<QueueState>);
struct Queue(Arc<QueueState>);
impl TaskQueueFactoryProvider for Factory {
    fn create_task_queue(&self, _: &[u8], _: QueuePriority) -> Box<dyn TaskQueueProvider> {
        Box::new(Queue(self.0.clone()))
    }
}
impl Drop for Factory {
    fn drop(&mut self) {
        self.0.factory_dropped.fetch_add(1, Ordering::SeqCst);
        panic_payload::raise();
    }
}
impl TaskQueueProvider for Queue {
    fn queue_name(&self) -> Vec<u8> {
        b"panic-boundary".to_vec()
    }
    fn post_task(&self, task: TaskQueueTask, _: TaskQueueLocation) {
        self.0.tasks.lock().unwrap().push(task);
    }
    fn post_delayed_task(
        &self,
        task: TaskQueueTask,
        _: TimeDelta,
        _: TaskDelayPrecision,
        _: TaskQueueLocation,
    ) {
        self.0.tasks.lock().unwrap().push(task);
    }
    fn delete(&self) {
        self.0.deleted.fetch_add(1, Ordering::SeqCst);
        panic_payload::raise();
    }
}
impl Drop for Queue {
    fn drop(&mut self) {
        self.0.queue_dropped.fetch_add(1, Ordering::SeqCst);
        panic_payload::raise();
    }
}
struct Capture(Arc<QueueState>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.capture_dropped.fetch_add(1, Ordering::SeqCst);
        panic_payload::raise();
    }
}

#[test]
fn native_queue_execution_cancellation_and_all_provider_drops_contain_hostile_payloads() {
    let state = Arc::new(QueueState::default());
    let factory = TaskQueueFactory::from_provider(Factory(state.clone())).unwrap();
    let mut queue = factory
        .create_queue_bytes(b"panic-boundary", QueuePriority::Normal)
        .unwrap();
    assert!(queue.post(|| panic_payload::raise()));
    assert!(state.tasks.lock().unwrap().pop().unwrap().execute());
    let capture = Capture(state.clone());
    assert!(queue.post(move || drop(capture)));
    queue.close();
    assert_eq!(state.deleted.load(Ordering::SeqCst), 1);
    assert_eq!(state.queue_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(state.capture_dropped.load(Ordering::SeqCst), 1);
    assert!(factory.has_failed());
    for ticket in std::mem::take(&mut *state.tasks.lock().unwrap()) {
        assert!(
            !ticket.execute(),
            "closed queue must retire even retained tickets"
        );
    }
    drop(queue);
    drop(factory);
    assert_eq!(state.factory_dropped.load(Ordering::SeqCst), 1);
}
