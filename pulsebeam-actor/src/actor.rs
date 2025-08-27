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
    #[error("Pre-start initialization failed: {0}")]
    PreStartFailed(String),
    #[error("Actor logic encountered an error: {0}")]
    LogicError(String),
    #[error("Post-stop cleanup failed: {0}")]
    PostStopFailed(String),
    #[error("Custom actor error: {0}")]
    Custom(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorStatus {
    Starting,
    PreStartFailed,
    Running,
    ExitedGracefully, // run_actor_logic returned Ok
    ExitedWithError,  // run_actor_logic returned Err
    Panicked,
    PostStopFailed, // post_stop itself failed
    ShutDown,       // Successfully completed all stages it attempted
}

impl Display for ActorStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                ActorStatus::Starting => "starting",
                ActorStatus::PreStartFailed => "pre_start_failed",
                ActorStatus::Running => "running",
                ActorStatus::ExitedGracefully => "exited_gracefully",
                ActorStatus::ExitedWithError => "exited_with_error",
                ActorStatus::Panicked => "panicked",
                ActorStatus::PostStopFailed => "post_stop_failed",
                ActorStatus::ShutDown => "shut_down",
            }
        )
    }
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
        hi_rx: mailbox::Receiver<Self::HighPriorityMessage>,
        lo_rx: mailbox::Receiver<Self::LowPriorityMessage>,
    ) -> impl Future<Output = Result<(), ActorError>> + Send + Sync;

    /// Called once before the main `run` loop starts.
    fn pre_start(&mut self) -> impl Future<Output = Result<(), ActorError>> + Send + Sync {
        async { Ok(()) }
    }

    /// Called once after `run` completes (Ok or Err),
    /// OR if `pre_start` fails, OR if `run_actor_logic` panics.
    /// This function MUST be panic-safe if it's to be called after a panic.
    fn post_stop(&mut self) -> impl Future<Output = Result<(), ActorError>> + Send + Sync {
        async { Ok(()) }
    }
}

pub trait ActorHandle<A: Actor>: std::fmt::Debug + Clone {
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

#[derive(Clone)]
pub struct BaseActorHandle<A: Actor> {
    hi_tx: mailbox::Sender<A::HighPriorityMessage>,
    lo_tx: mailbox::Sender<A::LowPriorityMessage>,
}

impl<A: Actor> BaseActorHandle<A> {
    pub async fn lo_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mailbox::SendError<A::LowPriorityMessage>> {
        self.lo_tx.send(message).await
    }

    pub fn lo_try_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mailbox::TrySendError<A::LowPriorityMessage>> {
        self.lo_tx.try_send(message)
    }

    pub async fn hi_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mailbox::SendError<A::HighPriorityMessage>> {
        self.hi_tx.send(message).await
    }

    pub fn hi_try_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mailbox::TrySendError<A::HighPriorityMessage>> {
        self.hi_tx.try_send(message)
    }
}

pub fn spawn<A: Actor>(
    spawner: &mut JoinSet<(A::ID, ActorStatus)>,
    actor: A,
    lo_cap: usize,
    hi_cap: usize,
) -> BaseActorHandle<A> {
    let (lo_tx, lo_rx) = mailbox::new::<A::LowPriorityMessage>(lo_cap);
    let (hi_tx, hi_rx) = mailbox::new::<A::HighPriorityMessage>(hi_cap);
    let actor_kind = actor.kind();
    let actor_id = actor.id();

    let task = async move {
        let status = run_instrumented(actor, hi_rx, lo_rx, actor_kind, &actor_id).await;
        (actor_id, status)
    }
    .in_current_span();

    spawner.spawn(task);

    BaseActorHandle { lo_tx, hi_tx }
}

#[tracing::instrument(
    name = "actor_run",
    skip_all,
    fields(
        actor.kind = %actor_kind,
        actor.id = %actor_id,
        status = tracing::field::Empty,
    )
)]
async fn run_instrumented<A>(
    mut actor: A,
    hi_rx: mailbox::Receiver<A::HighPriorityMessage>,
    lo_rx: mailbox::Receiver<A::LowPriorityMessage>,
    actor_kind: &'static str,
    actor_id: &A::ID,
) -> ActorStatus
where
    A: Actor + Send + 'static,
{
    tracing::debug!("Starting actor...");

    // 1. Pre-start Hook
    if let Err(err) = actor.pre_start().await {
        tracing::error!(error = %err, "pre_start failed. Proceeding to post_stop.");
        // If pre_start fails, we still must attempt cleanup.
        if let Err(post_err) = actor.post_stop().await {
            tracing::error!(error = %post_err, "post_stop also failed after pre_start failure.");
            return ActorStatus::PostStopFailed;
        }
        return ActorStatus::PreStartFailed;
    }

    tracing::debug!("pre_start successful. Running main logic.");

    // 2. Main Actor Logic
    let run_result = AssertUnwindSafe(actor.run(hi_rx, lo_rx))
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

    // 3. Post-stop Hook (guaranteed to run if pre_start succeeded)
    tracing::debug!("Attempting post_stop cleanup...");
    if let Err(err) = actor.post_stop().await {
        tracing::error!(error = %err, "post_stop failed.");
        // Failure in cleanup is a critical error that overrides any previous status.
        return ActorStatus::PostStopFailed;
    }

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
