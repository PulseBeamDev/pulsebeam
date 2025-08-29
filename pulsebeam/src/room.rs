use std::{collections::HashMap, sync::Arc};

use str0m::Rtc;
use tokio::task::JoinSet;
use tracing::Instrument;

use crate::{
    entity::{ParticipantId, RoomId, TrackId},
    message::TrackMeta,
    participant::{self, ParticipantActor, ParticipantHandle},
    system,
    track::{self, TrackHandle},
};
use pulsebeam_runtime::actor::{self, ActorHandle};

pub enum RoomMessage {
    PublishTrack(ParticipantHandle, Arc<TrackMeta>),
    AddParticipant(Arc<ParticipantId>, Rtc),
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
    system_ctx: system::SystemContext,

    room_id: Arc<RoomId>,
    participants: HashMap<Arc<ParticipantId>, ParticipantMeta>,
    participant_tasks: JoinSet<(Arc<ParticipantId>, actor::ActorStatus)>,
    track_tasks: JoinSet<(Arc<TrackId>, actor::ActorStatus)>,
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

    async fn run(&mut self, ctx: &mut actor::ActorContext<Self>) -> Result<(), actor::ActorError> {
        loop {
            tokio::select! {
                res = ctx.hi_rx.recv() => {
                    match res {
                        Some(msg) => self.handle_message(ctx, msg).await,
                        None => break,
                    }
                }

                Some(Ok((participant_id, _))) = self.participant_tasks.join_next() => {
                    self.handle_participant_left(participant_id).await;
                }

                Some(Ok((track_id, _))) = self.track_tasks.join_next() => {
                    // TODO: handle track lifecycle
                }

                else => break,
            }
        }

        Ok(())
    }
}

impl RoomActor {
    pub fn new(system_ctx: system::SystemContext, room_id: Arc<RoomId>) -> Self {
        Self {
            system_ctx,
            room_id,
            participants: HashMap::new(),
            participant_tasks: JoinSet::new(),
            track_tasks: JoinSet::new(),
        }
    }

    async fn handle_message(&mut self, ctx: &mut actor::ActorContext<Self>, msg: RoomMessage) {
        match msg {
            RoomMessage::AddParticipant(participant_id, rtc) => {
                self.handle_participant_joined(ctx, participant_id, rtc)
                    .await
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

                let track_actor =
                    track::TrackActor::new(participant_handle.clone(), track_meta.clone());
                let track_id = track_meta.id.clone();
                tracing::info!(
                    "{} published a track, added: {}",
                    origin.handle.participant_id,
                    track_id
                );

                // TODO: update capacities
                let (track_handle, track_runner) =
                    actor::LocalActorHandle::new(track_actor, actor::RunnerConfig::default());
                self.track_tasks.spawn(track_runner.run());
                let track_handle = track::TrackHandle {
                    handle: track_handle,
                    meta: track_meta,
                };
                origin.tracks.insert(track_id, track_handle.clone());
                let new_tracks = Arc::new(vec![track_handle]);
                for participant in self.participants.values() {
                    let _ = participant
                        .handle
                        .handle
                        .hi_send(participant::ParticipantControlMessage::TracksPublished(
                            new_tracks.clone(),
                        ))
                        .await;
                }
            }
        };
    }

    async fn handle_participant_joined(
        &mut self,
        ctx: &mut actor::ActorContext<Self>,
        participant_id: Arc<ParticipantId>,
        rtc: str0m::Rtc,
    ) {
        let participant_actor = ParticipantActor::new(
            self.system_ctx.clone(),
            ctx.handle.clone(),
            participant_id.clone(),
            rtc,
        );
        // TODO: capacity
        let (participant_handle, participant_runner) =
            actor::LocalActorHandle::new(participant_actor, actor::RunnerConfig::default());
        self.participant_tasks.spawn(participant_runner.run());

        // let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        // self.source
        //     .hi_send(source::SourceControlMessage::AddParticipant(
        //         ufrag,
        //         self.handle.clone(),
        //     ))
        //     .await
        //     .map_err(|_| ControllerError::ServiceUnavailable)?;
        self.participants.insert(
            participant_id.clone(),
            ParticipantMeta {
                handle: ParticipantHandle {
                    handle: participant_handle.clone(),
                    participant_id: participant_id.clone(),
                },
                tracks: HashMap::new(),
            },
        );

        let mut tracks = Vec::with_capacity(self.participants.len());
        for meta in self.participants.values() {
            tracks.extend(meta.tracks.values().cloned());
        }

        todo!();
        // let _ = participant_handle
        //     .hi_send(participant::ParticipantControlMessage::TracksUnpublished(
        //         Arc::new(tracks.clone()),
        //     ))
        //     .await;
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
        for (_, p) in &self.participants {
            let _ = p
                .handle
                .handle
                .hi_send(participant::ParticipantControlMessage::TracksUnpublished(
                    tracks.clone(),
                ))
                .await;
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
