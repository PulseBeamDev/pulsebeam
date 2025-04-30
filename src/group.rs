use std::{collections::HashMap, sync::Arc};

use tokio::{
    sync::mpsc::{self, error::SendError},
    task::JoinHandle,
};

use crate::{
    controller::ControllerHandle,
    message::{GroupId, PeerId},
    peer::PeerHandle,
};

pub enum GroupMessage {
    PublishTrack,
    UnpublishTrack,
    SubscribeTrack,
    UnsubscribeTrack,
    AddPeer(PeerHandle),
    RemovePeer(Arc<PeerId>),
}

pub struct GroupActor {
    controller: ControllerHandle,
    group_id: Arc<GroupId>,
    handle: GroupHandle,
    peers: HashMap<Arc<PeerId>, PeerHandle>,
}

impl GroupActor {
    async fn run(self, mut receiver: mpsc::Receiver<GroupMessage>) {
        while let Some(msg) = receiver.recv().await {
            self.handle_message(msg).await;
        }
    }

    async fn handle_message(&self, mut msg: GroupMessage) {
        match msg {
            _ => todo!(),
        }
    }
}

#[derive(Clone)]
pub struct GroupHandle {
    sender: mpsc::Sender<GroupMessage>,
    group_id: Arc<GroupId>,
}

impl GroupHandle {
    pub fn spawn(controller: ControllerHandle, group_id: Arc<GroupId>) -> (Self, JoinHandle<()>) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = GroupHandle {
            sender,
            group_id: group_id.clone(),
        };
        let actor = GroupActor {
            controller,
            group_id,
            handle: handle.clone(),
            peers: HashMap::new(),
        };
        let join = tokio::spawn(actor.run(receiver));
        (handle, join)
    }

    pub async fn add_peer(&self, peer: PeerHandle) -> Result<(), SendError<GroupMessage>> {
        self.sender.send(GroupMessage::AddPeer(peer)).await
    }
}
