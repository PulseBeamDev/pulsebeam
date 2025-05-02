use std::{
    collections::{BTreeSet, HashMap},
    fmt::Display,
    sync::Arc,
};

use str0m::media::{MediaAdded, MediaData, MediaKind, Mid};
use tokio::sync::mpsc::{self, error::SendError};

use crate::{
    controller::ControllerHandle,
    message::{GroupId, PeerId},
    peer::{PeerHandle, PeerMessage},
};

#[derive(Hash, PartialEq, Eq, Debug, Clone)]
pub struct TrackIn {
    pub peer_id: Arc<PeerId>,
    pub mid: Mid,
    pub kind: MediaKind,
}

#[derive(Debug)]
pub enum GroupMessage {
    PublishMedia(Arc<PeerId>, Arc<TrackIn>),
    UnpublishMedia(MediaKey),
    ForwardMedia(MediaKey, Arc<MediaData>),
    AddPeer(PeerHandle),
    RemovePeer(Arc<PeerId>),
}

/// Reponsibilities:
/// * Manage Participant Lifecycle
/// * Manage Track Lifecycle
/// * Maintain Group State Registry: Keep an up-to-date list of current participants and available tracks
/// * Broadcast Group Events
/// * Mediate Subscriptions: Process subscription requests to tracks
/// * Own & Supervise Track Actors
pub struct GroupActor {
    receiver: mpsc::Receiver<GroupMessage>,
    controller: ControllerHandle,
    group_id: Arc<GroupId>,
    handle: GroupHandle,
    peers: HashMap<Arc<PeerId>, PeerHandle>,

    medias: HashMap<MediaKey, Arc<MediaAdded>>,
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
                // TODO: clean up subscriptions and published medias
            }
            GroupMessage::PublishMedia(key, media) => {
                self.medias.insert(key.clone(), media.clone());
                for (_, peer) in self.peers.iter() {
                    if peer.peer_id == key.peer_id {
                        continue;
                    }
                    peer.sender
                        .send(PeerMessage::PublishMedia(key.clone(), media.clone()))
                        .await;
                }
            }
            GroupMessage::SubscribeMedia(key, peer) => {
                self.subscriptions.entry(key).or_default().insert(peer);
            }
            GroupMessage::UnsubscribeMedia(key, peer) => {
                if let Some(e) = self.subscriptions.get_mut(&key) {
                    e.remove(&peer);

                    if e.is_empty() {
                        self.subscriptions.remove(&key);
                    }
                }
            }
            GroupMessage::ForwardMedia(key, data) => {
                // TODO: separate control vs data loop
                if let Some(interests) = self.subscriptions.get(&key) {
                    for interest in interests.iter() {
                        if let Err(err) = interest
                            .sender
                            .try_send(PeerMessage::ForwardMedia(key.clone(), data.clone()))
                        {
                            tracing::warn!("dropping a media data to {interest}: {err}");
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
            subscriptions: HashMap::new(),
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
