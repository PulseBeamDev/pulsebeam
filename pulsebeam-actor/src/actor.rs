use tokio::{sync::mpsc, task::JoinSet};

pub trait Actor: Send + 'static {
    type HighPriorityMessage: Send + 'static;
    type LowPriorityMessage: Send + 'static;
    type ActorId: std::fmt::Debug
        + std::fmt::Display
        + Clone
        + Eq
        + std::hash::Hash
        + Send
        + Sync
        + 'static;

    fn id(&self) -> Self::ActorId;

    fn run(
        &mut self,
        hi_rx: mpsc::Receiver<Self::HighPriorityMessage>,
        lo_rx: mpsc::Receiver<Self::LowPriorityMessage>,
    ) -> impl Future<Output = ()> + Send + Sync;
}

pub trait ActorHandle<A: Actor>: std::fmt::Debug + Clone {
    fn lo_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> impl Future<Output = Result<(), mpsc::error::SendError<A::LowPriorityMessage>>>;

    fn lo_try_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mpsc::error::TrySendError<A::LowPriorityMessage>>;

    fn hi_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> impl Future<Output = Result<(), mpsc::error::SendError<A::HighPriorityMessage>>>;

    fn hi_try_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mpsc::error::TrySendError<A::HighPriorityMessage>>;
}

#[derive(Debug, Clone)]
pub struct BaseActorHandle<A: Actor> {
    hi_tx: mpsc::Sender<A::HighPriorityMessage>,
    lo_tx: mpsc::Sender<A::LowPriorityMessage>,
}

impl<A: Actor> BaseActorHandle<A> {
    pub async fn lo_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mpsc::error::SendError<A::LowPriorityMessage>> {
        self.lo_tx.send(message).await
    }

    pub fn lo_try_send(
        &self,
        message: A::LowPriorityMessage,
    ) -> Result<(), mpsc::error::TrySendError<A::LowPriorityMessage>> {
        self.lo_tx.try_send(message)
    }

    pub async fn hi_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mpsc::error::SendError<A::HighPriorityMessage>> {
        self.hi_tx.send(message).await
    }

    pub fn hi_try_send(
        &self,
        message: A::HighPriorityMessage,
    ) -> Result<(), mpsc::error::TrySendError<A::HighPriorityMessage>> {
        self.hi_tx.try_send(message)
    }
}

pub fn spawn<A: Actor>(
    mut spawner: JoinSet<A::ActorId>,
    mut actor: A,
    lo_cap: usize,
    hi_cap: usize,
) -> BaseActorHandle<A> {
    let (lo_tx, lo_rx) = mpsc::channel::<A::LowPriorityMessage>(lo_cap);
    let (hi_tx, hi_rx) = mpsc::channel::<A::HighPriorityMessage>(hi_cap);
    let actor_id = actor.id();

    spawner.spawn(async move {
        actor.run(hi_rx, lo_rx).await;
        actor_id
    });
    BaseActorHandle { lo_tx, hi_tx }
}
