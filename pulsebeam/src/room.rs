use std::{collections::HashMap, sync::Arc};

use futures::stream::{FuturesUnordered, StreamExt};
use str0m::Rtc;

use crate::{
    entity::{ParticipantId, RoomId, TrackId},
    message::TrackMeta,
    participant::{self, ParticipantActor, ParticipantHandle},
    system,
    track::{self, TrackHandle},
};
use pulsebeam_runtime::actor;

#[derive(Debug)]
pub enum RoomMessage {
    PublishTrack(Arc<TrackMeta>),
    AddParticipant(Arc<ParticipantId>, Box<Rtc>),
}

#[derive(Clone, Debug)]
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
    // participant_factory: Box<dyn actor::ActorFactory<participant::ParticipantActor>>,
    room_id: Arc<RoomId>,
    participant_tasks: FuturesUnordered<actor::JoinHandle<participant::ParticipantActor>>,
    track_tasks: FuturesUnordered<actor::JoinHandle<track::TrackActor>>,
    state: RoomState,
}

#[derive(Default, Clone, Debug)]
pub struct RoomState {
    participants: HashMap<Arc<ParticipantId>, ParticipantMeta>,
}

impl actor::Actor for RoomActor {
    type ActorId = Arc<RoomId>;
    type HighPriorityMsg = RoomMessage;
    type LowPriorityMsg = ();
    type ObservableState = RoomState;

    fn id(&self) -> Self::ActorId {
        self.room_id.clone()
    }

    fn get_observable_state(&self) -> Self::ObservableState {
        self.state.clone()
    }

    async fn run(&mut self, ctx: &mut actor::ActorContext<Self>) -> Result<(), actor::ActorError> {
        loop {
            tokio::select! {
                Some(msg) = ctx.sys_rx.recv() => {
                    self.on_system(ctx, msg).await;
                }
                Some(msg) = ctx.hi_rx.recv() => {
                    self.on_high_priority(ctx, msg).await;
                }

                Some((participant_id, _)) = self.participant_tasks.next() => {
                    self.handle_participant_left(participant_id).await;
                }

                Some((track_id, _)) = self.track_tasks.next() => {
                    // TODO: handle track lifecycle
                }

                else => break,
            }
        }

        Ok(())
    }

    async fn on_high_priority(
        &mut self,
        ctx: &mut actor::ActorContext<Self>,
        msg: Self::HighPriorityMsg,
    ) -> () {
        match msg {
            RoomMessage::AddParticipant(participant_id, rtc) => {
                self.handle_participant_joined(ctx, participant_id, rtc)
                    .await
            }
            RoomMessage::PublishTrack(track_meta) => {
                let Some(origin) = self
                    .state
                    .participants
                    .get_mut(&track_meta.id.origin_participant)
                else {
                    tracing::warn!(
                        "{} is missing from participants, ignoring track",
                        track_meta.id
                    );
                    return;
                };

                let track_actor = track::TrackActor::new(origin.handle.clone(), track_meta.clone());
                let track_id = track_meta.id.clone();
                tracing::info!(
                    "{} published a track, added: {}",
                    origin.handle.participant_id,
                    track_id
                );

                // TODO: update capacities
                let (track_handle, track_join) =
                    actor::spawn(track_actor, actor::RunnerConfig::default());
                self.track_tasks.push(track_join);
                let track_handle = track::TrackHandle {
                    handle: track_handle,
                    meta: track_meta,
                };
                origin.tracks.insert(track_id, track_handle.clone());
                let new_tracks = Arc::new(vec![track_handle]);
                for participant in self.state.participants.values() {
                    let _ = participant
                        .handle
                        .handle
                        .send_high(participant::ParticipantControlMessage::TracksPublished(
                            new_tracks.clone(),
                        ))
                        .await;
                }
            }
        };
    }
}

impl RoomActor {
    pub fn new(system_ctx: system::SystemContext, room_id: Arc<RoomId>) -> Self {
        Self {
            system_ctx,
            room_id,
            state: RoomState::default(),
            participant_tasks: FuturesUnordered::new(),
            track_tasks: FuturesUnordered::new(),
        }
    }

    async fn handle_participant_joined(
        &mut self,
        ctx: &mut actor::ActorContext<Self>,
        participant_id: Arc<ParticipantId>,
        rtc: Box<str0m::Rtc>,
    ) {
        let participant_actor = ParticipantActor::new(
            self.system_ctx.clone(),
            ctx.handle.clone(),
            participant_id.clone(),
            rtc,
        );

        // TODO: capacity
        let (participant_handle, participant_join) =
            actor::spawn(participant_actor, actor::RunnerConfig::default());
        self.participant_tasks.push(participant_join);

        // let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        // self.source
        //     .hi_send(source::SourceControlMessage::AddParticipant(
        //         ufrag,
        //         self.handle.clone(),
        //     ))
        //     .await
        //     .map_err(|_| ControllerError::ServiceUnavailable)?;
        self.state.participants.insert(
            participant_id.clone(),
            ParticipantMeta {
                handle: ParticipantHandle {
                    handle: participant_handle.clone(),
                    participant_id: participant_id.clone(),
                },
                tracks: HashMap::new(),
            },
        );

        let mut tracks = Vec::with_capacity(self.state.participants.len());
        for meta in self.state.participants.values() {
            tracks.extend(meta.tracks.values().cloned());
        }

        // let _ = participant_handle
        //     .hi_send(participant::ParticipantControlMessage::TracksUnpublished(
        //         Arc::new(tracks.clone()),
        //     ))
        //     .await;
    }

    async fn handle_participant_left(&mut self, participant_id: Arc<ParticipantId>) {
        // TODO: notify participant leaving
        let Some(participant) = self.state.participants.remove(&participant_id) else {
            return;
        };

        let tracks: Vec<Arc<TrackId>> = participant
            .tracks
            .into_values()
            .map(|t| t.meta.id.clone())
            .collect();
        let tracks = Arc::new(tracks);
        for p in self.state.participants.values() {
            let _ = p
                .handle
                .handle
                .send_high(participant::ParticipantControlMessage::TracksUnpublished(
                    tracks.clone(),
                ))
                .await;
        }
    }
}

pub type RoomHandle = actor::ActorHandle<RoomActor>;

#[cfg(test)]
mod test {
    use str0m::media::Mid;

    use super::*;
    use crate::room::RoomActor;
    use crate::test_utils;
    use pulsebeam_runtime::{prelude::*, rt};

    #[test]
    fn name() {
        let mut sim = test_utils::create_sim();

        sim.client("test", async {
            let system_ctx = test_utils::create_system_ctx().await;
            let (room_handle, _) =
                actor::spawn_default(RoomActor::new(system_ctx, test_utils::create_room("roomA")));
            let (participant_id, participant_rtc) = test_utils::create_participant();

            room_handle
                .send_high(RoomMessage::AddParticipant(
                    participant_id.clone(),
                    participant_rtc,
                ))
                .await
                .unwrap();

            let track = TrackMeta {
                id: Arc::new(TrackId::new(participant_id.clone(), Mid::new())),
                kind: str0m::media::MediaKind::Video,
                simulcast: None,
            };
            room_handle
                .send_high(RoomMessage::PublishTrack(Arc::new(track)))
                .await
                .unwrap();

            rt::yield_now().await;
            let state = room_handle.get_state().await.unwrap();
            assert_eq!(state.participants.len(), 1);
            assert_eq!(
                state
                    .participants
                    .get(&participant_id)
                    .unwrap()
                    .tracks
                    .len(),
                1
            );
            Ok(())
        });

        sim.run().unwrap();
    }
}
