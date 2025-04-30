use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt::Display,
    sync::Arc,
};

use str0m::media::{MediaAdded, MediaData};
use tokio::sync::mpsc::{self, error::SendError};

use crate::{
    controller::ControllerHandle,
    message::{GroupId, MediaKey, PeerId},
    peer::{PeerHandle, PeerMessage},
};

#[derive(Debug)]
pub enum GroupMessage {
    PublishMedia(MediaKey, MediaAdded),
    ForwardMedia(MediaKey, Arc<MediaData>),
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

    medias: HashMap<MediaKey, MediaAdded>,
    subscriptions: HashMap<MediaKey, BTreeSet<PeerHandle>>,
}

impl GroupActor {
    pub async fn run(mut self) {
        while let Some(msg) = self.receiver.recv().await {
            self.handle_message(msg).await;
        }
    }

    async fn handle_message(&mut self, mut msg: GroupMessage) {
        match msg {
            GroupMessage::AddPeer(peer) => {
                self.peers.insert(peer.peer_id.clone(), peer);
            }
            GroupMessage::RemovePeer(peer_id) => {
                self.peers.remove(&peer_id);
            }
            GroupMessage::PublishMedia(key, media) => {
                self.medias.insert(key, media);
                todo!("forward based on subscriptions");
            }
            GroupMessage::ForwardMedia(key, data) => {
                if let Some(interests) = self.subscriptions.get(&key) {
                    for interest in interests.iter() {
                        if let Err(res) = interest
                            .sender
                            .try_send(PeerMessage::ForwardMedia(key.clone(), data.clone()))
                        {
                            tracing::warn!("dropping a media data to {interest}");
                        }
                    }
                }
            }
            _ => todo!(),
        };
    }
}

#[derive(Clone)]
pub struct GroupHandle {
    pub sender: mpsc::Sender<GroupMessage>,
    pub group_id: Arc<GroupId>,
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
            medias: HashMap::new(),
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
