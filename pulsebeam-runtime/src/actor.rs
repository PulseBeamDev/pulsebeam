use crate::actor_loop;
use futures::FutureExt;
use std::any::Any;
use std::fmt::{Debug, Display, Formatter};
use std::hash::Hash;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use thiserror::Error;
use tracing::Instrument;

use crate::mailbox;

pub type JoinHandle<A: Actor> = Pin<Box<dyn Future<Output = (A::Meta, ActorStatus)> + Send>>;

#[derive(Error, Debug)]
pub enum ActorError {
    #[error("Actor logic encountered an error: {0}")]
    LogicError(String),
    #[error("Custom actor error: {0}")]
    Custom(String),
    #[error("Failed to send a system message, it may have shut down")]
    SystemError,
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
    Terminate,
}

pub struct ActorContext<A: Actor> {
    pub sys_rx: mailbox::Receiver<SystemMsg<A::ObservableState>>,
    pub hi_rx: mailbox::Receiver<A::HighPriorityMsg>,
    pub lo_rx: mailbox::Receiver<A::LowPriorityMsg>,
    pub handle: ActorHandle<A>,
}

pub trait Actor: Sized + Send + 'static {
    /// The type of high-priority messages this actor processes.
    type HighPriorityMsg: Debug + Send + 'static;
    /// The type of low-priority messages this actor processes.
    type LowPriorityMsg: Debug + Send + 'static;
    /// The unique identifier for this actor.
    type Meta: Eq + Hash + Debug + Clone + Send;

    /// ObservableState is a state snapshot of an actor. This is mainly used
    /// for testing.
    type ObservableState: std::fmt::Debug + Send + Sync + Clone;

    /// Returns the actor's unique meta
    fn meta(&self) -> Self::Meta;

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
    ) -> impl Future<Output = Result<(), ActorError>> + Send {
        async {
            actor_loop!(self, ctx);
            Ok(())
        }
    }

    /// Handles a syste message
    #[allow(unused_variables)]
    fn on_system(
        &mut self,
        ctx: &mut ActorContext<Self>,
        msg: SystemMsg<Self::ObservableState>,
    ) -> impl Future<Output = ()> + Send {
        async move {}
    }

    /// Handles a high-priority message.
    #[allow(unused_variables)]
    fn on_high_priority(
        &mut self,
        ctx: &mut ActorContext<Self>,
        msg: Self::HighPriorityMsg,
    ) -> impl Future<Output = ()> + Send {
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
    ) -> impl Future<Output = ()> + Send {
        async {
            todo!("unimplemented!");
        }
    }
}

// pub trait ActorFactory<A: Actor> {
//     fn prepare(
//         &self,
//         actor: A,
//         config: RunnerConfig,
//     ) -> (ActorHandle<A>, JoinHandle<(A::ActorId, ActorStatus)>);
// }
//
// impl<A, F> ActorFactory<A> for F
// where
//     A: Actor,
//     F: Fn(A, RunnerConfig) -> (ActorHandle<A>, JoinHandle<(A::ActorId, ActorStatus)>),
// {
//     fn prepare(
//         &self,
//         actor: A,
//         config: RunnerConfig,
//     ) -> (ActorHandle<A>, JoinHandle<(A::ActorId, ActorStatus)>) {
//         self(actor, config)
//     }
// }
//
// // Default implementation using LocalActorHandle::new
// impl<A: Actor> ActorFactory<A> for () {
//     fn prepare(
//         &self,
//         actor: A,
//         config: RunnerConfig,
//     ) -> (ActorHandle<A>, JoinHandle<(A::ActorId, ActorStatus)>) {
//         spawn(actor, config)
//     }
// }

/// A handle for sending high- and low-priority messages to an actor.
pub struct ActorHandle<A: Actor> {
    sys_tx: mailbox::Sender<SystemMsg<A::ObservableState>>,
    hi_tx: mailbox::Sender<A::HighPriorityMsg>,
    lo_tx: mailbox::Sender<A::LowPriorityMsg>,
    pub meta: A::Meta,
}

impl<A: Actor> Clone for ActorHandle<A> {
    fn clone(&self) -> Self {
        Self {
            sys_tx: self.sys_tx.clone(),
            hi_tx: self.hi_tx.clone(),
            lo_tx: self.lo_tx.clone(),
            meta: self.meta.clone(),
        }
    }
}

impl<A: Actor> std::fmt::Debug for ActorHandle<A> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.meta.fmt(f)
    }
}

impl<A: Actor> ActorHandle<A> {
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
            .map_err(|_| ActorError::SystemError)?;

        // Await the response. If it fails, the actor might have panicked or been terminated.
        rx.await.map_err(|_| ActorError::SystemError)
    }

    pub async fn terminate(&self) -> Result<(), ActorError> {
        self.sys_tx
            .send(SystemMsg::Terminate)
            .await
            .map_err(|_| ActorError::SystemError)
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

pub fn spawn<A: Actor>(a: A, config: RunnerConfig) -> (ActorHandle<A>, JoinHandle<A>) {
    let (lo_tx, lo_rx) = mailbox::new(config.lo_cap);
    let (hi_tx, hi_rx) = mailbox::new(config.hi_cap);
    let (sys_tx, sys_rx) = mailbox::new(1);

    let handle = ActorHandle {
        hi_tx,
        lo_tx,
        sys_tx,
        meta: a.meta(),
    };

    let ctx = ActorContext {
        sys_rx,
        hi_rx,
        lo_rx,
        handle: handle.clone(),
    };

    let actor_id = a.meta().clone();
    let join = tokio::spawn(run(a, ctx).instrument(tracing::span!(
        tracing::Level::INFO,
        "run",
        ?actor_id
    )))
    .map(|res| match res {
        Ok(ret) => (actor_id, ret),
        Err(_) => (actor_id, ActorStatus::ShutDown),
    })
    .boxed();

    (handle, join)
}

pub fn spawn_default<A: Actor>(
    a: A,
) -> (ActorHandle<A>, impl Future<Output = (A::Meta, ActorStatus)>) {
    spawn(a, RunnerConfig::default())
}

async fn run<A: Actor>(mut a: A, mut ctx: ActorContext<A>) -> ActorStatus {
    tracing::debug!("Starting actor...");

    let run_result = AssertUnwindSafe(a.run(&mut ctx)).catch_unwind().await;

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

fn extract_panic_message(payload: &Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        format!("{:?}", payload)
    }
}

#[macro_export]
macro_rules! actor_loop {
    // Simple case - just actor and context
    ($actor:ident, $ctx:ident) => {
        actor_loop!($actor, $ctx, pre_select: {}, select: {})
    };

    // With only pre_select
    ($actor:ident, $ctx:ident, pre_select: { $($pre_select:tt)* }) => {
        actor_loop!($actor, $ctx, pre_select: { $($pre_select)* }, select: {})
    };

    // Full form with both pre_select and select
    ($actor:ident, $ctx:ident, pre_select: { $($pre_select:tt)* }, select: { $($extra:tt)* }) => {

        loop {
            $($pre_select)*

            tokio::select! {
                biased;
                res = $ctx.sys_rx.recv() => {
                    match res {
                        Some(msg) => {
                            match msg {
                                $crate::actor::SystemMsg::GetState(responder) => {
                                    let _ = responder.send($actor.get_observable_state());
                                }
                                $crate::actor::SystemMsg::Terminate => {
                                    break;
                                }
                            }
                        }
                        None => break,
                    }
                }
                res = $ctx.hi_rx.recv() => {
                    match res {
                        Some(msg) => $actor.on_high_priority($ctx, msg).await,
                        None => break,
                    }
                }
                res = $ctx.lo_rx.recv() => {
                    match res {
                        Some(msg) => $actor.on_low_priority($ctx, msg).await,
                        None => break,
                    }
                }
                $($extra)*
                else => break,
            }
        }
    };
}
