use tokio::sync::mpsc::{self, error::TrySendError};

#[derive(Debug)]
pub enum IngressMessage {
    Ping,
}

pub struct IngressActor {}

impl IngressActor {
    fn new() -> Self {
        Self {}
    }

    fn handle_message(&mut self, msg: IngressMessage) {
        tracing::info!("received: {:?}", msg);
    }

    async fn run(mut self, mut receiver: mpsc::Receiver<IngressMessage>) {
        while let Some(msg) = receiver.recv().await {
            self.handle_message(msg);
        }
    }
}

#[derive(Clone)]
pub struct IngressHandle {
    sender: mpsc::Sender<IngressMessage>,
}

impl IngressHandle {
    pub fn spawn() -> Self {
        let (sender, receiver) = mpsc::channel(8);
        let actor = IngressActor::new();
        tokio::spawn(actor.run(receiver));
        Self { sender }
    }

    pub fn ping(&self) -> Result<(), TrySendError<IngressMessage>> {
        self.sender.try_send(IngressMessage::Ping)
    }
}
