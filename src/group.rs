use std::{collections::HashMap, fmt::Display, sync::Arc};

use tokio::sync::mpsc::{self, error::SendError};

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
    receiver: mpsc::Receiver<GroupMessage>,
    controller: ControllerHandle,
    group_id: Arc<GroupId>,
    handle: GroupHandle,
    peers: HashMap<Arc<PeerId>, PeerHandle>,
}

impl GroupActor {
    pub async fn run(mut self) {
        while let Some(msg) = self.receiver.recv().await {
            self.handle_message(msg).await;
        }
    }

    async fn handle_message(&mut self, mut msg: GroupMessage) {
        match msg {
            GroupMessage::AddPeer(peer) => self.peers.insert(peer.peer_id.clone(), peer),
            GroupMessage::RemovePeer(peer_id) => self.peers.remove(&peer_id),
            _ => todo!(),
        };
    }
}

#[derive(Clone)]
pub struct GroupHandle {
    sender: mpsc::Sender<GroupMessage>,
    group_id: Arc<GroupId>,
}

impl GroupHandle {
    pub fn new(controller: ControllerHandle, group_id: Arc<GroupId>) -> (Self, GroupActor) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = GroupHandle {
            sender,
            group_id: group_id.clone(),
        };
        let actor = GroupActor {
            receiver,
            controller,
            group_id,
            handle: handle.clone(),
            peers: HashMap::new(),
        };
        (handle, actor)
    }

    pub async fn add_peer(&self, peer: PeerHandle) -> Result<(), SendError<GroupMessage>> {
        self.sender.send(GroupMessage::AddPeer(peer)).await
    }

    pub async fn remove_peer(&self, peer: PeerHandle) -> Result<(), SendError<GroupMessage>> {
        self.sender.send(GroupMessage::AddPeer(peer)).await
    }
}

impl Display for GroupHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.group_id.as_str())
    }
}
