use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::sync::mpsc;

use crate::{
    message::{GroupId, JoinError, JoinRequest, PeerId},
    peer::{PeerActor, PeerHandle},
};

#[derive(thiserror::Error, Debug)]
pub enum GroupError {
    #[error("group is unavailable")]
    GroupUnavailable,
}

#[derive(Debug)]
pub enum GroupMessage {
    Join(JoinRequest),
}

#[derive(Debug)]
pub struct GroupActor {
    handle: GroupHandle,
    peers: HashMap<Arc<PeerId>, PeerHandle>,
}

impl GroupActor {
    async fn run(mut self, mut receiver: mpsc::Receiver<GroupMessage>) {
        while let Some(msg) = receiver.recv().await {
            self.handle_message(msg);
        }
    }

    fn handle_message(&mut self, msg: GroupMessage) {
        match msg {
            GroupMessage::Join(req) => {
                match PeerActor::new(self.handle.clone(), req.peer_id.clone(), req.offer) {
                    Ok((peer, answer)) => {
                        let handle = PeerHandle::spawn(peer);
                        self.peers.insert(req.peer_id, handle);
                        let _ = req.reply.send(Ok(answer));
                    }
                    Err(err) => {
                        let _ = req.reply.send(Err(JoinError::Unknown(err.to_string())));
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct GroupHandle {
    pub group_id: Arc<GroupId>,
    sender: mpsc::Sender<GroupMessage>,
}

impl GroupHandle {
    pub fn spawn(group_id: Arc<GroupId>) -> Self {
        let (sender, receiver) = mpsc::channel(8);
        let handle = Self {
            sender,
            group_id: group_id.clone(),
        };
        let actor = GroupActor {
            handle: handle.clone(),
            peers: HashMap::new(),
        };
        tokio::spawn(actor.run(receiver));
        handle
    }

    pub fn join(&self, req: JoinRequest) {
        if let Err(err) = self.sender.try_send(GroupMessage::Join(req)) {
            tracing::warn!("failed to join a group: {:?}", err);
            // req.reply
            //     .send(Err(JoinError::Group(GroupError::GroupUnavailable)));
        }
    }
}
