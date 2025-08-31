use futures_lite::FutureExt;
use std::any::Any;
use std::fmt::{Debug, Display, Formatter};
use std::hash::Hash;
use std::panic::AssertUnwindSafe;
use thiserror::Error;
use tracing::Instrument;

use crate::mailbox;

#[derive(Error, Debug)]
pub enum ActorError {
    #[error("Actor logic encountered an error: {0}")]
    LogicError(String),
    #[error("Custom actor error: {0}")]
    Custom(String),
    #[error("Failed to receive state from actor, it may have shut down")]
    StateReceiverError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorStatus {
    Starting,
    Running,
    ExitedGracefully, // run_actor_logic returned Ok
    ExitedWithError,  // run_actor_logic returned Err
    Panicked,
    ShutDown, // Successfully completed all stages it attempted
}

impl Display for ActorStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                ActorStatus::Starting => "starting",
                ActorStatus::Running => "running",
                ActorStatus::ExitedGracefully => "exited_gracefully",
                ActorStatus::ExitedWithError => "exited_with_error",
                ActorStatus::Panicked => "panicked",
                ActorStatus::ShutDown => "shut_down",
            }
        )
    }
}

#[derive(Debug)]
pub enum SystemMsg<S> {
    GetState(tokio::sync::oneshot::Sender<S>),
}

pub struct ActorContext<A: Actor> {
    pub sys_rx: mailbox::Receiver<SystemMsg<A::ObservableState>>,
    pub hi_rx: mailbox::Receiver<A::HighPriorityMsg>,
    pub lo_rx: mailbox::Receiver<A::LowPriorityMsg>,
    pub handle: ActorHandle<A>,
}

pub trait Actor: Sized {
    /// The type of high-priority messages this actor processes.
    type HighPriorityMsg: Send + 'static;
    /// The type of low-priority messages this actor processes.
    type LowPriorityMsg: Send + 'static;
    /// The unique identifier for this actor.
    type ActorId: Eq + Hash + Debug + Clone + Send;

    /// ObservableState is a state snapshot of an actor. This is mainly used
    /// for testing.
    type ObservableState: std::fmt::Debug + Send + Sync + Clone;

    /// Returns the actor's unique identifier.
    fn id(&self) -> Self::ActorId;

    // Each actor implementor is responsible for defining what state is observable.
    fn get_observable_state(&self) -> Self::ObservableState;

    /// Runs the actor's main message-processing loop.
    ///
    /// The default implementation processes high-priority messages before low-priority ones
    /// using `tokio::select!` with biased polling. Implementors may override this method
    /// for custom behavior.
    fn run(
        &mut self,
        ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = Result<(), ActorError>> {
        async {
            loop {
                tokio::select! {
                    biased;

                    Some(msg) = ctx.sys_rx.recv() => {
                        self.on_system(ctx, msg).await;
                    }

                    Some(msg) = ctx.hi_rx.recv() => {
                        self.on_high_priority(ctx, msg).await;
                    }

                    Some(msg) = ctx.lo_rx.recv() => {
                        self.on_low_priority(ctx, msg).await;
                    }

                    else => break,
                }
            }
            Ok(())
        }
    }

    /// Handles a syste message
    #[allow(unused_variables)]
    fn on_system(
        &mut self,
        ctx: &mut ActorContext<Self>,
        msg: SystemMsg<Self::ObservableState>,
    ) -> impl Future<Output = ()> {
        async {
            match msg {
                SystemMsg::GetState(responder) => {
                    // The actor provides its state, and we send it back.
                    // We ignore the result of the send; if the requester is gone, that's okay.
                    let _ = responder.send(self.get_observable_state());
                }
            }
        }
    }

    /// Handles a high-priority message.
    #[allow(unused_variables)]
    fn on_high_priority(
        &mut self,
        ctx: &mut ActorContext<Self>,
        msg: Self::HighPriorityMsg,
    ) -> impl Future<Output = ()> {
        async {
            todo!("unimplemented!");
        }
    }

    /// Handles a low-priority message.
    #[allow(unused_variables)]
    fn on_low_priority(
        &mut self,
        ctx: &mut ActorContext<Self>,
        msg: Self::LowPriorityMsg,
    ) -> impl Future<Output = ()> {
        async {
            todo!("unimplemented!");
        }
    }
}

pub trait ActorFactory<A: Actor>: Send + Sync + 'static {
    fn prepare(&self, actor: A, config: RunnerConfig) -> (ActorHandle<A>, Runner<A>);
}

impl<A, F> ActorFactory<A> for F
where
    A: Actor,
    F: Fn(A, RunnerConfig) -> (ActorHandle<A>, Runner<A>) + Send + Sync + 'static,
{
    fn prepare(&self, actor: A, config: RunnerConfig) -> (ActorHandle<A>, Runner<A>) {
        self(actor, config)
    }
}

// Default implementation using LocalActorHandle::new
impl<A: Actor> ActorFactory<A> for () {
    fn prepare(&self, actor: A, config: RunnerConfig) -> (ActorHandle<A>, Runner<A>) {
        ActorHandle::new(actor, config)
    }
}

/// A handle for sending high- and low-priority messages to an actor.
pub struct ActorHandle<A: Actor> {
    sys_tx: mailbox::Sender<SystemMsg<A::ObservableState>>,
    hi_tx: mailbox::Sender<A::HighPriorityMsg>,
    lo_tx: mailbox::Sender<A::LowPriorityMsg>,
}

impl<A: Actor> ActorHandle<A> {
    pub fn new(actor: A, config: RunnerConfig) -> (Self, Runner<A>) {
        let (lo_tx, lo_rx) = mailbox::new(config.lo_cap);
        let (hi_tx, hi_rx) = mailbox::new(config.hi_cap);
        let (sys_tx, sys_rx) = mailbox::new(1);

        let handle = ActorHandle {
            hi_tx,
            lo_tx,
            sys_tx,
        };

        let ctx = ActorContext {
            sys_rx,
            hi_rx,
            lo_rx,
            handle: handle.clone(),
        };

        let runner = Runner { actor, ctx };

        (handle, runner)
    }

    pub fn new_default(actor: A) -> (Self, Runner<A>) {
        Self::new(actor, RunnerConfig::default())
    }

    /// Sends a high-priority message asynchronously.
    ///
    /// Returns an error if the actor's mailbox is closed.
    #[inline]
    pub async fn send_high(
        &self,
        message: A::HighPriorityMsg,
    ) -> Result<(), mailbox::SendError<A::HighPriorityMsg>> {
        self.hi_tx.send(message).await
    }

    /// Attempts to send a high-priority message synchronously.
    ///
    /// Returns an error if the mailbox is full or closed.
    #[inline]
    pub fn try_send_high(
        &self,
        message: A::HighPriorityMsg,
    ) -> Result<(), mailbox::TrySendError<A::HighPriorityMsg>> {
        self.hi_tx.try_send(message)
    }

    /// Sends a low-priority message asynchronously.
    ///
    /// Returns an error if the actor's mailbox is closed.
    #[inline]
    pub async fn send_low(
        &self,
        message: A::LowPriorityMsg,
    ) -> Result<(), mailbox::SendError<A::LowPriorityMsg>> {
        self.lo_tx.send(message).await
    }

    /// Attempts to send a low-priority message synchronously.
    ///
    /// Returns an error if the mailbox is full or closed.
    #[inline]
    pub fn try_send_low(
        &self,
        message: A::LowPriorityMsg,
    ) -> Result<(), mailbox::TrySendError<A::LowPriorityMsg>> {
        self.lo_tx.try_send(message)
    }

    pub async fn get_state(&self) -> Result<A::ObservableState, ActorError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let msg = SystemMsg::GetState(tx);

        // Send the request. If it fails, the actor is gone.
        self.sys_tx
            .send(msg)
            .await
            .map_err(|_| ActorError::StateReceiverError)?;

        // Await the response. If it fails, the actor might have panicked or been terminated.
        rx.await.map_err(|_| ActorError::StateReceiverError)
    }
}

impl<A: Actor> Clone for ActorHandle<A> {
    fn clone(&self) -> Self {
        Self {
            hi_tx: self.hi_tx.clone(),
            lo_tx: self.lo_tx.clone(),
            sys_tx: self.sys_tx.clone(),
        }
    }
}

pub struct RunnerConfig {
    pub lo_cap: usize,
    pub hi_cap: usize,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            lo_cap: 1,
            hi_cap: 1,
        }
    }
}

impl RunnerConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_lo(mut self, cap: usize) -> Self {
        self.lo_cap = cap;
        self
    }

    pub fn with_hi(mut self, cap: usize) -> Self {
        self.hi_cap = cap;
        self
    }
}

pub struct Runner<A: Actor> {
    pub actor: A,
    pub ctx: ActorContext<A>,
}

impl<A: Actor> Runner<A> {
    pub async fn run(self) -> (A::ActorId, ActorStatus) {
        let actor_id = self.actor.id();

        let fut = async move {
            let status = self.run_instrumented().await;
            (actor_id, status)
        }
        .in_current_span();
        fut.await
    }

    async fn run_instrumented(mut self) -> ActorStatus {
        tracing::debug!("Starting actor...");

        let run_result = AssertUnwindSafe(self.actor.run(&mut self.ctx))
            .catch_unwind()
            .await;

        let status_after_run = match run_result {
            Ok(Ok(())) => {
                tracing::debug!("Main logic exited gracefully.");
                ActorStatus::ExitedGracefully
            }
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "Main logic exited with an error.");
                ActorStatus::ExitedWithError
            }
            Err(panic_payload) => {
                let panic_msg = extract_panic_message(&panic_payload);

                tracing::error!(panic.message = %panic_msg, "Actor panicked!");
                ActorStatus::Panicked
            }
        };

        tracing::debug!("post_stop successful.");
        tracing::info!(status = %status_after_run, "Actor fully shut down.");

        status_after_run
    }
}

fn extract_panic_message(payload: &Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        format!("{:?}", payload)
    }
}

pub struct ObserverActor<A: Actor> {
    pub high_priority_msgs: Vec<A::HighPriorityMsg>,
    pub low_priority_msgs: Vec<A::LowPriorityMsg>,
    pub inner: A,
}

impl<A: Actor> ObserverActor<A> {
    pub fn wrap(inner: A) -> ObserverActor<A> {
        Self {
            high_priority_msgs: Vec::new(),
            low_priority_msgs: Vec::new(),
            inner,
        }
    }
}

impl<A: Actor> Actor for ObserverActor<A> {
    type HighPriorityMsg = A::HighPriorityMsg;
    type LowPriorityMsg = A::LowPriorityMsg;
    type ActorId = A::ActorId;
    type ObservableState = ();

    fn id(&self) -> Self::ActorId {
        self.inner.id()
    }

    fn get_observable_state(&self) -> Self::ObservableState {}

    async fn on_high_priority(
        &mut self,
        _ctx: &mut ActorContext<Self>,
        msg: Self::HighPriorityMsg,
    ) -> () {
        self.high_priority_msgs.push(msg);
    }

    async fn on_low_priority(
        &mut self,
        _ctx: &mut ActorContext<Self>,
        msg: Self::LowPriorityMsg,
    ) -> () {
        self.low_priority_msgs.push(msg);
    }
}
