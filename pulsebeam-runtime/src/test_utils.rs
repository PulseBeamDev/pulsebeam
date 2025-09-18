use crate::actor::{ActorHandle, MessageSet};
use crate::mailbox;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, MutexGuard};

/// A handle to a spawned fake actor, used for state-based assertions in tests.
/// This is the object your test will hold and interact with.
pub struct FakeActor<S: 'static> {
    state: Arc<Mutex<S>>,
    // We hold the join handle to abort the internal task when the fake is dropped.
    _task_join_handle: tokio::task::JoinHandle<()>,
}

impl<S: 'static> FakeActor<S> {
    /// Provides locked, synchronous access to the fake's internal state for assertions.
    ///
    /// # Example
    /// ```
    /// let state = fake.state();
    /// assert_eq!(state.subscribers.len(), 1);
    /// ```
    pub fn state(&self) -> MutexGuard<'_, S> {
        self.state
            .lock()
            .expect("State mutex poisoned in test. This indicates a test logic error.")
    }

    /// A convenience method for asserting state in a more fluent way.
    /// The test will panic if the closure's assertions fail.
    ///
    /// # Example
    /// ```
    /// fake.assert_state(|state| {
    ///     assert_eq!(state.subscribers.len(), 1);
    /// });
    /// ```
    #[track_caller]
    pub fn assert_state<F>(&self, check: F)
    where
        F: FnOnce(&S),
    {
        check(&self.state());
    }
}

/// A builder for creating `FakeActor` instances declaratively.
/// This is the primary entry point for all tests.
pub struct FakeActorBuilder<A: MessageSet, S> {
    meta: A::Meta,
    state: S,
    on_high: Box<dyn FnMut(&mut S, A::HighPriorityMsg) + Send>,
    on_low: Box<dyn FnMut(&mut S, A::LowPriorityMsg) + Send>,
}

impl<A: MessageSet, S> FakeActorBuilder<A, S>
where
    S: Default + Debug + Clone + Send + Sync + 'static,
    A::HighPriorityMsg: Send + 'static,
    A::LowPriorityMsg: Send + 'static,
{
    /// Starts building a new `FakeActor`.
    ///
    /// It is generic over the REAL actor's `MessageSet` (`A`) and your
    /// test-specific `State` (`S`).
    pub fn new(meta: A::Meta) -> Self {
        Self {
            meta,
            state: Default::default(),
            on_high: Box::new(|_, _| {}), // Default is to do nothing
            on_low: Box::new(|_, _| {}),  // Default is to do nothing
        }
    }

    /// Defines the state transition logic for handling high-priority messages.
    pub fn on_high<F>(mut self, f: F) -> Self
    where
        F: FnMut(&mut S, A::HighPriorityMsg) + Send + 'static,
    {
        self.on_high = Box::new(f);
        self
    }

    /// Defines the state transition logic for handling low-priority messages.
    pub fn on_low<F>(mut self, f: F) -> Self
    where
        F: FnMut(&mut S, A::LowPriorityMsg) + Send + 'static,
    {
        self.on_low = Box::new(f);
        self
    }

    /// Consumes the builder, spawns the fake, and returns two things:
    /// 1. A real `ActorHandle<A>` to inject into the system under test.
    /// 2. A `FakeActor<S>` for your test to use for assertions.
    pub fn build(self) -> (ActorHandle<A>, FakeActor<S>) {
        let (lo_tx, mut lo_rx) = mailbox::new(128);
        let (hi_tx, mut hi_rx) = mailbox::new(128);
        let (sys_tx, _sys_rx) = mailbox::new(128);

        // This is a REAL, correctly-typed handle.
        let handle = ActorHandle {
            hi_tx,
            lo_tx,
            sys_tx,
            meta: self.meta,
        };

        let state = Arc::new(Mutex::new(self.state));
        let task_state = state.clone();
        let mut on_high = self.on_high;
        let mut on_low = self.on_low;

        // Spawn a tiny, self-contained message processing loop.
        let task_join_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = hi_rx.recv() => {
                        on_high(&mut task_state.lock().unwrap(), msg);
                    }
                    Some(msg) = lo_rx.recv() => {
                        on_low(&mut task_state.lock().unwrap(), msg);
                    }
                    else => {
                        // All sender handles were dropped, so the task can exit.
                        break;
                    }
                }
            }
        });

        let fake_actor = FakeActor {
            state,
            _task_join_handle: task_join_handle,
        };

        (handle, fake_actor)
    }
}
