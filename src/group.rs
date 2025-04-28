use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::sync::mpsc;

use crate::{
    egress::EgressHandle,
    ingress::IngressHandle,
    message::{GroupId, JoinError, JoinRequest, PeerId},
    peer::{PeerActor, PeerHandle},
};

#[derive(thiserror::Error, Debug)]
pub enum GroupError {
    #[error("group is unavailable")]
    Unavailable,

    #[error("peer id already exists")]
    AlreadyExists,
}

#[derive(Debug)]
pub enum GroupMessage {
    Join(JoinRequest),
}

#[derive(Debug)]
pub struct GroupActor {
    ingress: IngressHandle,
    egress: EgressHandle,
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
                if self.peers.contains_key(&req.peer_id) {
                    let _ = req
                        .reply
                        .send(Err(JoinError::Group(GroupError::Unavailable)));
                    return;
                }

                PeerHandle::spawn(
                    self.ingress.clone(),
                    self.egress.clone(),
                    req.group_id,
                    req.peer_id,
                    req.rtc,
                );
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
    pub fn spawn(ingress: IngressHandle, egress: EgressHandle, group_id: Arc<GroupId>) -> Self {
        let (sender, receiver) = mpsc::channel(8);
        let handle = Self {
            sender,
            group_id: group_id.clone(),
        };
        let actor = GroupActor {
            ingress,
            egress,
            handle: handle.clone(),
            peers: HashMap::new(),
        };
        tokio::spawn(actor.run(receiver));
        handle
    }

    pub fn join(&self, req: JoinRequest) {
        if let Err(err) = self.sender.try_send(GroupMessage::Join(req)) {
            tracing::warn!("failed to join a group: {:?}", err);
        }
    }
}
