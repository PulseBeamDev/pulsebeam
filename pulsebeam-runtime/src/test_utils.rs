use crate::actor::*;
use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::sync::Arc;

type HandlerFn<State, Msg> = Arc<dyn Fn(&mut State, Msg) + Send + Sync>;

/// A generic test actor implementation for state-based testing.
///
/// This allows you to define a custom observable state type and provide closures
/// that handle high-priority and low-priority messages by mutating the state.
///
/// The actor uses the default message loop, processing messages and updating state
/// accordingly. System messages (like GetState and Terminate) are handled by default.
///
/// Usage in tests:
/// - Define your State type (must implement Clone for observation).
/// - Provide closures that mutate the state based on received messages.
/// - Spawn the actor, send messages via the handle, and query observable state
///   to assert transitions, aligning with state-based testing principles from
///   "Software Engineering at Google" (focus on pre-state, action, post-state assertions).
///
/// Example:
/// ```
/// struct CounterState {
///     count: i32,
/// }
///
/// let test_actor = TestActor::new(
///     "test_meta".to_string(),
///     CounterState { count: 0 },
///     Arc::new(|state: &mut CounterState, msg: i32| { state.count += msg; }),
///     Arc::new(|state: &mut CounterState, msg: String| { /* ignore or handle */ }),
/// );
///
/// let (handle, join) = spawn(test_actor, RunnerConfig::default());
/// handle.send_high(5).await.unwrap();
/// let new_state = handle.get_state().await.unwrap();
/// assert_eq!(new_state.count, 5);
/// ```
pub struct TestActor<Meta, State, HighMsg, LowMsg> {
    meta: Meta,
    state: State,
    on_high: HandlerFn<State, HighMsg>,
    on_low: HandlerFn<State, LowMsg>,
}

impl<Meta, State, HighMsg, LowMsg> TestActor<Meta, State, HighMsg, LowMsg>
where
    Meta: Eq + Hash + Display + Debug + Clone + Send + 'static,
    State: Debug + Send + Sync + Clone + 'static,
    HighMsg: Debug + Send + 'static,
    LowMsg: Debug + Send + 'static,
{
    /// Creates a new TestActor with the given meta, initial state, and handler closures.
    pub fn new(
        meta: Meta,
        initial_state: State,
        on_high: HandlerFn<State, HighMsg>,
        on_low: HandlerFn<State, LowMsg>,
    ) -> Self {
        Self {
            meta,
            state: initial_state,
            on_high,
            on_low,
        }
    }
}

impl<Meta, State, HighMsg, LowMsg> MessageSet for TestActor<Meta, State, HighMsg, LowMsg>
where
    Meta: Eq + Hash + Display + Debug + Clone + Send + 'static,
    State: Debug + Send + Clone + 'static,
    HighMsg: Debug + Send + 'static,
    LowMsg: Debug + Send + 'static,
{
    type HighPriorityMsg = HighMsg;
    type LowPriorityMsg = LowMsg;
    type Meta = Meta;
    type ObservableState = State;
}

impl<Meta, State, HighMsg, LowMsg> Actor for TestActor<Meta, State, HighMsg, LowMsg>
where
    Meta: Eq + Hash + Display + Debug + Clone + Send + 'static,
    State: Debug + Send + Clone + 'static,
    HighMsg: Debug + Send + 'static,
    LowMsg: Debug + Send + 'static,
{
    fn meta(&self) -> Self::Meta {
        self.meta.clone()
    }

    fn get_observable_state(&self) -> Self::ObservableState {
        self.state.clone()
    }

    fn on_high_priority(
        &mut self,
        _ctx: &mut ActorContext<Self>,
        msg: Self::HighPriorityMsg,
    ) -> impl std::future::Future<Output = ()> + Send {
        (self.on_high)(&mut self.state, msg);
        async {}
    }

    fn on_low_priority(
        &mut self,
        _ctx: &mut ActorContext<Self>,
        msg: Self::LowPriorityMsg,
    ) -> impl std::future::Future<Output = ()> + Send {
        (self.on_low)(&mut self.state, msg);
        async {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, sleep};

    #[tokio::test(start_paused = true)]
    async fn test_join_handle_drop_aborts_actor() {
        // Define minimal types for this test (can be reused or customized).
        type State = i32; // Same as old FakeActor
        type HighMsg = i32;
        type LowMsg = i32;

        let actor = TestActor::new(
            "test_actor".to_string(),
            0,                                                // initial state
            Arc::new(|_state: &mut State, _msg: HighMsg| {}), // no-op handler
            Arc::new(|_state: &mut State, _msg: LowMsg| {}),  // no-op handler
        );

        let (mut handle, join_handle) = spawn(actor, RunnerConfig::default());

        // Act: Drop the join handle
        drop(join_handle);

        // Assert: Actor should be shut down
        sleep(Duration::from_millis(50)).await;
        let result = handle.get_state().await;
        assert!(result.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn test_state_transition_on_message() {
        type State = i32;
        type HighMsg = i32;

        let actor = TestActor::new(
            "counter".to_string(),
            0,
            Arc::new(|state: &mut State, msg: HighMsg| {
                *state += msg;
            }),
            Arc::new(|_state: &mut State, _msg: ()| {}),
        );

        let (mut handle, _join) = spawn(actor, RunnerConfig::default());

        handle.send_high(10).await.unwrap();
        let state = handle.get_state().await.unwrap();
        assert_eq!(state, 10);

        handle.send_high(5).await.unwrap();
        let state = handle.get_state().await.unwrap();
        assert_eq!(state, 15);
    }
}
