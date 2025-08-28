use std::{collections::HashMap, sync::Arc};

use tokio::task::JoinSet;
use tracing::Instrument;

use crate::{
    entity::{ParticipantId, RoomId, TrackId},
    message::TrackMeta,
    participant::{ParticipantActor, ParticipantHandle},
    rng::Rng,
    track::TrackHandle,
};
use pulsebeam_runtime::actor;

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
    room_id: Arc<RoomId>,

    participants: HashMap<Arc<ParticipantId>, ParticipantMeta>,
    participant_tasks: JoinSet<Arc<ParticipantId>>,
    track_tasks: JoinSet<Arc<TrackId>>,
}

impl actor::Actor for RoomActor {
    type ID = Arc<RoomId>;
    type HighPriorityMessage = RoomMessage;
    type LowPriorityMessage = ();

    fn kind(&self) -> &'static str {
        "room"
    }

    fn id(&self) -> Self::ID {
        self.room_id.clone()
    }

    async fn run(&mut self, mut ctx: actor::ActorContext<Self>) -> Result<(), actor::ActorError> {
        loop {
            tokio::select! {
                res = ctx.hi_rx.recv() => {
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
    pub fn new(rng: Rng, room_id: Arc<RoomId>) -> Self {
        Self {
            rng,
            room_id,
            participants: HashMap::new(),
            participant_tasks: JoinSet::new(),
            track_tasks: JoinSet::new(),
        }
    }

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
                let task = async move {
                    actor::run(participant_actor).await;
                    participant_id
                }
                .in_current_span();

                self.participant_tasks.spawn(task);

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
                tracing::info!("current tracks: {:?}", self.participants.values());
                let new_tracks = Arc::new(vec![handle]);
                for participant in self.participants.values() {
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

pub type RoomHandle = actor::LocalActorHandle<RoomActor>;

// #[cfg(test)]
// mod test {
//     use std::sync::Arc;
//
//     use rand::SeedableRng;
//
//     use crate::{
//         entity::{ExternalParticipantId, ExternalRoomId, ParticipantId, RoomId},
//         net::VirtualUdpSocket,
//         participant::ParticipantHandle,
//         rng::Rng,
//         room::RoomHandle,
//         sink::UdpSinkHandle,
//         source::UdpSourceHandle,
//     };
//
//     #[tokio::test]
//     async fn name() {
//         let socket = turmoil::net::UdpSocket::bind("0.0.0.0:3478").await.unwrap();
//         let socket: VirtualUdpSocket = Arc::new(socket).into();
//         let server_addr = "192.168.1.1:3478".parse().unwrap();
//
//         let rng = Rng::seed_from_u64(1);
//         let (source_handle, source_actor) = UdpSourceHandle::new(server_addr, socket.clone());
//         let (sink_handle, sink_actor) = UdpSinkHandle::new(socket.clone());
//
//         let room_id = RoomId::new(ExternalRoomId::new("test".to_string()).unwrap());
//         let participant_id = ParticipantId::new(
//             &mut rng,
//             ExternalParticipantId::new("a".to_string()).unwrap(),
//         );
//         let (room_handle, room_actor) = RoomHandle::new(rng, Arc::new(room_id));
//         ParticipantHandle::new(
//             rng,
//             source_handle,
//             sink_handle,
//             room_handle,
//             participant_id,
//             rtc,
//         );
//         handle.add_participant(handle, actor)
//     }
// }
