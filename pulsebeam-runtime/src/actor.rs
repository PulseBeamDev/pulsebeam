use futures_lite::FutureExt;
use std::any::Any;
use std::fmt::{Debug, Display, Formatter};
use std::panic::{AssertUnwindSafe, RefUnwindSafe, UnwindSafe};
use thiserror::Error;
use tracing::Instrument;

use crate::mailbox;

#[derive(Error, Debug)]
pub enum ActorError {
    #[error("Actor logic encountered an error: {0}")]
    LogicError(String),
    #[error("Custom actor error: {0}")]
    Custom(String),
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

pub struct ActorContext<A: Actor> {
    pub hi_rx: mailbox::Receiver<A::HighPriorityMessage>,
    pub lo_rx: mailbox::Receiver<A::LowPriorityMessage>,
    pub handle: LocalActorHandle<A>,
}

pub trait Actor: Sized {
    type HighPriorityMessage: Send;
    type LowPriorityMessage: Send;
    type ID: Eq + std::hash::Hash + Display + Debug + Clone + Send + UnwindSafe + RefUnwindSafe;

    fn id(&self) -> Self::ID;

    fn run(&mut self, ctx: &mut ActorContext<Self>)
    -> impl Future<Output = Result<(), ActorError>>;
}

pub trait ActorHandle<A: Actor>: Clone + Send + Sync {
    fn lo_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> impl Future<Output = Result<(), mailbox::SendError<A::LowPriorityMessage>>>;

    fn lo_try_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mailbox::TrySendError<A::LowPriorityMessage>>;

    fn hi_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> impl Future<Output = Result<(), mailbox::SendError<A::HighPriorityMessage>>>;

    fn hi_try_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mailbox::TrySendError<A::HighPriorityMessage>>;
}

pub trait ActorFactory<A: Actor>: Send + 'static {
    fn prepare(&self, actor: A, config: RunnerConfig) -> (LocalActorHandle<A>, Runner<A>);
}

impl<A, F> ActorFactory<A> for F
where
    A: Actor,
    F: Fn(A, RunnerConfig) -> (LocalActorHandle<A>, Runner<A>) + Send + 'static,
{
    fn prepare(&self, actor: A, config: RunnerConfig) -> (LocalActorHandle<A>, Runner<A>) {
        self(actor, config)
    }
}

// Default implementation using LocalActorHandle::new
impl<A: Actor> ActorFactory<A> for () {
    fn prepare(&self, actor: A, config: RunnerConfig) -> (LocalActorHandle<A>, Runner<A>) {
        LocalActorHandle::new(actor, config)
    }
}

pub struct LocalActorHandle<A: Actor> {
    hi_tx: mailbox::Sender<A::HighPriorityMessage>,
    lo_tx: mailbox::Sender<A::LowPriorityMessage>,
}

impl<A: Actor> LocalActorHandle<A> {
    pub fn new(actor: A, config: RunnerConfig) -> (Self, Runner<A>) {
        let (lo_tx, lo_rx) = mailbox::new(config.lo_cap);
        let (hi_tx, hi_rx) = mailbox::new(config.hi_cap);

        let handle = LocalActorHandle { hi_tx, lo_tx };

        let ctx = ActorContext {
            hi_rx,
            lo_rx,
            handle: handle.clone(),
        };

        let runner = Runner { actor, ctx };

        (handle, runner)
    }
}

impl<A: Actor> Clone for LocalActorHandle<A> {
    fn clone(&self) -> Self {
        Self {
            hi_tx: self.hi_tx.clone(),
            lo_tx: self.lo_tx.clone(),
        }
    }
}

impl<A: Actor> ActorHandle<A> for LocalActorHandle<A> {
    #[inline]
    async fn lo_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mailbox::SendError<A::LowPriorityMessage>> {
        self.lo_tx.send(message).await
    }

    #[inline]
    fn lo_try_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mailbox::TrySendError<A::LowPriorityMessage>> {
        self.lo_tx.try_send(message)
    }

    #[inline]
    async fn hi_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mailbox::SendError<A::HighPriorityMessage>> {
        self.hi_tx.send(message).await
    }

    #[inline]
    fn hi_try_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mailbox::TrySendError<A::HighPriorityMessage>> {
        self.hi_tx.try_send(message)
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
    actor: A,
    ctx: ActorContext<A>,
}

impl<A: Actor> Runner<A> {
    pub async fn run(self) -> (A::ID, ActorStatus) {
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
