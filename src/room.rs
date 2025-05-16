use std::{collections::HashMap, fmt::Display, ops::Deref, sync::Arc};

use rand::RngCore;
use tokio::{
    sync::mpsc::{self, error::SendError},
    task::JoinSet,
};
use tracing::Instrument;

use crate::{
    controller::ControllerHandle,
    entity::{ParticipantId, RoomId, TrackId},
    message::TrackIn,
    participant::ParticipantHandle,
    rng::Rng,
    track::TrackHandle,
};

#[derive(Debug)]
pub enum RoomMessage {
    PublishMedia(TrackIn),
    AddParticipant(ParticipantHandle),
    RemoveParticipant(Arc<ParticipantId>),
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

    participants: HashMap<Arc<ParticipantId>, ParticipantHandle>,
    tracks: HashMap<Arc<TrackId>, TrackHandle>,

    track_tasks: JoinSet<Arc<TrackId>>,
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

                Some(Ok(key)) = self.track_tasks.join_next() => {
                    // track actor exited
                    self.tracks.remove(&key);
                }
            }
        }
    }

    async fn handle_message(&mut self, mut msg: RoomMessage) {
        match msg {
            RoomMessage::AddParticipant(participant) => {
                for (_, track) in &self.tracks {
                    let _ = participant.new_track(track.clone()).await;
                }
                self.participants
                    .insert(participant.participant_id.clone(), participant);
            }
            RoomMessage::RemoveParticipant(participant_id) => {
                self.participants.remove(&participant_id);
                // TODO: clean up subscriptions and published medias
            }
            RoomMessage::PublishMedia(track) => {
                let Some(origin_handle) = self.participants.get(&track.id.origin_participant)
                else {
                    return;
                };

                if let Some(_) = self.tracks.get(&track.id) {
                    tracing::warn!(
                        "Detected an update to an existing track. This is ignored for now."
                    );
                } else {
                    let track_id = track.id.clone();
                    let track = Arc::new(track);
                    let (handle, actor) = TrackHandle::new(origin_handle.clone(), track);
                    self.tracks.insert(track_id.clone(), handle.clone());
                    self.track_tasks.spawn(
                        async move {
                            actor.run().await;
                            track_id
                        }
                        .in_current_span(),
                    );

                    for (_, participant) in &self.participants {
                        let _ = participant.new_track(handle.clone()).await;
                    }
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
            tracks: HashMap::new(),
            track_tasks: JoinSet::new(),
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

    pub async fn publish(&self, track: TrackIn) -> Result<(), SendError<RoomMessage>> {
        self.sender.send(RoomMessage::PublishMedia(track)).await
    }
}

impl Display for RoomHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.room_id.deref().as_ref())
    }
}
