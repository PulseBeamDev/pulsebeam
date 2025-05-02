use std::{collections::HashMap, fmt::Display, panic::AssertUnwindSafe, sync::Arc};

use futures::FutureExt;
use str0m::media::Mid;
use tokio::{
    sync::mpsc::{self, error::SendError},
    task::JoinSet,
};

use crate::{
    controller::ControllerHandle,
    message::{GroupId, PeerId, TrackIn},
    peer::PeerHandle,
    track::TrackHandle,
};

#[derive(Debug)]
pub enum GroupMessage {
    PublishMedia(PeerHandle, TrackIn),
    AddPeer(PeerHandle),
    RemovePeer(Arc<PeerId>),
}

#[derive(Hash, PartialEq, Eq)]
struct TrackKey {
    origin: Arc<PeerId>,
    mid: Mid,
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
    handle: GroupHandle,

    peers: HashMap<Arc<PeerId>, PeerHandle>,
    tracks: HashMap<TrackKey, TrackHandle>,

    track_tasks: JoinSet<TrackKey>,
}

impl GroupActor {
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                biased;

                res = self.receiver.recv() => {
                    match res {
                        Some(msg) => self.handle_message(msg).await,
                        None => break,
                    }
                }

                Some(Ok(key)) = self.track_tasks.join_next() => {
                    // track actor exited
                    self.tracks.remove(&key);
                }
            }
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
            GroupMessage::PublishMedia(origin, track) => {
                let key = TrackKey {
                    origin: origin.peer_id.clone(),
                    mid: track.mid,
                };

                if let Some(_) = self.tracks.get(&key) {
                    tracing::warn!(
                        "Detected an update to an existing track. This is ignored for now."
                    );
                } else {
                    let track = Arc::new(track);
                    let (handle, actor) = TrackHandle::new(origin, track.clone());
                    self.track_tasks.spawn(async move {
                        match AssertUnwindSafe(actor.run()).catch_unwind().await {
                            Ok(Ok(())) => {
                                tracing::info!(?track, "track actor exited.");
                            }
                            Ok(Err(err)) => {
                                tracing::warn!(?track, "track actor exited with an error: {err}");
                            }
                            Err(err) => {
                                tracing::error!(?track, "track actor panicked: {:?}", err);
                            }
                        };
                        key
                    });
                    self.tracks.insert(key, handle);
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
            handle: handle.clone(),
            peers: HashMap::new(),
            tracks: HashMap::new(),
            track_tasks: JoinSet::new(),
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
