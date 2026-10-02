#![allow(clippy::expect_used, reason = "controlled test world invariants")]

pub use pulsebeam_webrtc_sys::*;

use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

static LEASE: Mutex<()> = Mutex::new(());
thread_local! {
    static WORLD: RefCell<Weak<SimulationWorld>> = const { RefCell::new(Weak::new()) };
}

/// A shared caller-thread world for every peer participating in one scenario.
/// The native driver is process-exclusive, so parallel Rust tests serialize
/// world ownership, not protocol time or packet delivery.
pub struct SimulationWorld {
    pub controlled: ControlledWorld,
    clock_origin: RefCell<Option<(Instant, Duration)>>,
    _lease: MutexGuard<'static, ()>,
}

impl SimulationWorld {
    pub fn acquire() -> Rc<Self> {
        WORLD.with(|slot| {
            if let Some(world) = slot.borrow().upgrade() {
                return world;
            }
            // The mutex protects exclusivity, not state. Native RAII releases the world even on test panic.
            let lease = LEASE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let world = Rc::new(Self {
                controlled: ControlledWorld::acquire(11, Duration::from_secs(10))
                    .expect("pinned controlled native engine"),
                clock_origin: RefCell::new(None),
                _lease: lease,
            });
            *slot.borrow_mut() = Rc::downgrade(&world);
            world
        })
    }

    pub fn bind_clock(&self, origin: Instant) {
        self.clock_origin.borrow_mut().get_or_insert((origin, self.controlled.now()));
    }

    pub fn native_time_at(&self, at: Instant) -> Duration {
        let (origin, native_origin) = self.clock_origin.borrow().expect("bound scenario clock");
        native_origin + at.duration_since(origin)
    }

    pub fn monotonic_now(&self) -> Instant {
        let (origin, native_origin) = self.clock_origin.borrow().expect("bound scenario clock");
        origin + self.controlled.now().saturating_sub(native_origin)
    }

    pub fn advance_clock_to(&self, at: Instant) {
        self.advance_to(self.native_time_at(at));
    }

    pub fn pump(&self) {
        self.controlled.pump(512);
    }

    pub fn advance_to(&self, at: Duration) {
        if at > self.controlled.now() {
            self.controlled.advance(at - self.controlled.now())
                .expect("forward simulated time");
        }
        self.pump();
    }
}

pub fn complete_operation(
    world: &SimulationWorld,
    peer: &PeerConnection,
    operation: OperationId,
) -> Option<SessionDescription> {
    for _ in 0..2_000 {
        world.pump();
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::OperationComplete(completion) = event {
                assert_eq!(completion.operation_id, operation);
                return completion.result.expect("native signaling operation");
            }
        }
        world.controlled.advance(Duration::from_millis(1))
            .expect("signaling simulation step");
    }
    panic!("native signaling operation stalled: {operation:?}");
}
