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
use pulsebeam_runtime::actor::{self, LocalActorHandle};
use pulsebeam_runtime::prelude::*;

pub enum RoomMessage {
    PublishTrack(Arc<TrackMeta>),
    AddParticipant(Arc<ParticipantId>, Box<Rtc>),
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
    participant_factory: Box<dyn actor::ActorFactory<participant::ParticipantActor>>,
}

impl actor::Actor for RoomActor {
    type ActorId = Arc<RoomId>;
    type HighPriorityMsg = RoomMessage;
    type LowPriorityMsg = ();

    fn id(&self) -> Self::ActorId {
        self.room_id.clone()
    }

    async fn run(&mut self, ctx: &mut actor::ActorContext<Self>) -> Result<(), actor::ActorError> {
        loop {
            tokio::select! {
                Some(msg) = ctx.hi_rx.recv() => {
                    self.on_high_priority(ctx, msg).await;
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
                let Some(origin) = self.participants.get_mut(&track_meta.id.origin_participant)
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
            participants: HashMap::new(),
            participant_tasks: JoinSet::new(),
            track_tasks: JoinSet::new(),
            participant_factory: Box::new(()),
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
        let (participant_handle, participant_runner) = self
            .participant_factory
            .prepare(participant_actor, actor::RunnerConfig::default());
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
        for p in self.participants.values() {
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

pub type RoomHandle = actor::LocalActorHandle<RoomActor>;

#[cfg(test)]
mod test {
    use str0m::media::Mid;

    use super::*;
    use crate::room::RoomActor;
    use crate::test_utils;

    #[test]
    fn name() {
        let mut sim = test_utils::create_sim();

        sim.client("test", async {
            let system_ctx = test_utils::create_system_ctx().await;
            let mut room = test_utils::create_runner(RoomActor::new(
                system_ctx,
                test_utils::create_room("roomA"),
            ));
            let (participant_id, participant_rtc) = test_utils::create_participant();

            room.actor
                .on_high_priority(
                    &mut room.ctx,
                    RoomMessage::AddParticipant(participant_id.clone(), participant_rtc),
                )
                .await;

            let track = TrackMeta {
                id: Arc::new(TrackId::new(participant_id.clone(), Mid::new())),
                kind: str0m::media::MediaKind::Video,
                simulcast: None,
            };
            room.actor
                .on_high_priority(&mut room.ctx, RoomMessage::PublishTrack(Arc::new(track)))
                .await;

            assert_eq!(room.actor.participants.len(), 1);
            assert_eq!(
                room.actor
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
