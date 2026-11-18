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

trait RevisionedState: Clone + PartialEq {
    fn set_revision(&mut self, revision: u64);
}

impl RevisionedState for AgentState {
    fn set_revision(&mut self, revision: u64) {
        self.observation_revision = revision;
    }
}

impl RevisionedState for crate::ffi::PublicationState {
    fn set_revision(&mut self, revision: u64) {
        self.observation_revision = revision;
    }
}

impl RevisionedState for crate::ffi::PipeState {
    fn set_revision(&mut self, revision: u64) {
        self.observation_revision = revision;
    }
}

macro_rules! revisioned_resource_state {
    ($($state:ty),+ $(,)?) => {$ (
        impl RevisionedState for $state {
            fn set_revision(&mut self, revision: u64) {
                self.observation_revision = revision;
            }
        }
    )+ };
}

revisioned_resource_state!(
    crate::ffi::VideoSubscriptionState,
    crate::ffi::AudioSubscriptionState,
    crate::ffi::DataSubscriptionState,
    crate::ffi::DataPublicationState,
    crate::ffi::WatchState,
);

trait ResourceState: RevisionedState {
    fn mark_closed(&mut self);
}

impl ResourceState for crate::ffi::PublicationState {
    fn mark_closed(&mut self) {
        self.closed = true;
    }
}

impl ResourceState for crate::ffi::WatchState {
    fn mark_closed(&mut self) {
        self.close();
    }
}

impl ResourceState for crate::ffi::PipeState {
    fn mark_closed(&mut self) {
        self.status = crate::ffi::PipeStatus::Closed;
    }
}

macro_rules! closable_resource_state {
    ($($state:ty),+ $(,)?) => {$ (
        impl ResourceState for $state {
            fn mark_closed(&mut self) {
                self.closed = true;
            }
        }
    )+ };
}

closable_resource_state!(
    crate::ffi::VideoSubscriptionState,
    crate::ffi::AudioSubscriptionState,
    crate::ffi::DataSubscriptionState,
    crate::ffi::DataPublicationState,
);

impl<T: RevisionedState> Hub<T> {
    fn set(
        &self,
        value: T,
        closing: bool,
        source_revision: Option<u64>,
    ) -> Result<PublishResult, RevisionExhausted> {
        let result = self.prepare(value, closing, source_revision)?;
        if result == PublishResult::Published {
            self.notify();
        }
        Ok(result)
    }

    fn prepare(
        &self,
        mut value: T,
        closing: bool,
        source_revision: Option<u64>,
    ) -> Result<PublishResult, RevisionExhausted> {
        let mut hub = lock(&self.inner);
        if hub.closed {
            return Ok(PublishResult::Closed);
        }
        if let Some(revision) = source_revision
            && hub
                .source_revision
                .is_some_and(|current| current >= revision)
        {
            return Ok(PublishResult::Unchanged);
        }
        value.set_revision(hub.revision);
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
        value.set_revision(revision);
        hub.revision = revision;
        hub.value = value;
        hub.closed = closing;
        Ok(PublishResult::Published)
    }

    fn notify(&self) {
        let wakes = core::mem::take(&mut lock(&self.inner).waiters);
        for (_, waker) in wakes {
            waker.wake();
        }
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

/// Core-internal resource snapshot; foreign APIs expose concrete records and cursors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObserverSnapshot<T> {
    pub revision: u64,
    pub value: T,
}

impl<T: Clone + PartialEq> RevisionedState for ObserverSnapshot<T> {
    fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }
}

pub struct BoundedUpdates<T: Clone> {
    cursor: Cursor<ObserverSnapshot<T>>,
}

impl<T: Clone> BoundedUpdates<T> {
    pub fn next(
        &self,
    ) -> Result<impl Future<Output = Option<ObserverSnapshot<T>>>, ConcurrentNext> {
        self.cursor.next()
    }

    pub fn close(&self) {
        self.cursor.close();
    }
}

/// Core-internal shared coordinator, not a generic foreign observable API.
pub struct BoundedObserver<T> {
    hub: Hub<ObserverSnapshot<T>>,
}

impl<T: Clone + PartialEq> BoundedObserver<T> {
    pub fn new(value: T) -> Self {
        Self {
            hub: Hub::new(ObserverSnapshot { revision: 1, value }, 1),
        }
    }

    pub fn state(&self) -> ObserverSnapshot<T> {
        lock(&self.hub.inner).value.clone()
    }

    pub fn updates(&self) -> Result<BoundedUpdates<T>, ObserverLimit> {
        Ok(BoundedUpdates {
            cursor: self.hub.subscribe()?,
        })
    }

    pub fn publish(&self, value: T) -> Result<PublishResult, RevisionExhausted> {
        self.set(value, false, None)
    }

    pub fn publish_from(
        &self,
        source_revision: u64,
        value: T,
    ) -> Result<PublishResult, RevisionExhausted> {
        self.set(value, false, Some(source_revision))
    }

    pub fn close(&self, value: T) -> Result<PublishResult, RevisionExhausted> {
        self.set(value, true, None)
    }

    fn set(
        &self,
        value: T,
        closing: bool,
        source_revision: Option<u64>,
    ) -> Result<PublishResult, RevisionExhausted> {
        self.hub.set(
            ObserverSnapshot { revision: 0, value },
            closing,
            source_revision,
        )
    }
}

macro_rules! resource_observer {
    ($observer:ident, $updates:ident, $notification:ident, $state:ty) => {
        pub struct $updates {
            cursor: Cursor<$state>,
        }

        impl $updates {
            pub fn next(&self) -> Result<impl Future<Output = Option<$state>>, ConcurrentNext> {
                self.cursor.next()
            }

            pub fn close(&self) {
                self.cursor.close();
            }
        }

        #[must_use = "notify after releasing the owner's serialization guard"]
        pub struct $notification {
            hub: Hub<$state>,
            result: PublishResult,
        }

        impl $notification {
            pub fn notify(self) -> PublishResult {
                if self.result == PublishResult::Published {
                    self.hub.notify();
                }
                self.result
            }
        }

        pub struct $observer {
            hub: Hub<$state>,
        }

        impl $observer {
            pub fn new(mut initial: $state) -> Self {
                initial.set_revision(1);
                Self {
                    hub: Hub::new(initial, 1),
                }
            }

            pub fn state(&self) -> $state {
                lock(&self.hub.inner).value.clone()
            }

            pub fn updates(&self) -> Result<$updates, ObserverLimit> {
                Ok($updates {
                    cursor: self.hub.subscribe()?,
                })
            }

            /// Commit under the owner's serialization guard, then notify after releasing it.
            /// Deferred notifications never republish a captured value or roll back newer state.
            pub fn prepare(
                &self,
                mut state: $state,
                closing: bool,
            ) -> Result<$notification, RevisionExhausted> {
                if closing {
                    state.mark_closed();
                }
                let result = self.hub.prepare(state, closing, None)?;
                Ok($notification {
                    hub: Hub {
                        inner: self.hub.inner.clone(),
                    },
                    result,
                })
            }

            pub fn publish_from(
                &self,
                source_revision: u64,
                state: $state,
            ) -> Result<PublishResult, RevisionExhausted> {
                self.hub.set(state, false, Some(source_revision))
            }

            pub fn close(&self, mut state: $state) -> Result<PublishResult, RevisionExhausted> {
                state.mark_closed();
                self.hub.set(state, true, None)
            }
        }
    };
}

resource_observer!(
    PublicationStateObserver,
    PublicationUpdates,
    PublicationNotification,
    crate::ffi::PublicationState
);
resource_observer!(
    PipeStateObserver,
    PipeUpdates,
    PipeNotification,
    crate::ffi::PipeState
);
resource_observer!(
    VideoSubscriptionStateObserver,
    VideoSubscriptionUpdates,
    VideoSubscriptionNotification,
    crate::ffi::VideoSubscriptionState
);
resource_observer!(
    AudioSubscriptionStateObserver,
    AudioSubscriptionUpdates,
    AudioSubscriptionNotification,
    crate::ffi::AudioSubscriptionState
);
resource_observer!(
    DataSubscriptionStateObserver,
    DataSubscriptionUpdates,
    DataSubscriptionNotification,
    crate::ffi::DataSubscriptionState
);
resource_observer!(
    DataPublicationStateObserver,
    DataPublicationUpdates,
    DataPublicationNotification,
    crate::ffi::DataPublicationState
);

resource_observer!(
    WatchStateObserver,
    WatchUpdates,
    WatchNotification,
    crate::ffi::WatchState
);

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
        value: AgentState,
        closing: bool,
        source_revision: Option<u64>,
    ) -> Result<PublishResult, RevisionExhausted> {
        self.hub.set(value, closing, source_revision)
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
    fn prepared_publications_commit_in_order_and_notify_without_republishing() {
        let mut core = crate::Agent::new(crate::AgentConfig {
            endpoint: "http://pulsebeam.test".into(),
            token: "token".into(),
            retry: Default::default(),
            log_level: Default::default(),
        })
        .unwrap();
        let crate::ResourceResult::Publication { owner, .. } = core
            .resource(crate::ResourceCommand::ClaimPublication {
                spec: crate::TrackSpec::Video {
                    label: "camera".into(),
                },
            })
            .unwrap()
        else {
            panic!("expected publication");
        };
        let observer = PublicationStateObserver::new(core.publication_state(owner).unwrap());
        let cursor = observer.updates().unwrap();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut cx = Context::from_waker(&waker);
        let mut initial = Box::pin(cursor.next().unwrap());
        assert!(initial.as_mut().poll(&mut cx).is_ready());
        let mut pending = Box::pin(cursor.next().unwrap());
        assert!(pending.as_mut().poll(&mut cx).is_pending());

        core.resource(crate::ResourceCommand::SetPublicationMuted { owner, muted: true })
            .unwrap();
        let older = observer
            .prepare(core.publication_state(owner).unwrap(), false)
            .unwrap();
        assert!(!observer.state().configuration.enabled);
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
        core.resource(crate::ResourceCommand::SetPublicationMuted {
            owner,
            muted: false,
        })
        .unwrap();
        let newer = observer
            .prepare(core.publication_state(owner).unwrap(), false)
            .unwrap();
        let latest = observer.state();
        assert!(latest.configuration.enabled);
        newer.notify();
        older.notify();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(pending.as_mut().poll(&mut cx), Poll::Ready(Some(latest)));

        let mut final_read = Box::pin(cursor.next().unwrap());
        assert!(final_read.as_mut().poll(&mut cx).is_pending());
        core.resource(crate::ResourceCommand::SetPublicationMuted { owner, muted: true })
            .unwrap();
        drop(
            observer
                .prepare(core.publication_state(owner).unwrap(), false)
                .unwrap(),
        );
        let closing = observer
            .prepare(core.publication_state(owner).unwrap(), true)
            .unwrap();
        assert!(observer.state().closed);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        closing.notify();
        assert_eq!(counter.0.load(Ordering::SeqCst), 2);
        assert_eq!(
            final_read.as_mut().poll(&mut cx),
            Poll::Ready(Some(observer.state()))
        );
        let mut ended = Box::pin(cursor.next().unwrap());
        assert_eq!(ended.as_mut().poll(&mut cx), Poll::Ready(None));
    }

    #[test]
    fn pipe_cursors_retain_runtime_failure_and_deliver_final_close() {
        use crate::ffi::{ErrorCode, ErrorInfo, PipeState, PipeStatus};

        let observer = PipeStateObserver::new(PipeState {
            observation_revision: 0,
            status: PipeStatus::Pending,
            error: None,
        });
        let a = observer.updates().unwrap();
        let b = observer.updates().unwrap();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut cx = Context::from_waker(&waker);
        for cursor in [&a, &b] {
            let mut read = Box::pin(cursor.next().unwrap());
            assert_eq!(
                read.as_mut().poll(&mut cx),
                Poll::Ready(Some(observer.state()))
            );
        }
        let mut pending = Box::pin(a.next().unwrap());
        assert!(pending.as_mut().poll(&mut cx).is_pending());
        let failed = observer
            .prepare(
                PipeState {
                    observation_revision: 0,
                    status: PipeStatus::Unavailable,
                    error: Some(ErrorInfo::new(
                        ErrorCode::Unsupported,
                        "Input operation failed",
                    )),
                },
                false,
            )
            .unwrap();
        assert_eq!(observer.state().observation_revision, 2);
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
        let closing = observer.prepare(observer.state(), true).unwrap();
        let final_state = observer.state();
        assert_eq!(final_state.status, PipeStatus::Closed);
        assert_eq!(
            final_state.error.as_ref().unwrap().code,
            ErrorCode::Unsupported
        );
        closing.notify();
        failed.notify();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            pending.as_mut().poll(&mut cx),
            Poll::Ready(Some(final_state.clone()))
        );
        let mut coalesced = Box::pin(b.next().unwrap());
        assert_eq!(
            coalesced.as_mut().poll(&mut cx),
            Poll::Ready(Some(final_state))
        );
        for cursor in [&a, &b] {
            let mut ended = Box::pin(cursor.next().unwrap());
            assert_eq!(ended.as_mut().poll(&mut cx), Poll::Ready(None));
        }
    }

    #[test]
    fn data_subscription_observers_keep_independent_cursors_and_terminal_failure_on_close() {
        use crate::ffi::{
            DataSubscriptionState, ErrorCode, ErrorInfo, PublicationAdmissionStatus,
            PublicationExecution, SubscriptionSourceStatus, TopicMode, TrackSelector,
        };

        let initial = DataSubscriptionState {
            observation_revision: 0,
            intent_revision: 3,
            evaluated_revision: 0,
            accepted_revision: None,
            rejected_revision: None,
            selector: TrackSelector {
                room: "source-room".into(),
                participant_id: "publisher-endpoint".into(),
                label: "events".into(),
            },
            delivery: TopicMode::Ordered,
            local_enabled: true,
            closed: false,
            source_status: crate::ffi::SubscriptionSourceStatus::Unavailable,
            source_track_id: None,
            admission: PublicationAdmissionStatus::Pending,
            admission_pending: true,
            error: None,
            execution: PublicationExecution::Unavailable,
            execution_error: None,
            terminal_error: None,
        };
        let observer = DataSubscriptionStateObserver::new(initial);
        let first = observer.updates().unwrap();
        let second = observer.updates().unwrap();
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut context = Context::from_waker(&waker);
        for cursor in [&first, &second] {
            let mut initial = Box::pin(cursor.next().unwrap());
            assert_eq!(
                initial.as_mut().poll(&mut context),
                Poll::Ready(Some(observer.state()))
            );
        }
        let mut pending = Box::pin(first.next().unwrap());
        assert!(pending.as_mut().poll(&mut context).is_pending());
        assert!(first.next().is_err());

        let mut failed_state = observer.state();
        failed_state.source_status = SubscriptionSourceStatus::Available;
        failed_state.source_track_id = Some("opaque-track".into());
        failed_state.admission = PublicationAdmissionStatus::Admitted;
        failed_state.admission_pending = false;
        failed_state.execution = PublicationExecution::Installed;
        failed_state.terminal_error = Some(ErrorInfo::new(
            ErrorCode::ContinuityLost,
            "Reliable receive continuity was lost",
        ));
        let failed = observer.prepare(failed_state, false).unwrap();
        let closing = observer.prepare(observer.state(), true).unwrap();
        failed.notify();
        closing.notify();
        let final_state = observer.state();
        assert_eq!(final_state.observation_revision, 3);
        assert!(final_state.closed);
        assert_eq!(
            final_state.terminal_error.as_ref().map(|error| error.code),
            Some(ErrorCode::ContinuityLost)
        );
        assert_eq!(
            pending.as_mut().poll(&mut context),
            Poll::Ready(Some(final_state.clone()))
        );
        let mut coalesced = Box::pin(second.next().unwrap());
        assert_eq!(
            coalesced.as_mut().poll(&mut context),
            Poll::Ready(Some(final_state))
        );
        for cursor in [&first, &second] {
            let mut ended = Box::pin(cursor.next().unwrap());
            assert_eq!(ended.as_mut().poll(&mut context), Poll::Ready(None));
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
    fn resource_observers_share_bounded_cursors_and_source_fencing() {
        let observer = BoundedObserver::new(0_u8);
        let cursors = (0..MAX_OBSERVERS)
            .map(|_| observer.updates().unwrap())
            .collect::<Vec<_>>();
        assert!(observer.updates().is_err());
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter);
        let mut context = Context::from_waker(&waker);
        let mut initial = Box::pin(cursors[0].next().unwrap());
        assert_eq!(
            initial.as_mut().poll(&mut context),
            Poll::Ready(Some(observer.state()))
        );
        drop(initial);
        let mut pending = Box::pin(cursors[0].next().unwrap());
        assert!(pending.as_mut().poll(&mut context).is_pending());
        assert!(cursors[0].next().is_err());
        assert_eq!(observer.publish_from(4, 0), Ok(PublishResult::Unchanged));
        assert_eq!(observer.publish_from(3, 1), Ok(PublishResult::Unchanged));
        assert_eq!(
            observer.state(),
            ObserverSnapshot {
                revision: 1,
                value: 0
            }
        );
        assert_eq!(observer.publish_from(5, 1), Ok(PublishResult::Published));
        assert_eq!(
            pending.as_mut().poll(&mut context),
            Poll::Ready(Some(observer.state()))
        );
        drop(pending);
        assert_eq!(observer.close(2), Ok(PublishResult::Published));
        let final_state = observer.state();
        assert_eq!(
            final_state,
            ObserverSnapshot {
                revision: 3,
                value: 2
            }
        );
        assert_eq!(observer.publish_from(6, 3), Ok(PublishResult::Closed));
        let mut final_read = Box::pin(cursors[1].next().unwrap());
        assert_eq!(
            final_read.as_mut().poll(&mut context),
            Poll::Ready(Some(final_state))
        );
        drop(final_read);
        let mut end = Box::pin(cursors[1].next().unwrap());
        assert_eq!(end.as_mut().poll(&mut context), Poll::Ready(None));
        drop(end);
        let mut cancelled = Box::pin(cursors[0].next().unwrap());
        cursors[0].close();
        assert_eq!(cancelled.as_mut().poll(&mut context), Poll::Ready(None));
        drop(cancelled);
        drop(cursors);
        assert!(observer.updates().is_ok());
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
