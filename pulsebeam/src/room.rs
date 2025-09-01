use std::{collections::HashMap, sync::Arc};

use futures::stream::{FuturesUnordered, StreamExt};
use str0m::Rtc;

use crate::{
    entity::{ParticipantId, RoomId, TrackId},
    message::TrackMeta,
    participant, source, system, track,
};
use pulsebeam_runtime::actor;

#[derive(Debug)]
pub enum RoomMessage {
    PublishTrack(Arc<TrackMeta>),
    AddParticipant(Arc<ParticipantId>, Box<Rtc>),
}

#[derive(Clone, Debug)]
pub struct ParticipantMeta {
    handle: participant::ParticipantHandle,
    tracks: HashMap<Arc<TrackId>, track::TrackHandle>,
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
    tracks: HashMap<Arc<TrackId>, track::TrackHandle>,
}

impl actor::Actor for RoomActor {
    type Meta = Arc<RoomId>;
    type HighPriorityMsg = RoomMessage;
    type LowPriorityMsg = ();
    type ObservableState = RoomState;

    fn meta(&self) -> Self::Meta {
        self.room_id.clone()
    }

    fn get_observable_state(&self) -> Self::ObservableState {
        self.state.clone()
    }

    async fn run(&mut self, ctx: &mut actor::ActorContext<Self>) -> Result<(), actor::ActorError> {
        loop {
            tokio::select! {
                biased;

                Some(msg) = ctx.sys_rx.recv() => {
                    self.on_system(ctx, msg).await;
                }
                Some(msg) = ctx.hi_rx.recv() => {
                    self.on_high_priority(ctx, msg).await;
                }

                Some((participant_id, _)) = self.participant_tasks.next() => {
                    self.handle_participant_left(participant_id).await;
                }

                Some((track_meta, _)) = self.track_tasks.next() => {
                    self.handle_track_unpublished(track_meta).await;
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
                self.handle_track_published(track_meta).await;
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
        mut rtc: Box<str0m::Rtc>,
    ) {
        let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        let participant_actor = participant::ParticipantActor::new(
            self.system_ctx.clone(),
            ctx.handle.clone(),
            participant_id.clone(),
            rtc,
        );

        // TODO: capacity
        let (participant_handle, participant_join) =
            actor::spawn(participant_actor, actor::RunnerConfig::default());
        self.participant_tasks.push(participant_join);

        self.system_ctx
            .source_handle
            .send_high(source::SourceControlMessage::AddParticipant(
                ufrag,
                participant_handle.clone(),
            ))
            .await
            .expect("TODO: handle error");
        self.state.participants.insert(
            participant_id.clone(),
            ParticipantMeta {
                handle: participant_handle.clone(),
                tracks: HashMap::new(),
            },
        );

        // TODO: remove tracks that the participant doesn't have access to
        // if we failed to send a message, this means that participant has exited. The cleanup
        // step will remove this participant from internal state.
        let _ = participant_handle
            .send_high(participant::ParticipantControlMessage::TracksSnapshot(
                self.state.tracks.clone(),
            ))
            .await;
    }

    async fn handle_participant_left(&mut self, participant_id: Arc<ParticipantId>) {
        let Some(participant) = self.state.participants.remove(&participant_id) else {
            return;
        };

        for track_id in participant.tracks.keys() {
            self.state.tracks.remove(track_id);
        }

        // mark this tracks to be shared and immutable
        let tracks = Arc::new(participant.tracks);
        let msg = participant::ParticipantControlMessage::TracksUnpublished(tracks);
        self.broadcast_message(msg).await;
    }

    async fn handle_track_published(&mut self, track_meta: Arc<TrackMeta>) {
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
            origin.handle.meta,
            track_id
        );

        // TODO: update capacities
        let (track_handle, track_join) = actor::spawn(track_actor, actor::RunnerConfig::default());
        self.track_tasks.push(track_join);
        origin.tracks.insert(track_id.clone(), track_handle.clone());
        self.state
            .tracks
            .insert(track_id.clone(), track_handle.clone());

        let mut new_tracks = HashMap::new();
        new_tracks.insert(track_id, track_handle.clone());
        let new_tracks = Arc::new(new_tracks);
        let msg = participant::ParticipantControlMessage::TracksPublished(new_tracks);
        self.broadcast_message(msg).await;
    }

    async fn handle_track_unpublished(&mut self, track_meta: Arc<TrackMeta>) {
        let track_handle = if let Some(track_handle) = self.state.tracks.remove(&track_meta.id) {
            track_handle
        } else {
            return;
        };
        if let Some(meta) = self
            .state
            .participants
            .get_mut(&track_meta.id.origin_participant)
        {
            meta.tracks.remove(&track_meta.id);
        }

        let mut removed_tracks = HashMap::new();
        removed_tracks.insert(track_meta.id.clone(), track_handle);
        let removed_tracks = Arc::new(removed_tracks);
        let msg = participant::ParticipantControlMessage::TracksUnpublished(removed_tracks);
        self.broadcast_message(msg).await;
    }

    async fn broadcast_message(&mut self, msg: participant::ParticipantControlMessage) {
        // TODO: handle large scale room by batching with a fixed interval driven by the
        // room instead of reactive.
        for participant in self.state.participants.values() {
            let _ = participant.handle.send_high(msg.clone()).await;
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
    use pulsebeam_runtime::rt;

    #[test]
    fn publish_tracks_correctly() {
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
                simulcast_rids: None,
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
