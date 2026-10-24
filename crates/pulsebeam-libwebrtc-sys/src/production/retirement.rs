//! Single-use production final-owner handoff. Never used by controlled drivers.
use super::{PeerError, PeerErrorKind, ProductionSession, error};
use std::sync::{Arc, Condvar, Mutex};

#[cfg(test)]
thread_local! {
    // One caller-local injected admission failure, never a production option
    // or a process-global switch that could affect unrelated tests/builders.
    static FAIL_NEXT_RESERVATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(test)]
#[path = "retirement_tests.rs"]
mod tests;

// This is a single ownership handoff, not a reusable executor, registry or
// application dispatch loop. All allocations/thread admission happen at build;
// handoff contains no channel failure or thread-spawn path inside an SDK loan.
#[allow(
    clippy::large_enum_variant,
    reason = "The pre-admitted retirement handoff must not allocate during an SDK loan."
)]
enum RetirementRequest {
    Waiting,
    Cancelled,
    Retire(ProductionSession),
}
struct RetirementState {
    request: Mutex<RetirementRequest>,
    ready: Condvar,
    #[cfg(test)]
    handoff_used: Arc<std::sync::atomic::AtomicBool>,
}
pub(super) struct SessionRetirement {
    state: Arc<RetirementState>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl SessionRetirement {
    pub(super) fn new() -> Result<Self, PeerError> {
        #[cfg(test)]
        if FAIL_NEXT_RESERVATION.with(|flag| flag.replace(false)) {
            return Err(error(
                PeerErrorKind::ResourceExhausted,
                "injected production retirement worker admission failure",
            ));
        }
        let state = Arc::new(RetirementState {
            request: Mutex::new(RetirementRequest::Waiting),
            ready: Condvar::new(),
            #[cfg(test)]
            handoff_used: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        let pending = state.clone();
        let worker = std::thread::Builder::new()
            .name("webrtc-retire".into())
            .spawn(move || {
                let request = {
                    let mut slot = pending
                        .request
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    while matches!(*slot, RetirementRequest::Waiting) {
                        slot = pending
                            .ready
                            .wait(slot)
                            .unwrap_or_else(|error| error.into_inner());
                    }
                    std::mem::replace(&mut *slot, RetirementRequest::Cancelled)
                };
                if let RetirementRequest::Retire(owner) = request {
                    // Peer Close/destruction and SDK queue deletion supply the
                    // native quiescence waits, now off every graph-owned role.
                    let _ = crate::provider_panic::catch_task(|| drop(owner));
                }
            })
            .map_err(|value| {
                error(
                    PeerErrorKind::ResourceExhausted,
                    &format!("cannot reserve production retirement worker: {value}"),
                )
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }
    pub(super) fn retire(mut self, owner: ProductionSession) {
        #[cfg(test)]
        self.state
            .handoff_used
            .store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let mut slot = self
                .state
                .request
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            // This private owner is consumed once; only its worker can read it.
            *slot = RetirementRequest::Retire(owner);
        }
        self.state.ready.notify_one();
        // Never join from the SDK callback that teardown may be waiting on.
        // The worker owns its state and exits after consuming this one graph.
        drop(self.worker.take());
    }
}
impl Drop for SessionRetirement {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            {
                let mut slot = self
                    .state
                    .request
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                *slot = RetirementRequest::Cancelled;
            }
            self.state.ready.notify_one();
            // Unused worker: no engine resources or callbacks run on it.
            if let Err(payload) = worker.join() {
                std::mem::forget(payload);
            }
        }
    }
}
