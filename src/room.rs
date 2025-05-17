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
pub enum RoomMessage {
    PublishMedia(TrackHandle),
    AddParticipant(ParticipantHandle, ParticipantActor),
}

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
                    // TODO: notify participant leaving
                    self.participants.remove(&participant_id);
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

                for (_, meta) in &self.participants {
                    for (_, track) in &meta.tracks {
                        let _ = participant_handle.new_track(track.clone()).await;
                    }
                }
            }
            RoomMessage::PublishMedia(track) => {
                for (_, participant) in &self.participants {
                    let _ = participant.handle.new_track(track.clone()).await;
                }
            }
            _ => todo!(),
        };
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

    pub async fn publish(&self, track: TrackHandle) -> Result<(), SendError<RoomMessage>> {
        self.sender.send(RoomMessage::PublishMedia(track)).await
    }
}

impl Display for RoomHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.room_id.deref().as_ref())
    }
}
