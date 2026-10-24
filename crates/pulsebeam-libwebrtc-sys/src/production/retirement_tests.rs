//! Actual graph-role dispatch and reservation error/ordinary Drop regressions.
use super::*;
use crate::{ControlledWorld, FieldTrialsView, RtcConfiguration};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::ThreadId,
    time::{Duration, Instant},
};

struct Marker {
    released: Arc<AtomicBool>,
    thread: Arc<Mutex<Option<ThreadId>>>,
}
impl Drop for Marker {
    fn drop(&mut self) {
        *self.thread.lock().unwrap() = Some(std::thread::current().id());
        self.released.store(true, Ordering::SeqCst);
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
fn new_marker() -> (Arc<Marker>, Arc<AtomicBool>, Arc<Mutex<Option<ThreadId>>>) {
    let released = Arc::new(AtomicBool::new(false));
    let thread = Arc::new(Mutex::new(None));
    (
        Arc::new(Marker {
            released: released.clone(),
            thread: thread.clone(),
        }),
        released,
        thread,
    )
}
fn await_release(released: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !released.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "retired graph retained its creation environment"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    loop {
        if let Ok(world) = ControlledWorld::acquire(947, Duration::from_secs(3)) {
            drop(world);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "graph role/production lease outlived retirement"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn actual_owned_network_worker_and_signaling_tasks_cannot_self_join_or_release_lease_early() {
    // Run this and other process clock/runtime tests with --test-threads=1.
    for role in 0..3 {
        for with_peer in [false, true] {
            let (marker, released, _) = new_marker();
            let weak = Arc::downgrade(&marker);
            let actor = ProductionSession::builder(Default::default())
                .field_trials(Trials(marker))
                .build()
                .unwrap();
            let mut actor = std::thread::spawn(move || {
                let mut actor = actor;
                actor.create_raw_video_source().unwrap();
                actor
            })
            .join()
            .unwrap();
            if with_peer {
                let peer = actor.create_peer(Default::default()).unwrap();
                // Leave an actual SDP operation pending for shutdown fencing.
                actor.create_offer(peer, false).unwrap();
            }
            let owner = Arc::new(Mutex::new(Some(actor)));
            let callback_owner = owner.clone();
            let (dropped_tx, dropped_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (returned_tx, returned_rx) = mpsc::channel();
            let caller = std::thread::current().id();
            {
                let guard = owner.lock().unwrap();
                assert!(guard.as_ref().unwrap().factory.post_owned_role_for_test(
                    role,
                    move || {
                        assert_ne!(std::thread::current().id(), caller);
                        let actor = callback_owner.lock().unwrap().take().unwrap();
                        drop(actor);
                        dropped_tx.send(()).unwrap();
                        // Keep this real graph-owned native task alive after
                        // handoff. Its role/production lease must not be released
                        // until the caller permits this callback to return.
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        returned_tx.send(()).unwrap();
                    }
                ));
            }
            dropped_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("final actor Drop blocked on its own SDK role");
            assert!(owner.lock().unwrap().is_none());
            assert!(
                ControlledWorld::acquire(948, Duration::from_secs(3)).is_err(),
                "production lease released while its actual owned-role task was still running"
            );
            release_tx.send(()).unwrap();
            returned_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            await_release(&released);
            assert!(weak.upgrade().is_none());
        }
    }
}

#[test]
fn reservation_failure_releases_constructed_factory_and_ordinary_drop_joins_unused_worker() {
    let (marker, released, _) = new_marker();
    let weak = Arc::downgrade(&marker);
    FAIL_NEXT_RESERVATION.with(|flag| flag.set(true));
    let error = ProductionSession::builder(Default::default())
        .field_trials(Trials(marker))
        .build()
        .err()
        .expect("injected reservation failure must reject the actor");
    assert_eq!(error.kind, PeerErrorKind::ResourceExhausted);
    assert!(error.message.contains("retirement worker admission"));
    assert!(
        released.load(Ordering::SeqCst) && weak.upgrade().is_none(),
        "construction error leaked the already-created native factory/environment"
    );
    await_release(&released);
    for closed in [false, true] {
        let (marker, released, _) = new_marker();
        let weak = Arc::downgrade(&marker);
        let mut actor = ProductionSession::builder(Default::default())
            .field_trials(Trials(marker))
            .build()
            .unwrap();
        let retirement = actor.retirement.as_ref().unwrap();
        let worker = Arc::downgrade(&retirement.state);
        let handoff_used = retirement.state.handoff_used.clone();
        // An ordinary binding error must not consume or detach the reserved
        // one-shot worker. It remains idle until normal owner destruction.
        let mut invalid = RtcConfiguration::default();
        invalid.ice_candidate_pool_size = -1;
        assert!(actor.create_peer_with_configuration(invalid).is_err());
        if closed {
            actor.shutdown().unwrap();
        }
        drop(actor);
        assert!(released.load(Ordering::SeqCst) && weak.upgrade().is_none());
        // The last SDK Environment copy can release the thread-safe trial
        // provider on an SDK role during synchronous teardown. Its destructor
        // thread is not evidence of using the actor retirement worker.
        assert!(
            !handoff_used.load(Ordering::SeqCst),
            "ordinary caller destruction must not use the retirement handoff"
        );
        assert!(
            worker.upgrade().is_none(),
            "unused reservation worker/state did not exit and join before ordinary Drop returned"
        );
        await_release(&released);
    }
}

#[test]
fn caller_owned_native_queue_survives_migrated_actor_final_drop() {
    let factory = crate::TaskQueueFactory::default_threaded().unwrap();
    let mut queue = factory
        .create_queue("caller-owned-retirement", crate::QueuePriority::Normal)
        .unwrap();
    let (marker, released, _) = new_marker();
    let weak = Arc::downgrade(&marker);
    let mut actor = ProductionSession::builder(Default::default())
        .task_queue_factory(&factory)
        .field_trials(Trials(marker))
        .build()
        .unwrap();
    actor.create_raw_video_source().unwrap();
    let actor = std::thread::spawn(move || actor).join().unwrap();
    let (dropped_tx, dropped_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    assert!(queue.post(move || {
        assert_eq!(
            crate::TaskQueueContext::current().unwrap().name_bytes(),
            b"caller-owned-retirement"
        );
        drop(actor);
        dropped_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    }));
    dropped_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("actor final Drop blocked on a disjoint caller-owned queue");
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !released.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "caller queue retained the actor environment after its final Drop"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(weak.upgrade().is_none());
    let (alive_tx, alive_rx) = mpsc::channel();
    assert!(queue.post(move || alive_tx.send(()).unwrap()));
    alive_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("actor teardown closed the caller-owned native queue");
    queue.close();
    drop((queue, factory));
    await_release(&released);
}

struct NestedOwner {
    child: Mutex<Option<ProductionSession>>,
    callback_finished: Arc<AtomicBool>,
}
impl Drop for NestedOwner {
    fn drop(&mut self) {
        // This is the selected native field-trial provider's actual final
        // destruction, not a synthetic callback-scope probe. It must safely
        // drop a different actor even when inside the outer teardown loan.
        drop(self.child.get_mut().unwrap().take());
        self.callback_finished.store(true, Ordering::SeqCst);
    }
}
#[derive(Clone)]
struct NestedTrials(Arc<NestedOwner>);
impl FieldTrialsView for NestedTrials {
    fn lookup(&self, _: &str) -> String {
        let _ = &self.0;
        String::new()
    }
    fn create_copy(&self) -> Arc<dyn FieldTrialsView> {
        Arc::new(self.clone())
    }
}

#[test]
fn selected_native_provider_final_drop_can_retire_a_nested_migrated_actor() {
    let (marker, released, _) = new_marker();
    let weak = Arc::downgrade(&marker);
    let child = ProductionSession::builder(Default::default())
        .field_trials(Trials(marker))
        .build()
        .unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let nested = Arc::new(NestedOwner {
        child: Mutex::new(Some(child)),
        callback_finished: finished.clone(),
    });
    let nested_weak = Arc::downgrade(&nested);
    let parent = ProductionSession::builder(Default::default())
        .field_trials(NestedTrials(nested))
        .build()
        .unwrap();
    let unused_worker = Arc::downgrade(&parent.retirement.as_ref().unwrap().state);
    let parent = std::thread::spawn(move || parent).join().unwrap();
    drop(parent);
    assert!(finished.load(Ordering::SeqCst) && nested_weak.upgrade().is_none());
    assert!(unused_worker.upgrade().is_none());
    await_release(&released);
    assert!(weak.upgrade().is_none());
}
