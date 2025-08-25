use std::{collections::HashMap, fmt::Display, ops::Deref, sync::Arc};

use tokio::{
    sync::mpsc::{self, error::SendError},
    task::JoinSet,
};
use tracing::Instrument;

use crate::{
    actor::{self, Actor, ActorError},
    entity::{ParticipantId, RoomId, TrackId},
    message::TrackMeta,
    participant::{ParticipantActor, ParticipantHandle},
    rng::Rng,
    track::TrackHandle,
};

#[derive(Debug)]
pub enum RoomMessage {
    PublishTrack(ParticipantHandle, Arc<TrackMeta>),
    AddParticipant(ParticipantHandle, ParticipantActor),
}

#[derive(Debug)]
pub struct ParticipantMeta {
    handle: ParticipantHandle,
    tracks: HashMap<Arc<TrackId>, TrackHandle>,
}

/// Reponsibilities:
/// * Manage Participant Lifecycle
/// * Manage Track Lifecycle
/// * Maintain Room State Registry: Keep an up-to-date list of current participants and available tracks
/// * Broadcast Room Events
/// * Mediate Subscriptions: Process subscription requests to tracks
/// * Own & Supervise Track Actors
pub struct RoomActor {
    rng: Rng,
    receiver: mpsc::Receiver<RoomMessage>,
    handle: RoomHandle,

    participants: HashMap<Arc<ParticipantId>, ParticipantMeta>,
    participant_tasks: JoinSet<Arc<ParticipantId>>,
    track_tasks: JoinSet<Arc<TrackId>>,
}

impl Actor for RoomActor {
    type ID = Arc<RoomId>;

    fn kind(&self) -> &'static str {
        "room"
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

                Some(Ok(track_id)) = self.track_tasks.join_next() => {
                    // TODO: handle track lifecycle
                }

                else => break,
            }
        }

        Ok(())
    }
}

impl RoomActor {
    async fn handle_message(&mut self, msg: RoomMessage) {
        match msg {
            RoomMessage::AddParticipant(participant_handle, participant_actor) => {
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
                for meta in self.participants.values() {
                    tracks.extend(meta.tracks.values().cloned());
                }
                tracing::info!(
                    "{} joined, adding tracks: {:?}",
                    participant_handle.participant_id,
                    self.participants,
                );
                let _ = participant_handle.add_tracks(Arc::new(tracks)).await;
            }
            RoomMessage::PublishTrack(participant_handle, track_meta) => {
                let Some(origin) = self.participants.get_mut(&track_meta.id.origin_participant)
                else {
                    tracing::warn!(
                        "{} is missing from participants, ignoring track",
                        track_meta.id
                    );
                    return;
                };

                let (handle, track_actor) =
                    TrackHandle::new(participant_handle.clone(), track_meta.clone());
                let track_id = track_meta.id.clone();
                tracing::info!(
                    "{} published a track, added: {}",
                    origin.handle.participant_id,
                    track_id
                );
                self.track_tasks.spawn(
                    async move {
                        actor::run(track_actor).await;
                        track_id
                    }
                    .in_current_span(),
                );
                origin.tracks.insert(track_meta.id.clone(), handle.clone());
                let new_tracks = Arc::new(vec![handle]);
                for participant in self.participants.values() {
                    let _ = participant.handle.add_tracks(new_tracks.clone()).await;
                    tracing::info!(
                        "current tracks: {} -> {:?}",
                        participant.handle.participant_id,
                        participant.tracks
                    );
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
pub struct RoomHandle {
    pub sender: mpsc::Sender<RoomMessage>,
    pub room_id: Arc<RoomId>,
}

impl RoomHandle {
    pub fn new(rng: Rng, room_id: Arc<RoomId>) -> (Self, RoomActor) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = RoomHandle {
            sender,
            room_id: room_id.clone(),
        };
        let actor = RoomActor {
            rng,
            receiver,
            handle: handle.clone(),
            participants: HashMap::new(),
            participant_tasks: JoinSet::new(),
            track_tasks: JoinSet::new(),
        };
        (handle, actor)
    }

    pub async fn add_participant(
        &self,
        handle: ParticipantHandle,
        actor: ParticipantActor,
    ) -> Result<(), SendError<RoomMessage>> {
        self.sender
            .send(RoomMessage::AddParticipant(handle, actor))
            .await
    }

    pub async fn publish(
        &self,
        participant_handle: ParticipantHandle,
        track_meta: Arc<TrackMeta>,
    ) -> Result<(), SendError<RoomMessage>> {
        self.sender
            .send(RoomMessage::PublishTrack(participant_handle, track_meta))
            .await
    }
}

impl Display for RoomHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.room_id.deref().as_ref())
    }
}
