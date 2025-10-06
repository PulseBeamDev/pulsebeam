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

pub struct JoinHandle<M: MessageSet> {
    inner: Pin<Box<dyn futures::Future<Output = (M::Meta, ActorStatus)> + Send>>,
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

impl<M: MessageSet> futures::Future for JoinHandle<M> {
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
    pub hi_rx: mailbox::Receiver<M::HighPriorityMsg>,
    pub lo_rx: mailbox::Receiver<M::LowPriorityMsg>,
    pub handle: ActorHandle<M>,
}

pub trait MessageSet: Sized + Send + 'static {
    type HighPriorityMsg: Send + 'static;
    type LowPriorityMsg: Send + 'static;
    type Meta: Eq + Hash + Display + Debug + Clone + Send;
    type ObservableState: Debug + Send + Clone;
}

pub trait Actor<M: MessageSet>: Sized + Send + 'static {
    fn meta(&self) -> M::Meta;
    fn get_observable_state(&self) -> M::ObservableState;

    fn run(
        &mut self,
        ctx: &mut ActorContext<M>,
    ) -> impl futures::Future<Output = Result<(), ActorError>> + Send {
        async {
            actor_loop!(self, ctx);
            Ok(())
        }
    }

    fn on_system(
        &mut self,
        _ctx: &mut ActorContext<M>,
        _msg: SystemMsg<M::ObservableState>,
    ) -> impl futures::Future<Output = ()> + Send {
        async move {}
    }

    fn on_high_priority(
        &mut self,
        _ctx: &mut ActorContext<M>,
        _msg: M::HighPriorityMsg,
    ) -> impl futures::Future<Output = ()> + Send {
        async move {}
    }

    fn on_low_priority(
        &mut self,
        _ctx: &mut ActorContext<M>,
        _msg: M::LowPriorityMsg,
    ) -> impl futures::Future<Output = ()> + Send {
        async move {}
    }
}

pub struct ActorHandle<M: MessageSet> {
    pub sys_tx: mailbox::Sender<SystemMsg<M::ObservableState>>,
    pub hi_tx: mailbox::Sender<M::HighPriorityMsg>,
    pub lo_tx: mailbox::Sender<M::LowPriorityMsg>,
    pub meta: M::Meta,
}

impl<M: MessageSet> Clone for ActorHandle<M> {
    fn clone(&self) -> Self {
        Self {
            sys_tx: self.sys_tx.clone(),
            hi_tx: self.hi_tx.clone(),
            lo_tx: self.lo_tx.clone(),
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
    pub async fn send_high(
        &mut self,
        msg: M::HighPriorityMsg,
    ) -> Result<(), mailbox::SendError<M::HighPriorityMsg>> {
        self.hi_tx.send(msg).await
    }

    pub fn try_send_high(
        &mut self,
        msg: M::HighPriorityMsg,
    ) -> Result<(), mailbox::TrySendError<M::HighPriorityMsg>> {
        self.hi_tx.try_send(msg)
    }

    pub async fn send_low(
        &mut self,
        msg: M::LowPriorityMsg,
    ) -> Result<(), mailbox::SendError<M::LowPriorityMsg>> {
        self.lo_tx.send(msg).await
    }

    pub fn try_send_low(
        &mut self,
        msg: M::LowPriorityMsg,
    ) -> Result<(), mailbox::TrySendError<M::LowPriorityMsg>> {
        self.lo_tx.try_send(msg)
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
    pub lo_cap: usize,
    pub hi_cap: usize,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            lo_cap: 1,
            hi_cap: 128,
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

pub fn spawn<A, M>(a: A, config: RunnerConfig) -> (ActorHandle<M>, JoinHandle<M>)
where
    M: MessageSet,
    A: Actor<M>,
{
    let (lo_tx, lo_rx) = mailbox::new(config.lo_cap);
    let (hi_tx, hi_rx) = mailbox::new(config.hi_cap);
    let (sys_tx, sys_rx) = mailbox::new(1);

    let handle = ActorHandle::<M> {
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

    let fut = run(a, ctx).instrument(tracing::span!(tracing::Level::INFO, "run", %actor_id));
    let join = tokio::task::Builder::new()
        .name(&actor_id.clone().to_string())
        // .spawn(tokio::task::unconstrained(fut))
        .spawn(fut)
        .unwrap();
    let abort_handle = join.abort_handle();

    let join = join
        .map(|res| match res {
            Ok(ret) => (actor_id, ret),
            Err(_) => (actor_id, ActorStatus::ShutDown),
        })
        .boxed();

    (
        handle,
        JoinHandle {
            inner: join,
            abort_handle,
        },
    )
}

pub fn spawn_default<A, M>(a: A) -> (ActorHandle<M>, JoinHandle<M>)
where
    M: MessageSet,
    A: Actor<M>,
{
    spawn(a, RunnerConfig::default())
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
        Ok(Err(_)) => ActorStatus::ExitedWithError,
        Err(p) => {
            tracing::error!("Actor panicked: {:?}", extract_panic_message(&p));
            ActorStatus::Panicked
        }
    };

    tracing::info!(status = %status, "Actor exited");
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
        // const TIME_SLICE_US: u64 = 100; // 100μs time slice
        // let mut last_yield = tokio::time::Instant::now();
        loop {
            // // Force yield if time slice expired
            // if last_yield.elapsed().as_micros() as u64 >= TIME_SLICE_US {
            //     tokio::task::yield_now().await;
            //     last_yield = tokio::time::Instant::now();
            // }

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
                res = $ctx.hi_rx.recv() => { if let Some(msg) = res { $actor.on_high_priority($ctx, msg).await } else { break; } }
                res = $ctx.lo_rx.recv() => { if let Some(msg) = res { $actor.on_low_priority($ctx, msg).await } else { break; } }
                $($extra)*
                else => break,
            }
        }
    };
}
