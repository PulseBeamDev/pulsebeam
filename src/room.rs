use std::{collections::HashMap, fmt::Display, ops::Deref, sync::Arc};

use tokio::sync::mpsc::{self, error::SendError};

use crate::{
    controller::ControllerHandle,
    entity::{ParticipantId, RoomId, TrackId},
    participant::ParticipantHandle,
    rng::Rng,
    track::TrackHandle,
};

#[derive(Debug)]
pub enum RoomMessage {
    PublishMedia(TrackHandle),
    AddParticipant(ParticipantHandle),
    RemoveParticipant(Arc<ParticipantId>),
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
    controller: ControllerHandle,
    handle: RoomHandle,

    participants: HashMap<Arc<ParticipantId>, ParticipantMeta>,
}

impl RoomActor {
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                res = self.receiver.recv() => {
                    match res {
                        Some(msg) => self.handle_message(msg).await,
                        None => break,
                    }
                }
            }
        }
    }

    async fn handle_message(&mut self, mut msg: RoomMessage) {
        match msg {
            RoomMessage::AddParticipant(participant) => {
                for (_, meta) in &self.participants {
                    for (_, track) in &meta.tracks {
                        let _ = participant.new_track(track.clone()).await;
                    }
                }
                self.participants.insert(
                    participant.participant_id.clone(),
                    ParticipantMeta {
                        handle: participant,
                        tracks: HashMap::new(),
                    },
                );
            }
            RoomMessage::RemoveParticipant(participant_id) => {
                self.participants.remove(&participant_id);
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
    pub fn new(rng: Rng, controller: ControllerHandle, room_id: Arc<RoomId>) -> (Self, RoomActor) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = RoomHandle {
            sender,
            room_id: room_id.clone(),
        };
        let actor = RoomActor {
            rng,
            receiver,
            controller,
            handle: handle.clone(),
            participants: HashMap::new(),
        };
        (handle, actor)
    }

    pub async fn add_participant(
        &self,
        participant: ParticipantHandle,
    ) -> Result<(), SendError<RoomMessage>> {
        self.sender
            .send(RoomMessage::AddParticipant(participant))
            .await
    }

    pub async fn remove_participant(
        &self,
        participant_id: Arc<ParticipantId>,
    ) -> Result<(), SendError<RoomMessage>> {
        self.sender
            .send(RoomMessage::RemoveParticipant(participant_id))
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
