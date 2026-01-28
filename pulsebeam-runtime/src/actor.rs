use crate::actor_loop;
use futures_lite::FutureExt;
use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::hash::Hash;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;
use tokio_metrics::TaskMonitor;

use crate::mailbox;

pub type ActorKind = &'static str;

#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub tags: HashMap<ActorKind, String>,
}

tokio::task_local! {
    static CURRENT_SCOPE: RefCell<Scope>;
}

pub struct JoinHandle<M: MessageSet> {
    inner: Pin<Box<dyn Future<Output = (M::Meta, ActorStatus)> + Send>>,
    abort_handle: tokio::task::AbortHandle,
}

impl<M: MessageSet> JoinHandle<M> {
    pub fn abort(&self) {
        self.abort_handle.abort();
    }
}

impl<M: MessageSet> Drop for JoinHandle<M> {
    fn drop(&mut self) {
        self.abort();
    }
}

impl<M: MessageSet> Future for JoinHandle<M> {
    type Output = (M::Meta, ActorStatus);

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

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
    ExitedGracefully,
    ExitedWithError,
    Panicked,
    ShutDown,
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

pub struct ActorContext<M: MessageSet> {
    pub sys_rx: mailbox::Receiver<SystemMsg<M::ObservableState>>,
    pub rx: mailbox::Receiver<M::Msg>,
    pub handle: ActorHandle<M>,
}

pub trait MessageSet: Sized + Send + 'static {
    type Msg: Send + 'static;
    type Meta: Eq + Hash + Display + Debug + Clone + Send;
    type ObservableState: Debug + Send + Clone;
}

pub trait Actor<M: MessageSet>: Sized + Send + 'static {
    fn meta(&self) -> M::Meta;
    fn get_observable_state(&self) -> M::ObservableState;

    fn kind() -> ActorKind;
    fn monitor() -> Arc<TaskMonitor>;

    fn run(
        &mut self,
        ctx: &mut ActorContext<M>,
    ) -> impl Future<Output = Result<(), ActorError>> + Send {
        async {
            actor_loop!(self, ctx);
            Ok(())
        }
    }

    fn on_system(
        &mut self,
        _ctx: &mut ActorContext<M>,
        _msg: SystemMsg<M::ObservableState>,
    ) -> impl Future<Output = ()> + Send {
        async move {}
    }

    fn on_msg(
        &mut self,
        _ctx: &mut ActorContext<M>,
        _msg: M::Msg,
    ) -> impl Future<Output = ()> + Send {
        async move {}
    }
}

pub struct ActorHandle<M: MessageSet> {
    pub sys_tx: mailbox::Sender<SystemMsg<M::ObservableState>>,
    pub tx: mailbox::Sender<M::Msg>,
    pub meta: M::Meta,
}

impl<M: MessageSet> Clone for ActorHandle<M> {
    fn clone(&self) -> Self {
        Self {
            sys_tx: self.sys_tx.clone(),
            tx: self.tx.clone(),
            meta: self.meta.clone(),
        }
    }
}

impl<M: MessageSet> Debug for ActorHandle<M> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.meta, f)
    }
}

impl<M: MessageSet> ActorHandle<M> {
    pub async fn send(&mut self, msg: impl Into<M::Msg>) -> Result<(), mailbox::SendError<M::Msg>> {
        self.tx.send(msg.into()).await
    }

    pub fn try_send(
        &mut self,
        msg: impl Into<M::Msg>,
    ) -> Result<(), mailbox::TrySendError<M::Msg>> {
        self.tx.try_send(msg.into())
    }

    pub async fn get_state(&mut self) -> Result<M::ObservableState, ActorError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sys_tx
            .send(SystemMsg::GetState(tx))
            .await
            .map_err(|_| ActorError::SystemError)?;
        rx.await.map_err(|_| ActorError::SystemError)
    }

    pub async fn terminate(&mut self) -> Result<(), ActorError> {
        self.sys_tx
            .send(SystemMsg::Terminate)
            .await
            .map_err(|_| ActorError::SystemError)
    }
}

pub struct RunnerConfig {
    pub mailbox_cap: usize,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self { mailbox_cap: 128 }
    }
}

impl RunnerConfig {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_mailbox_cap(mut self, cap: usize) -> Self {
        self.mailbox_cap = cap;
        self
    }
}

pub fn prepare<A, M>(
    a: A,
    config: RunnerConfig,
) -> (ActorHandle<M>, impl Future<Output = (M::Meta, ActorStatus)>)
where
    M: MessageSet,
    A: Actor<M>,
{
    let (tx, rx) = mailbox::new(config.mailbox_cap);
    let (sys_tx, sys_rx) = mailbox::new(1);

    let handle = ActorHandle::<M> {
        tx,
        sys_tx,
        meta: a.meta(),
    };

    let ctx = ActorContext {
        rx,
        sys_rx,
        handle: handle.clone(),
    };

    // Capture current scope for nesting
    let mut active_scope = CURRENT_SCOPE
        .try_with(|s| s.borrow().clone())
        .unwrap_or_default();
    active_scope.tags.insert(A::kind(), a.meta().to_string());

    let parent_span = tracing::Span::current();
    let span = tracing::info_span!(
        parent: None,
        "actor",
        ctx = ?active_scope.tags
    );
    span.follows_from(parent_span);

    let scope_for_child = active_scope.clone();

    // Wrap the actor's run in instrumentation and unwind safety
    let runnable = CURRENT_SCOPE.scope(RefCell::new(scope_for_child), run(a, ctx));
    // let runnable = A::monitor().instrument(runnable);
    let runnable = tracing::Instrument::instrument(runnable, span);

    let actor_id = handle.meta.clone();
    let runnable = async move {
        let status = runnable.await;
        (actor_id, status)
    };

    (handle, runnable)
}

async fn run<A, M>(mut a: A, mut ctx: ActorContext<M>) -> ActorStatus
where
    M: MessageSet,
    A: Actor<M>,
{
    tracing::debug!("Starting actor...");
    let run_result = AssertUnwindSafe(a.run(&mut ctx)).catch_unwind().await;

    let status = match run_result {
        Ok(Ok(())) => ActorStatus::ExitedGracefully,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "Actor exited with logic error");
            ActorStatus::ExitedWithError
        }
        Err(p) => {
            let panic_msg = extract_panic_message(&p);
            tracing::error!(panic.message = %panic_msg, "Actor panicked");
            ActorStatus::Panicked
        }
    };

    tracing::info!(final_status = ?status, "Actor exited");
    status
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
    ($actor:ident, $ctx:ident) => { actor_loop!($actor, $ctx, pre_select: {}, select: {}) };
    ($actor:ident, $ctx:ident, pre_select: { $($pre_select:tt)* }) => { actor_loop!($actor, $ctx, pre_select: { $($pre_select)* }, select: {}) };
    ($actor:ident, $ctx:ident, pre_select: { $($pre_select:tt)* }, select: { $($extra:tt)* }) => {
        // let mut last_yield = tokio::time::Instant::now();
        loop {
            $($pre_select)*
            tokio::select! {
                biased;
                res = $ctx.sys_rx.recv() => {
                    match res {
                        Some(msg) => match msg {
                            $crate::actor::SystemMsg::GetState(responder) => {
                                let _ = responder.send($actor.get_observable_state());
                            }
                            $crate::actor::SystemMsg::Terminate => break,
                        },
                        None => break,
                    }
                }
                res = $ctx.rx.recv() => { if let Some(msg) = res { $actor.on_msg($ctx, msg).await } else { break; } }
                $($extra)*
                else => break,
            }

            // if last_yield.elapsed() > std::time::Duration::from_micros(50) {
            //     tokio::task::yield_now().await;
            //     last_yield = tokio::time::Instant::now();
            // }
        }
    };
}
