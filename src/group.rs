use tokio::sync::mpsc::{self, error::TrySendError};

#[derive(thiserror::Error, Debug)]
pub enum GroupError {}

#[derive(Debug)]
pub enum GroupMessage {}

#[derive(Debug)]
pub struct GroupActor {}

impl GroupActor {
    pub fn new() -> Result<Self, GroupError> {
        Ok(Self {})
    }

    async fn run(self, mut receiver: mpsc::Receiver<GroupMessage>) {
        while let Some(msg) = receiver.recv().await {}
    }
}

#[derive(Clone)]
pub struct PeerHandle {
    sender: mpsc::Sender<GroupMessage>,
}

impl PeerHandle {
    pub fn spawn(actor: GroupActor) -> Self {
        let (sender, receiver) = mpsc::channel(8);
        tokio::spawn(actor.run(receiver));
        Self { sender }
    }
}
