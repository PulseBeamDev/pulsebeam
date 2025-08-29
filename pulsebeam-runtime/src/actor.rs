use futures_lite::FutureExt;
use std::any::Any;
use std::fmt::{Debug, Display, Formatter};
use std::panic::{AssertUnwindSafe, RefUnwindSafe, UnwindSafe};
use thiserror::Error;
use tokio::task::JoinSet;
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

pub trait Actor: Send + Sized + 'static {
    type HighPriorityMessage: Send + 'static;
    type LowPriorityMessage: Send + 'static;
    type ID: Eq
        + std::hash::Hash
        + Display
        + Debug
        + Clone
        + Send
        + Sync
        + UnwindSafe
        + RefUnwindSafe
        + 'static;

    fn kind(&self) -> &'static str;
    fn id(&self) -> Self::ID;

    // Marking Send + Sync because of Rust compiler can't determine if the implementation is
    // Send safe:
    //  * https://blog.rust-lang.org/inside-rust/2023/05/03/stabilizing-async-fn-in-trait/
    //  * https://github.com/rust-lang/rust/issues/103854
    //  * https://users.rust-lang.org/t/why-is-the-future-not-implemented-send/89238
    //  * https://www.reddit.com/r/rust/comments/1mvasiv/generics_with_tokiotaskspawn/
    fn run(
        &mut self,
        ctx: ActorContext<Self>,
    ) -> impl Future<Output = Result<(), ActorError>> + Send + Sync;
}

pub trait ActorHandle<A: Actor>: Clone + Send + Sync + 'static {
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

pub struct LocalActorHandle<A: Actor> {
    hi_tx: mailbox::Sender<A::HighPriorityMessage>,
    lo_tx: mailbox::Sender<A::LowPriorityMessage>,
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
    async fn lo_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mailbox::SendError<A::LowPriorityMessage>> {
        self.lo_tx.send(message).await
    }

    fn lo_try_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mailbox::TrySendError<A::LowPriorityMessage>> {
        self.lo_tx.try_send(message)
    }

    async fn hi_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mailbox::SendError<A::HighPriorityMessage>> {
        self.hi_tx.send(message).await
    }

    fn hi_try_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mailbox::TrySendError<A::HighPriorityMessage>> {
        self.hi_tx.try_send(message)
    }
}

pub trait Spawner<A: Actor> {
    fn spawn<F>(&mut self, fut: F)
    where
        F: Future<Output = (A::ID, ActorStatus)> + Send + 'static;
}

impl<A: Actor> Spawner<A> for tokio::runtime::Handle {
    fn spawn<F>(&mut self, fut: F)
    where
        F: Future<Output = (A::ID, ActorStatus)> + Send + 'static,
    {
        tokio::spawn(fut);
    }
}

impl<A: Actor> Spawner<A> for JoinSet<(A::ID, ActorStatus)> {
    fn spawn<F>(&mut self, fut: F)
    where
        F: Future<Output = (A::ID, ActorStatus)> + Send + 'static,
    {
        self.spawn(fut);
    }
}

pub struct SpawnConfig {
    pub lo_cap: usize,
    pub hi_cap: usize,
}

impl Default for SpawnConfig {
    fn default() -> Self {
        Self {
            lo_cap: 1,
            hi_cap: 1,
        }
    }
}

impl SpawnConfig {
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

pub fn spawn<A, S>(spawner: &mut S, actor: A, config: SpawnConfig) -> LocalActorHandle<A>
where
    A: Actor + 'static,
    S: Spawner<A>,
{
    let (lo_tx, lo_rx) = mailbox::new(config.lo_cap);
    let (hi_tx, hi_rx) = mailbox::new(config.hi_cap);

    let actor_kind = actor.kind();
    let actor_id = actor.id();

    let handle = LocalActorHandle { lo_tx, hi_tx };

    let ctx = ActorContext {
        hi_rx,
        lo_rx,
        handle: handle.clone(),
    };

    let fut = async move {
        let status = run_instrumented(actor, ctx, actor_kind, &actor_id).await;
        (actor_id, status)
    }
    .in_current_span();

    spawner.spawn(fut);
    handle
}

#[tracing::instrument(
    name = "run_instrumented",
    skip_all,
    fields(
        actor.kind = %actor_kind,
        actor.id = %actor_id,
        status = tracing::field::Empty,
    )
)]
async fn run_instrumented<A>(
    mut actor: A,
    ctx: ActorContext<A>,
    actor_kind: &'static str,
    actor_id: &A::ID,
) -> ActorStatus
where
    A: Actor + Send + 'static,
{
    tracing::debug!("Starting actor...");

    let run_result = AssertUnwindSafe(actor.run(ctx)).catch_unwind().await;

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

    // If post_stop was successful, the status is determined by the outcome of the run logic.
    status_after_run
}

fn extract_panic_message(payload: &Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        // It might be other types if `panic_any` was used with a custom payload.
        // For this example, we'll keep it simple. A production version might try more downcasts.
        format!("{:?}", payload) // Fallback to Debug representation of the payload
    }
}
