use tokio::{sync::mpsc, task::JoinSet};

pub trait Actor: Send + 'static {
    type HighPriorityMessage: Send + 'static;
    type LowPriorityMessage: Send + 'static;
    type ActorId: AsRef<str>;

    fn id() -> Self::ActorId;

    fn run(
        &mut self,
        hi_rx: mpsc::Receiver<Self::HighPriorityMessage>,
        lo_rx: mpsc::Receiver<Self::LowPriorityMessage>,
    ) -> impl Future<Output = ()>;
}

pub trait ActorRef<A: Actor>: std::fmt::Debug + Clone {
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

pub enum RoomHighPriorityMessage {}

pub enum RoomLowPriorityMessage {}

pub struct Room {}

impl Actor for Room {
    type HighPriorityMessage = RoomHighPriorityMessage;
    type LowPriorityMessage = RoomLowPriorityMessage;
    type ActorId = &'static str;

    fn id() -> Self::ActorId {
        "room"
    }

    async fn run(
        &mut self,
        hi_rx: mpsc::Receiver<Self::HighPriorityMessage>,
        lo_rx: mpsc::Receiver<Self::LowPriorityMessage>,
    ) -> () {
    }
}

#[derive(Debug, Clone)]
pub struct BaseActorRef<A: Actor> {
    hi_tx: mpsc::Sender<A::HighPriorityMessage>,
    lo_tx: mpsc::Sender<A::LowPriorityMessage>,
}

impl<A: Actor> BaseActorRef<A> {
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
    spawner: JoinSet<A::ActorId>,
    mut actor: A,
    lo_cap: usize,
    hi_cap: usize,
) -> BaseActorRef<A> {
    let (lo_tx, lo_rx) = mpsc::channel::<A::LowPriorityMessage>(lo_cap);
    let (hi_tx, hi_rx) = mpsc::channel::<A::HighPriorityMessage>(hi_cap);

    // tokio::spawn(actor.run(lo_rx, hi_rx));

    BaseActorRef { lo_tx, hi_tx }
}
