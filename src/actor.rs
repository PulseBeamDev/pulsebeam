use futures::FutureExt;
use std::any::Any;
use std::fmt;
use std::fmt::{Debug, Display, Formatter};
use std::panic::{AssertUnwindSafe, RefUnwindSafe, UnwindSafe};
use thiserror::Error;
use tracing;

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
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
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

pub trait Actor: Send + Sized {
    // Renamed trait
    fn kind(&self) -> &'static str;

    type ID: Display + Debug + Clone + Send + Sync + UnwindSafe + RefUnwindSafe + 'static;
    fn id(&self) -> Self::ID;

    /// Called once before the main `run` loop starts.
    fn pre_start(&mut self) -> impl Future<Output = Result<(), ActorError>> {
        async { Ok(()) }
    }

    /// The main logic of the actor.
    fn run(&mut self) -> impl Future<Output = Result<(), ActorError>>;

    /// Called once after `run` completes (Ok or Err),
    /// OR if `pre_start` fails, OR if `run_actor_logic` panics.
    /// This function MUST be panic-safe if it's to be called after a panic.
    fn post_stop(&mut self) -> impl Future<Output = Result<(), ActorError>> {
        async { Ok(()) }
    }
}

// --- Actor Runner ---

pub async fn run<A>(actor: A)
where
    A: Actor + Send + 'static,
{
    let actor_kind = actor.kind();
    let actor_id = actor.id();

    run_instrumented(actor, actor_kind, actor_id).await;
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
async fn run_instrumented<A>(mut actor: A, actor_kind: &'static str, actor_id: A::ID)
where
    A: Actor + Send + 'static,
{
    let span = tracing::Span::current();
    tracing::info!("Starting actor...");

    let mut current_status = ActorStatus::Starting;
    span.record("status", &tracing::field::display(current_status));

    // 1. Pre-start Hook
    match actor.pre_start().await {
        Ok(()) => {
            tracing::info!("pre_start successful.");
            current_status = ActorStatus::Running;
        }
        Err(err) => {
            tracing::error!(error = %err, "pre_start failed. Proceeding to post_stop then shutting down.");
            current_status = ActorStatus::PreStartFailed;
            // Fall through to post_stop
        }
    }
    span.record("status", &tracing::field::display(current_status));

    // 2. Run Main Actor Logic (only if pre_start was OK)
    if current_status == ActorStatus::Running {
        let future_to_run = actor.run();
        // AssertUnwindSafe is used because the future produced by actor.run_actor_logic()
        // might not be UnwindSafe by default if `A` (the actor type) isn't UnwindSafe.
        // We are catching the panic from this specific unit of work.
        let run_result = AssertUnwindSafe(future_to_run).catch_unwind().await;

        match run_result {
            Ok(Ok(())) => {
                tracing::info!("Exited gracefully from run_actor_logic.");
                current_status = ActorStatus::ExitedGracefully;
            }
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "Exited with an error from run_actor_logic.");
                current_status = ActorStatus::ExitedWithError;
            }
            Err(panic_payload) => {
                // panic_payload is Box<dyn Any + Send>
                let panic_msg = extract_panic_message(&panic_payload);
                tracing::error!(panic.message = %panic_msg, "PANICKED!");
                current_status = ActorStatus::Panicked;
                // No separate on_panic hook, post_stop is expected to handle cleanup.
                tracing::warn!(
                    "Attempting post_stop after panic. Ensure post_stop is panic-safe and handles potential inconsistent state!"
                );
            }
        };
        span.record("status", &tracing::field::display(current_status));
    }

    // 3. Unified Post-stop Hook
    // Called if pre_start failed OR after run_actor_logic (regardless of its outcome, including panic)
    tracing::info!("Attempting post_stop...");
    match actor.post_stop().await {
        Ok(()) => {
            tracing::info!("post_stop successful.");
            // Determine final status based on previous state
            match current_status {
                ActorStatus::PreStartFailed
                | ActorStatus::Panicked
                | ActorStatus::ExitedWithError
                | ActorStatus::PostStopFailed => {
                    // Do not override these more severe statuses with a simple "shut_down"
                    // The current_status already reflects the primary failure.
                }
                ActorStatus::ExitedGracefully | ActorStatus::Running | ActorStatus::Starting => {
                    // If pre_start failed, current_status is PreStartFailed, and we don't change it here.
                    // If everything was fine up to this point, or just run_actor_logic finished, now it's fully shut down.
                    if current_status != ActorStatus::PreStartFailed {
                        current_status = ActorStatus::ShutDown;
                    }
                }
                ActorStatus::ShutDown => {} // Already shut down, no change
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "post_stop failed.");
            // If post_stop fails, that's a significant failure.
            // Only update if not already in a more critical state like Panicked or PreStartFailed.
            if current_status != ActorStatus::Panicked
                && current_status != ActorStatus::PreStartFailed
            {
                current_status = ActorStatus::PostStopFailed;
            }
        }
    }

    span.record("status", &tracing::field::display(current_status));
    tracing::info!("Fully shut down with final status: {}", current_status);
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
