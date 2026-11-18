// Observer coordination is a cold control-plane boundary. Locks cover only state/waker
// bookkeeping; neither awaits nor wake callbacks run while a lock is held.
// SAFETY policy exception: this isolated module has no packet-path callers and bounds
// all lock/refcount work to at most 32 observers and their snapshot/waker bookkeeping.
#![allow(clippy::disallowed_types)]

use alloc::sync::Arc;
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use std::sync::{Mutex, MutexGuard};

use crate::ffi::{AgentState, AgentStatus};

pub const MAX_OBSERVERS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObserverLimit;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConcurrentNext;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevisionExhausted;

struct HubState<T> {
    value: T,
    revision: u64,
    source_revision: Option<u64>,
    closed: bool,
    cursors: usize,
    waiters: Vec<(usize, Waker)>,
}

// This hub is only a bounded, cold control-plane snapshot/waker boundary. No packet path
// reaches it; locks are held only while updating state or registering/detaching wakers.
#[allow(clippy::disallowed_types)]
struct Hub<T> {
    inner: Arc<Mutex<HubState<T>>>,
}

struct CursorState {
    seen: Option<u64>,
    outstanding: Option<u64>,
    next_read: u64,
    closed: bool,
}

// Same cold control-plane exception as Hub above; each cursor owns its local read state.
#[allow(clippy::disallowed_types)]
struct Cursor<T: Clone> {
    hub: Arc<Mutex<HubState<T>>>,
    local: Arc<Mutex<CursorState>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl<T: Clone> Hub<T> {
    fn new(initial: T, revision: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HubState {
                value: initial,
                revision,
                source_revision: None,
                closed: false,
                cursors: 0,
                waiters: Vec::new(),
            })),
        }
    }

    fn subscribe(&self) -> Result<Cursor<T>, ObserverLimit> {
        let mut state = lock(&self.inner);
        if state.cursors >= MAX_OBSERVERS {
            return Err(ObserverLimit);
        }
        state.cursors = state.cursors.checked_add(1).ok_or(ObserverLimit)?;
        Ok(Cursor {
            hub: self.inner.clone(),
            local: Arc::new(Mutex::new(CursorState {
                seen: None,
                outstanding: None,
                next_read: 0,
                closed: false,
            })),
        })
    }
}

impl<T: Clone> Cursor<T> {
    fn next(&self) -> Result<Next<T>, ConcurrentNext> {
        let mut local = lock(&self.local);
        if local.outstanding.is_some() {
            return Err(ConcurrentNext);
        }
        let read_id = local.next_read.checked_add(1).ok_or(ConcurrentNext)?;
        local.next_read = read_id;
        local.outstanding = Some(read_id);
        Ok(Next {
            hub: self.hub.clone(),
            local: self.local.clone(),
            read_id,
        })
    }

    fn close(&self) {
        {
            let mut local = lock(&self.local);
            if local.closed {
                return;
            }
            local.closed = true;
        }
        let id = Arc::as_ptr(&self.local) as usize;
        let wake = {
            let mut hub = lock(&self.hub);
            if let Some(cursors) = hub.cursors.checked_sub(1) {
                hub.cursors = cursors;
            }
            hub.waiters
                .iter()
                .position(|(key, _)| *key == id)
                .map(|index| hub.waiters.swap_remove(index).1)
        };
        if let Some(waker) = wake {
            waker.wake();
        }
    }
}

impl<T: Clone> Drop for Cursor<T> {
    fn drop(&mut self) {
        self.close();
    }
}

struct Next<T> {
    hub: Arc<Mutex<HubState<T>>>,
    local: Arc<Mutex<CursorState>>,
    read_id: u64,
}

impl<T> Drop for Next<T> {
    fn drop(&mut self) {
        let id = Arc::as_ptr(&self.local) as usize;
        let mut local = lock(&self.local);
        if local.outstanding == Some(self.read_id) {
            local.outstanding = None;
            lock(&self.hub).waiters.retain(|(key, _)| *key != id);
        }
    }
}

impl<T: Clone> Future for Next<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut local = lock(&this.local);
        if local.closed {
            if local.outstanding == Some(this.read_id) {
                local.outstanding = None;
            }
            return Poll::Ready(None);
        }

        let mut hub = lock(&this.hub);
        if local.seen != Some(hub.revision) {
            local.seen = Some(hub.revision);
            if local.outstanding == Some(this.read_id) {
                local.outstanding = None;
            }
            return Poll::Ready(Some(hub.value.clone()));
        }
        if hub.closed {
            if local.outstanding == Some(this.read_id) {
                local.outstanding = None;
            }
            return Poll::Ready(None);
        }

        let id = Arc::as_ptr(&this.local) as usize;
        if let Some((_, waker)) = hub.waiters.iter_mut().find(|(key, _)| *key == id) {
            waker.clone_from(cx.waker());
        } else {
            debug_assert!(hub.waiters.len() < MAX_OBSERVERS);
            hub.waiters.push((id, cx.waker().clone()));
        }
        Poll::Pending
    }
}

/// Independent latest-value observation cursor for the concrete Agent state.
pub struct AgentUpdates {
    cursor: Cursor<AgentState>,
}

impl AgentUpdates {
    pub fn next(&self) -> Result<impl Future<Output = Option<AgentState>>, ConcurrentNext> {
        self.cursor.next()
    }

    pub fn close(&self) {
        self.cursor.close();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishResult {
    Published,
    Unchanged,
    Closed,
}

/// Core-owned bounded latest-state publisher. Main projects actor state into it.
pub struct AgentStateObserver {
    hub: Hub<AgentState>,
}

impl AgentStateObserver {
    pub fn new(mut initial: AgentState) -> Self {
        let revision = initial.observation_revision.max(1);
        initial.observation_revision = revision;
        Self {
            hub: Hub::new(initial, revision),
        }
    }

    /// Return the currently published state from the same hub used by cursors.
    pub fn state(&self) -> AgentState {
        lock(&self.hub.inner).value.clone()
    }

    pub fn updates(&self) -> Result<AgentUpdates, ObserverLimit> {
        Ok(AgentUpdates {
            cursor: self.hub.subscribe()?,
        })
    }

    /// Equal state is a no-op and does not advance the revision or wake readers.
    pub fn publish(&self, state: AgentState) -> Result<PublishResult, RevisionExhausted> {
        self.set(state, false, None)
    }

    /// Ignore a projection superseded before its caller released the actor lock.
    pub fn publish_from(
        &self,
        source_revision: u64,
        state: AgentState,
    ) -> Result<PublishResult, RevisionExhausted> {
        self.set(state, false, Some(source_revision))
    }

    /// Publish one final `Closed` snapshot. Repeated closes are idempotent.
    pub fn close(&self, mut final_state: AgentState) -> Result<PublishResult, RevisionExhausted> {
        final_state.status = AgentStatus::Closed;
        self.set(final_state, true, None)
    }

    fn set(
        &self,
        mut value: AgentState,
        closing: bool,
        source_revision: Option<u64>,
    ) -> Result<PublishResult, RevisionExhausted> {
        let wakes = {
            let mut hub = lock(&self.hub.inner);
            if hub.closed {
                return Ok(PublishResult::Closed);
            }
            if let Some(revision) = source_revision {
                if hub
                    .source_revision
                    .is_some_and(|current| current >= revision)
                {
                    return Ok(PublishResult::Unchanged);
                }
            }
            value.observation_revision = hub.revision;
            if !closing && value == hub.value {
                if source_revision.is_some() {
                    hub.source_revision = source_revision;
                }
                return Ok(PublishResult::Unchanged);
            }
            let revision = hub.revision.checked_add(1).ok_or(RevisionExhausted)?;
            if source_revision.is_some() {
                hub.source_revision = source_revision;
            }
            value.observation_revision = revision;
            hub.revision = revision;
            hub.value = value;
            hub.closed = closing;
            core::mem::take(&mut hub.waiters)
                .into_iter()
                .map(|(_, waker)| waker)
                .collect::<Vec<_>>()
        };
        for waker in wakes {
            waker.wake();
        }
        Ok(PublishResult::Published)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::AgentStatus;
    use alloc::string::String;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::Wake;

    struct LockProbe {
        hub: Arc<Mutex<HubState<AgentState>>>,
        unlocked_on_wake: AtomicBool,
    }

    impl Wake for LockProbe {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.unlocked_on_wake
                .store(self.hub.try_lock().is_ok(), Ordering::SeqCst);
        }
    }

    struct Counter(AtomicUsize);
    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn poll_next(cursor: &AgentUpdates, counter: &Arc<Counter>) -> Poll<Option<AgentState>> {
        let mut next = Box::pin(cursor.next().unwrap());
        let waker = Waker::from(Arc::clone(counter));
        next.as_mut().poll(&mut Context::from_waker(&waker))
    }

    fn state(revision: u64) -> AgentState {
        AgentState {
            observation_revision: revision,
            desired_join_revision: 0,
            evaluated_join_revision: 0,
            desired_credential_revision: 0,
            evaluated_credential_revision: 0,
            room: None,
            desired_room: None,
            status: AgentStatus::Idle,
            error: None,
        }
    }

    #[test]
    fn independent_cursors_coalesce_and_state_shares_hub() {
        let observer = AgentStateObserver::new(state(7));
        let a = observer.updates().unwrap();
        let b = observer.updates().unwrap();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        assert_eq!(poll_next(&a, &counter), Poll::Ready(Some(state(7))));
        assert_eq!(poll_next(&b, &counter), Poll::Ready(Some(state(7))));
        assert_eq!(observer.publish(state(7)), Ok(PublishResult::Unchanged));
        let mut changed = state(7);
        changed.desired_room = Some(String::from("room"));
        assert_eq!(
            observer.publish(changed.clone()),
            Ok(PublishResult::Published)
        );
        assert_eq!(observer.state(), changed_with_revision(changed, 8));
        assert_eq!(poll_next(&a, &counter), Poll::Ready(Some(observer.state())));
        assert_eq!(poll_next(&b, &counter), Poll::Ready(Some(observer.state())));
        let newest = observer.state();
        assert_eq!(
            observer.publish_from(20, newest.clone()),
            Ok(PublishResult::Unchanged)
        );
        assert_eq!(
            observer.publish_from(19, state(0)),
            Ok(PublishResult::Unchanged)
        );
        assert_eq!(observer.state(), newest);
    }

    fn changed_with_revision(mut state: AgentState, revision: u64) -> AgentState {
        state.observation_revision = revision;
        state
    }

    #[test]
    fn completed_read_drop_does_not_cancel_a_new_read() {
        let observer = AgentStateObserver::new(state(0));
        let cursor = observer.updates().unwrap();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let mut first = Box::pin(cursor.next().unwrap());
        let waker = Waker::from(Arc::clone(&counter));
        let mut cx = Context::from_waker(&waker);
        assert_eq!(first.as_mut().poll(&mut cx), Poll::Ready(Some(state(1))));
        let mut second = Box::pin(cursor.next().unwrap());
        assert_eq!(second.as_mut().poll(&mut cx), Poll::Pending);
        drop(first);
        assert_eq!(cursor.next().err(), Some(ConcurrentNext));
        let mut changed = state(0);
        changed.status = AgentStatus::Joined;
        observer.publish(changed.clone()).unwrap();
        changed.observation_revision = 2;
        assert_eq!(second.as_mut().poll(&mut cx), Poll::Ready(Some(changed)));
    }

    #[test]
    fn canceling_pending_read_releases_its_reservation() {
        let observer = AgentStateObserver::new(state(0));
        let cursor = observer.updates().unwrap();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        assert_eq!(poll_next(&cursor, &counter), Poll::Ready(Some(state(1))));

        let mut canceled = Box::pin(cursor.next().unwrap());
        let waker = Waker::from(Arc::clone(&counter));
        let mut cx = Context::from_waker(&waker);
        assert_eq!(canceled.as_mut().poll(&mut cx), Poll::Pending);
        drop(canceled);

        let mut next = Box::pin(cursor.next().unwrap());
        assert_eq!(next.as_mut().poll(&mut cx), Poll::Pending);
        let mut changed = state(0);
        changed.status = AgentStatus::Joined;
        observer.publish(changed.clone()).unwrap();
        changed.observation_revision = 2;
        assert_eq!(next.as_mut().poll(&mut cx), Poll::Ready(Some(changed)));
    }

    #[test]
    fn close_is_immutable_final_then_end_and_wakes_outside_locks() {
        let observer = AgentStateObserver::new(state(0));
        let cursor = observer.updates().unwrap();
        let probe = Arc::new(LockProbe {
            hub: observer.hub.inner.clone(),
            unlocked_on_wake: AtomicBool::new(false),
        });
        let mut initial = Box::pin(cursor.next().unwrap());
        let waker = Waker::from(Arc::clone(&probe));
        let mut cx = Context::from_waker(&waker);
        assert_eq!(initial.as_mut().poll(&mut cx), Poll::Ready(Some(state(1))));
        let mut pending = Box::pin(cursor.next().unwrap());
        assert_eq!(pending.as_mut().poll(&mut cx), Poll::Pending);
        let mut final_state = state(0);
        final_state.desired_room = Some(String::from("final"));
        assert_eq!(
            observer.close(final_state.clone()),
            Ok(PublishResult::Published)
        );
        assert!(probe.unlocked_on_wake.load(Ordering::SeqCst));
        assert_eq!(
            pending.as_mut().poll(&mut cx),
            Poll::Ready(Some(changed_with_revision(
                AgentState {
                    status: AgentStatus::Closed,
                    ..final_state
                },
                2,
            )))
        );
        assert_eq!(observer.publish(state(2)), Ok(PublishResult::Closed));
        assert_eq!(observer.close(state(9)), Ok(PublishResult::Closed));
        assert_eq!(observer.state().observation_revision, 2);
        let mut end = Box::pin(cursor.next().unwrap());
        assert_eq!(end.as_mut().poll(&mut cx), Poll::Ready(None));
    }

    #[test]
    fn observer_budget_is_reclaimed_and_initial_revision_is_monotonic() {
        let observer = AgentStateObserver::new(state(41));
        let cursors = (0..MAX_OBSERVERS)
            .map(|_| observer.updates().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(observer.updates().err(), Some(ObserverLimit));
        drop(cursors);
        assert!(observer.updates().is_ok());
        let mut changed = state(0);
        changed.status = AgentStatus::Joined;
        assert_eq!(observer.publish(changed), Ok(PublishResult::Published));
        assert_eq!(observer.state().observation_revision, 42);
    }

    #[test]
    fn revision_exhaustion_does_not_reuse_or_close_resource() {
        let observer = AgentStateObserver::new(state(u64::MAX));
        let mut changed = state(u64::MAX);
        changed.status = AgentStatus::Joined;
        assert_eq!(observer.publish(changed.clone()), Err(RevisionExhausted));
        assert_eq!(observer.state().observation_revision, u64::MAX);
        assert_eq!(observer.close(changed.clone()), Err(RevisionExhausted));
        assert_eq!(observer.publish(changed), Err(RevisionExhausted));
    }
}
