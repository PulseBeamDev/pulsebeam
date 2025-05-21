use std::{collections::HashMap, fmt::Display, ops::Deref, sync::Arc};

use tokio::{
    sync::mpsc::{self, error::SendError},
    task::JoinSet,
};
use tracing::Instrument;

use crate::{
    actor::{self, Actor, ActorError},
    entity::{ParticipantId, RoomId, TrackId},
    participant::{ParticipantActor, ParticipantHandle},
    rng::Rng,
    track::TrackHandle,
};

#[derive(Debug)]
pub enum RouterMessage {
    PublishTrack(TrackHandle),
    AddParticipant(ParticipantHandle, ParticipantActor),
}

pub struct ParticipantMeta {
    handle: ParticipantHandle,
    tracks: HashMap<Arc<TrackId>, TrackHandle>,
}

pub struct RouterActor {
    rng: Rng,
    receiver: mpsc::Receiver<RouterMessage>,
    handle: RouterHandle,

    participants: HashMap<Arc<ParticipantId>, ParticipantMeta>,
    participant_tasks: JoinSet<Arc<ParticipantId>>,
}

impl Actor for RouterActor {
    type ID = Arc<RoomId>;

    fn kind(&self) -> &'static str {
        "router"
    }

    fn id(&self) -> Self::ID {
        self.handle.room_id.clone()
    }

    async fn run(&mut self) -> Result<(), ActorError> {
        loop {
            tokio::select! {
                res = self.receiver.recv() => {
                    match res {
                        Some(msg) => self.handle_message(msg).await,
                        None => break,
                    }
                }

                Some(Ok(participant_id)) = self.participant_tasks.join_next() => {
                    self.handle_participant_left(participant_id).await;
                }

                else => break,
            }
        }

        Ok(())
    }
}

impl RouterActor {
    async fn handle_message(&mut self, msg: RouterMessage) {
        match msg {
            RouterMessage::AddParticipant(participant_handle, participant_actor) => {
                let participant_id = participant_handle.participant_id.clone();
                self.participants.insert(
                    participant_handle.participant_id.clone(),
                    ParticipantMeta {
                        handle: participant_handle.clone(),
                        tracks: HashMap::new(),
                    },
                );
                self.participant_tasks.spawn(
                    async move {
                        actor::run(participant_actor).await;
                        participant_id
                    }
                    .in_current_span(),
                );

                let mut tracks = Vec::with_capacity(self.participants.len());
                for (_, meta) in &self.participants {
                    tracks.extend(meta.tracks.values().cloned());
                }
                let _ = participant_handle.add_tracks(Arc::new(tracks)).await;
            }
            RouterMessage::PublishTrack(track) => {
                let Some(origin) = self.participants.get_mut(&track.meta.id.origin_participant)
                else {
                    tracing::warn!(
                        "{} is missing from participants, ignoring track",
                        track.meta.id
                    );
                    return;
                };

                origin.tracks.insert(track.meta.id.clone(), track.clone());
                let new_tracks = Arc::new(vec![track]);
                for (_, participant) in &self.participants {
                    let _ = participant.handle.add_tracks(new_tracks.clone()).await;
                }
            }
        };
    }

    async fn handle_participant_left(&mut self, participant_id: Arc<ParticipantId>) {
        // TODO: notify participant leaving
        let Some(participant) = self.participants.remove(&participant_id) else {
            return;
        };

        let tracks: Vec<Arc<TrackId>> = participant
            .tracks
            .into_values()
            .map(|t| t.meta.id.clone())
            .collect();
        let tracks = Arc::new(tracks);
        for (_, participant) in &self.participants {
            let _ = participant.handle.remove_tracks(tracks.clone()).await;
        }
    }
}

#[derive(Clone)]
pub struct RouterHandle {
    pub sender: mpsc::Sender<RouterMessage>,
    pub room_id: Arc<RoomId>,
}

impl RouterHandle {
    pub fn new(rng: Rng, room_id: Arc<RoomId>) -> (Self, RouterActor) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = RouterHandle {
            sender,
            room_id: room_id.clone(),
        };
        let actor = RouterActor {
            rng,
            receiver,
            handle: handle.clone(),
            participants: HashMap::new(),
            participant_tasks: JoinSet::new(),
        };
        (handle, actor)
    }

    pub async fn add_participant(
        &self,
        handle: ParticipantHandle,
        actor: ParticipantActor,
    ) -> Result<(), SendError<RouterMessage>> {
        self.sender
            .send(RouterMessage::AddParticipant(handle, actor))
            .await
    }

    pub async fn publish(&self, track: TrackHandle) -> Result<(), SendError<RouterMessage>> {
        self.sender.send(RouterMessage::PublishTrack(track)).await
    }
}

impl Display for RouterHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.room_id.deref().as_ref())
    }
}
