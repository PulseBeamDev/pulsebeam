use std::{
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

#[derive(Default)]
struct State {
    changed: bool,
    waker: Option<Waker>,
}

/// Level-triggered notification of activity, not an event queue. One actor
/// waiter may register. Registration and notification share a lock so a wake
/// cannot be lost between checking for activity and returning Pending.
#[derive(Default)]
pub(crate) struct Readiness(Mutex<State>);

impl Readiness {
    pub(crate) fn notify(&self) {
        let waker = {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            state.changed = true;
            state.waker.take()
        };
        // No binding/native lock is held while invoking the executor's waker.
        if let Some(waker) = waker {
            // A waker can run on a foreign producer thread with no SDK queue
            // TLS. It is still inside a native callback loan, not a control root.
            let _ = crate::provider_panic::catch_provider(|| waker.wake());
        }
    }

    pub(crate) fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.changed {
            state.changed = false;
            state.waker = None;
            Poll::Ready(())
        } else {
            if !state
                .waker
                .as_ref()
                .is_some_and(|w| w.will_wake(cx.waker()))
            {
                state.waker = Some(cx.waker().clone());
            }
            Poll::Pending
        }
    }
}

impl Drop for Readiness {
    fn drop(&mut self) {
        let waker = self
            .0
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .waker
            .take();
        let _ = crate::provider_panic::catch_provider(|| drop(waker));
    }
}

pub(crate) struct RustReadiness(pub(crate) Arc<Readiness>);

pub(crate) fn notify_readiness(readiness: &RustReadiness) {
    // A custom executor waker must not unwind across the C++ callback boundary.
    let _ = crate::provider_panic::catch_provider(|| readiness.0.notify());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    #[derive(Default)]
    struct Counter(AtomicUsize);
    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn notifications_coalesce_and_registration_is_not_lost() {
        let readiness = Readiness::default();
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        readiness.notify();
        readiness.notify();
        assert_eq!(readiness.poll(&mut cx), Poll::Ready(()));
        assert_eq!(readiness.poll(&mut cx), Poll::Pending);
        readiness.notify();
        readiness.notify();
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
        assert_eq!(readiness.poll(&mut cx), Poll::Ready(()));
        assert_eq!(readiness.poll(&mut cx), Poll::Pending);
        readiness.notify();
        assert_eq!(counter.0.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn notification_registration_race_always_observes_activity() {
        let waker = Waker::noop();
        for _ in 0..100 {
            let readiness = Arc::new(Readiness::default());
            let signal = readiness.clone();
            let worker = std::thread::spawn(move || signal.notify());
            let first = readiness.poll(&mut Context::from_waker(waker));
            worker.join().unwrap();
            if first.is_pending() {
                assert!(readiness.poll(&mut Context::from_waker(waker)).is_ready());
            }
        }
    }
}
