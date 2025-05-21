use std::{collections::HashMap, fmt::Display, ops::Deref, sync::Arc};

use str0m::Rtc;
use tokio::{
    sync::mpsc::{self, error::SendError},
    task::JoinSet,
};
use tracing::Instrument;

use crate::{
    actor::{self, Actor, ActorError},
    context,
    entity::{ParticipantId, RoomId, TrackId},
    message::TrackIn,
    participant::{ParticipantActor, ParticipantHandle},
    rng::Rng,
    router::RouterHandle,
};

#[derive(Debug)]
pub enum RoomMessage {
    PublishTrack(Arc<TrackIn>),
    AddParticipant(Arc<ParticipantId>, Rtc),
}

pub struct ParticipantMeta {
    handle: ParticipantHandle,
    tracks: HashMap<Arc<TrackId>, Arc<TrackIn>>,
}

/// Reponsibilities:
/// * Manage Participant Lifecycle
/// * Manage Track Lifecycle
/// * Maintain Room State Registry: Keep an up-to-date list of current participants and available tracks
/// * Broadcast Room Events
/// * Mediate Subscriptions: Process subscription requests to tracks
/// * Own & Supervise Track Actors
pub struct RoomActor {
    ctx: context::Context,
    receiver: mpsc::Receiver<RoomMessage>,
    handle: RoomHandle,
    router: RouterHandle,

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
                    self.handle_participant_left(participant_id).await;
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
            RoomMessage::AddParticipant(participant_id, rtc) => {
                let mut tracks = Vec::new();
                for meta in self.participants.values() {
                    for (track_id, _) in &meta.tracks {
                        tracks.push(track_id.clone());
                    }
                }

                let (participant_handle, participant_actor) = ParticipantHandle::new(
                    self.ctx.clone(),
                    self.handle.clone(),
                    self.router.clone(),
                    participant_id.clone(),
                    rtc,
                    tracks,
                );
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
            }
            RoomMessage::PublishTrack(track_id) => {
                let Some(origin) = self.participants.get_mut(&track_id.origin_participant) else {
                    tracing::warn!("{} is missing from participants, ignoring track", track_id);
                    return;
                };

                origin.tracks.insert(track_id.clone(), track_id.clone());
                let new_tracks = Arc::new(vec![track_id]);
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
pub struct RoomHandle {
    pub sender: mpsc::Sender<RoomMessage>,
    pub room_id: Arc<RoomId>,
}

impl RoomHandle {
    pub fn new(ctx: context::Context, room_id: Arc<RoomId>) -> (Self, RoomActor) {
        let (sender, receiver) = mpsc::channel(8);
        let handle = RoomHandle {
            sender,
            room_id: room_id.clone(),
        };
        let actor = RoomActor {
            ctx,
            receiver,
            handle: handle.clone(),
            participants: HashMap::new(),
            participant_tasks: JoinSet::new(),
        };
        (handle, actor)
    }

    pub async fn add_participant(
        &self,
        participant_id: Arc<ParticipantId>,
        rtc: Rtc,
    ) -> Result<(), SendError<RoomMessage>> {
        self.sender
            .send(RoomMessage::AddParticipant(participant_id, rtc))
            .await
    }

    pub async fn publish(&self, track: Arc<TrackIn>) -> Result<(), SendError<RoomMessage>> {
        self.sender.send(RoomMessage::PublishTrack(track)).await
    }
}

impl Display for RoomHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.room_id.deref().as_ref())
    }
}
