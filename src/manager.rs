use std::{collections::HashMap, sync::Arc};

use tokio::sync::mpsc;

use crate::{
    egress::EgressHandle,
    group::GroupHandle,
    ingress::IngressHandle,
    message::{GroupId, JoinRequest},
};

#[derive(thiserror::Error, Debug)]
pub enum ManagerError {
    #[error("server is busy, please try again later.")]
    ServiceUnavailable,

    #[error("unknown error: {0}")]
    Unknown(String),
}

#[derive(Debug)]
pub enum ManagerMessage {
    Join(JoinRequest),
}

pub struct ManagerActor {
    pub ingress: IngressHandle,
    pub egress: EgressHandle,
    pub groups: HashMap<Arc<GroupId>, GroupHandle>,
}

impl ManagerActor {
    async fn run(mut self, mut receiver: mpsc::Receiver<ManagerMessage>) {
        while let Some(msg) = receiver.recv().await {
            self.handle_message(msg);
        }
    }

    fn handle_message(&mut self, msg: ManagerMessage) {
        match msg {
            ManagerMessage::Join(req) => {
                if let Some(group) = self.groups.get(&req.group_id) {
                    group.join(req);
                } else {
                    let handle = GroupHandle::spawn(
                        self.ingress.clone(),
                        self.egress.clone(),
                        req.group_id.clone(),
                    );
                    self.groups.insert(req.group_id.clone(), handle.clone());
                    handle.join(req);
                };
            }
        }
    }
}

#[derive(Clone)]
pub struct ManagerHandle {
    sender: mpsc::Sender<ManagerMessage>,
}

impl ManagerHandle {
    pub fn spawn(actor: ManagerActor) -> Self {
        let (sender, receiver) = mpsc::channel(1);
        let handle = Self { sender };
        tokio::spawn(actor.run(receiver));
        handle
    }

    pub fn join(&self, req: JoinRequest) -> Result<(), ManagerError> {
        self.sender
            .try_send(ManagerMessage::Join(req))
            .map_err(|_| ManagerError::ServiceUnavailable)
    }
}
