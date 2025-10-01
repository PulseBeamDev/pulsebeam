use std::{collections::HashMap, sync::Arc, time::Duration};

use futures::{
    future::Either,
    stream::{FuturesUnordered, StreamExt},
};
use str0m::Rtc;

use crate::{
    entity::{ParticipantId, RoomId, TrackId},
    gateway,
    message::TrackMeta,
    node, participant, track,
};
use pulsebeam_runtime::actor;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum RoomMessage {
    PublishTrack(track::TrackHandle),
    AddParticipant(Arc<ParticipantId>, Rtc),
    RemoveParticipant(Arc<ParticipantId>),
}

#[derive(Clone, Debug)]
pub struct ParticipantMeta {
    handle: participant::ParticipantHandle,
    tracks: HashMap<Arc<TrackId>, track::TrackHandle>,
}

pub struct RoomMessageSet;

impl actor::MessageSet for RoomMessageSet {
    type Meta = Arc<RoomId>;
    type HighPriorityMsg = RoomMessage;
    type LowPriorityMsg = ();
    type ObservableState = RoomState;
}

/// Reponsibilities:
/// * Manage Participant Lifecycle
/// * Manage Track Lifecycle
/// * Maintain Room State Registry: Keep an up-to-date list of current participants and available tracks
/// * Broadcast Room Events
/// * Mediate Subscriptions: Process subscription requests to tracks
/// * Own & Supervise Track Actors
pub struct RoomActor {
    node_ctx: node::NodeContext,
    // participant_factory: Box<dyn actor::ActorFactory<participant::ParticipantActor>>,
    room_id: Arc<RoomId>,
    participant_tasks: FuturesUnordered<actor::JoinHandle<participant::ParticipantMessageSet>>,
    state: RoomState,
}

#[derive(Default, Clone, Debug)]
pub struct RoomState {
    participants: HashMap<Arc<ParticipantId>, ParticipantMeta>,
    tracks: HashMap<Arc<TrackId>, track::TrackHandle>,
}

impl actor::Actor<RoomMessageSet> for RoomActor {
    fn meta(&self) -> Arc<RoomId> {
        self.room_id.clone()
    }

    fn get_observable_state(&self) -> RoomState {
        self.state.clone()
    }

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<RoomMessageSet>,
    ) -> Result<(), actor::ActorError> {
        pulsebeam_runtime::actor_loop!(self, ctx,
            pre_select: {
                let empty_room_timer =
                    if self.state.participants.is_empty() {
                        Either::Left(tokio::time::sleep(EMPTY_ROOM_TIMEOUT))
                    } else {
                        Either::Right(futures::future::pending::<()>())
                    };
            },
            select: {
                Some((participant_id, _)) = self.participant_tasks.next() => {
                    self.handle_participant_left(participant_id).await;
                },

                _ = empty_room_timer => {
                    tracing::info!("room has been empty for: {EMPTY_ROOM_TIMEOUT:?}, exiting.");
                    break;
                }
            }
        );

        Ok(())
    }

    async fn on_high_priority(
        &mut self,
        ctx: &mut actor::ActorContext<RoomMessageSet>,
        msg: RoomMessage,
    ) -> () {
        match msg {
            RoomMessage::AddParticipant(participant_id, rtc) => {
                self.handle_participant_joined(ctx, participant_id, rtc)
                    .await
            }
            RoomMessage::RemoveParticipant(participant_id) => {
                if let Some(participant_handle) = self.state.participants.get_mut(&participant_id) {
                    // if it's closed, then the participant has exited
                    let _ = participant_handle.handle.terminate().await;
                }
            }
            RoomMessage::PublishTrack(track_handle) => {
                self.handle_track_published(track_handle).await;
            }
        };
    }
}

impl RoomActor {
    pub fn new(node_ctx: node::NodeContext, room_id: Arc<RoomId>) -> Self {
        Self {
            node_ctx,
            room_id,
            state: RoomState::default(),
            participant_tasks: FuturesUnordered::new(),
        }
    }

    async fn handle_participant_joined(
        &mut self,
        ctx: &mut actor::ActorContext<RoomMessageSet>,
        participant_id: Arc<ParticipantId>,
        mut rtc: str0m::Rtc,
    ) {
        let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        let participant_actor = participant::ParticipantActor::new(
            self.node_ctx.clone(),
            ctx.handle.clone(),
            participant_id.clone(),
            rtc,
        );

        // TODO: capacity
        let participant_cfg = actor::RunnerConfig::default().with_lo(1024);
        let (mut participant_handle, participant_join) =
            actor::spawn(participant_actor, participant_cfg);
        self.participant_tasks.push(participant_join);

        self.node_ctx
            .gateway
            .send_high(gateway::GatewayControlMessage::AddParticipant(
                ufrag.clone(),
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
        let Some(mut participant) = self.state.participants.remove(&participant_id) else {
            return;
        };

        // Explicitly terminate the tracks owned by the leaving participant.
        // This prevents "zombie" track actors from causing race conditions if the participant rejoins quickly.
        for (track_id, track_handle) in participant.tracks.iter_mut() {
            // Remove the track from the central registry.
            self.state.tracks.remove(track_id);
            // Terminate the track actor itself.
            let _ = track_handle.terminate().await;
        }

        self.node_ctx
            .gateway
            .send_high(gateway::GatewayControlMessage::RemoveParticipant(
                participant_id,
            ))
            .await
            .expect("TODO: handle error");

        // mark this tracks to be shared and immutable
        let tracks = Arc::new(participant.tracks);
        let msg = participant::ParticipantControlMessage::TracksUnpublished(tracks);
        self.broadcast_message(msg).await;
    }

    async fn handle_track_published(&mut self, track_handle: track::TrackHandle) {
        let Some(origin) = self
            .state
            .participants
            .get_mut(&track_handle.meta.id.origin_participant)
        else {
            tracing::warn!(
                "{} is missing from participants, ignoring track",
                track_handle.meta.id
            );
            return;
        };

        let track_id = track_handle.meta.id.clone();
        tracing::info!(
            "{} published a track, added: {}",
            origin.handle.meta,
            track_id
        );

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
        for participant in self.state.participants.values_mut() {
            let _ = participant.handle.send_high(msg.clone()).await;
        }
    }
}

pub type RoomHandle = actor::ActorHandle<RoomMessageSet>;

// #[cfg(test)]
// mod test {
//     use str0m::media::Mid;
//
//     use super::*;
//     use crate::room::RoomActor;
//     use crate::test_utils;
//     use pulsebeam_runtime::rt;
//     use std::time::Duration;
//
//     #[test]
//     fn publish_tracks_correctly() {
//         let mut sim = test_utils::create_sim();
//
//         sim.client("test", async {
//             let system_ctx = test_utils::create_system_ctx().await;
//             let (mut room_handle, _) =
//                 actor::spawn_default(RoomActor::new(system_ctx, test_utils::create_room("roomA")));
//             let (participant_id, participant_rtc) = test_utils::create_participant();
//
//             room_handle
//                 .send_high(RoomMessage::AddParticipant(
//                     participant_id.clone(),
//                     participant_rtc,
//                 ))
//                 .await
//                 .unwrap();
//
//             let track = TrackMeta {
//                 id: Arc::new(TrackId::new(participant_id.clone(), Mid::new())),
//                 kind: str0m::media::MediaKind::Video,
//                 simulcast_rids: None,
//             };
//             room_handle
//                 .send_high(RoomMessage::PublishTrack(Arc::new(track)))
//                 .await
//                 .unwrap();
//
//             rt::yield_now().await;
//             let state = room_handle.get_state().await.unwrap();
//             assert_eq!(state.participants.len(), 1);
//             assert_eq!(
//                 state
//                     .participants
//                     .get(&participant_id)
//                     .unwrap()
//                     .tracks
//                     .len(),
//                 1
//             );
//             Ok(())
//         });
//
//         sim.run().unwrap();
//     }
//
//     // Test that a participant's departure cleans up their tracks and updates room state.
//     #[test]
//     fn participant_leave_cleans_up_tracks() {
//         let mut sim = test_utils::create_sim();
//
//         sim.client("test", async {
//             // Setup: Create a room and add a participant with a track.
//             let system_ctx = test_utils::create_system_ctx().await;
//             let (mut room_handle, _) =
//                 actor::spawn_default(RoomActor::new(system_ctx, test_utils::create_room("roomA")));
//             let (participant_id, participant_rtc) = test_utils::create_participant();
//
//             room_handle
//                 .send_high(RoomMessage::AddParticipant(
//                     participant_id.clone(),
//                     participant_rtc,
//                 ))
//                 .await
//                 .unwrap();
//
//             let track = TrackMeta {
//                 id: Arc::new(TrackId::new(participant_id.clone(), Mid::new())),
//                 kind: str0m::media::MediaKind::Video,
//                 simulcast_rids: None,
//             };
//             room_handle
//                 .send_high(RoomMessage::PublishTrack(Arc::new(track)))
//                 .await
//                 .unwrap();
//
//             rt::yield_now().await;
//
//             // Simulate participant leaving by dropping their actor.
//             let state = room_handle.get_state().await.unwrap();
//             let mut participant_handle = state
//                 .participants
//                 .get(&participant_id)
//                 .unwrap()
//                 .handle
//                 .clone();
//             participant_handle.terminate().await.unwrap();
//
//             // Allow time for the RoomActor to process the participant leaving.
//             rt::sleep(Duration::from_millis(100)).await;
//
//             // Verify: The participant and their tracks should be removed from the room state.
//             let state = room_handle.get_state().await.unwrap();
//             assert_eq!(state.participants.len(), 0, "Participant should be removed");
//             assert_eq!(state.tracks.len(), 0, "Tracks should be removed");
//
//             Ok(())
//         });
//
//         sim.run().unwrap();
//     }
//
//     // Test that publishing a track for an unknown participant is ignored.
//     #[test]
//     fn publish_track_for_unknown_participant() {
//         let mut sim = test_utils::create_sim();
//
//         sim.client("test", async {
//             // Setup: Create a room without any participants.
//             let system_ctx = test_utils::create_system_ctx().await;
//             let (mut room_handle, _) =
//                 actor::spawn_default(RoomActor::new(system_ctx, test_utils::create_room("roomA")));
//             let (participant_id, _) = test_utils::create_participant();
//
//             // Attempt to publish a track for a participant that doesn't exist.
//             let track = TrackMeta {
//                 id: Arc::new(TrackId::new(participant_id.clone(), Mid::new())),
//                 kind: str0m::media::MediaKind::Video,
//                 simulcast_rids: None,
//             };
//             room_handle
//                 .send_high(RoomMessage::PublishTrack(Arc::new(track)))
//                 .await
//                 .unwrap();
//
//             rt::yield_now().await;
//
//             // Verify: The track should not be added to the room state.
//             let state = room_handle.get_state().await.unwrap();
//             assert_eq!(state.participants.len(), 0, "No participants should exist");
//             assert_eq!(state.tracks.len(), 0, "No tracks should be added");
//
//             Ok(())
//         });
//
//         sim.run().unwrap();
//     }
//
//     // Test that concurrent participant joins are handled correctly.
//     #[test]
//     fn concurrent_participant_joins() {
//         let mut sim = test_utils::create_sim();
//
//         sim.client("test", async {
//             // Setup: Create a room.
//             let system_ctx = test_utils::create_system_ctx().await;
//             let (mut room_handle, _) =
//                 actor::spawn_default(RoomActor::new(system_ctx, test_utils::create_room("roomA")));
//
//             // Create multiple participants.
//             let participants: Vec<_> = (0..3).map(|_| test_utils::create_participant()).collect();
//
//             // Send AddParticipant messages concurrently.
//             for (participant_id, participant_rtc) in participants {
//                 room_handle
//                     .send_high(RoomMessage::AddParticipant(
//                         participant_id.clone(),
//                         participant_rtc,
//                     ))
//                     .await
//                     .unwrap();
//             }
//
//             rt::yield_now().await;
//
//             // Verify: All participants should be added to the room state.
//             let state = room_handle.get_state().await.unwrap();
//             assert_eq!(
//                 state.participants.len(),
//                 3,
//                 "All participants should be added"
//             );
//
//             Ok(())
//         });
//
//         sim.run().unwrap();
//     }
//
//     // Test that a failed message send to a participant is handled gracefully.
//     #[test]
//     fn handle_failed_message_send() {
//         let mut sim = test_utils::create_sim();
//
//         sim.client("test", async {
//             // Setup: Create a room and add a participant.
//             let system_ctx = test_utils::create_system_ctx().await;
//             let (mut room_handle, _) =
//                 actor::spawn_default(RoomActor::new(system_ctx, test_utils::create_room("roomA")));
//             let (participant_id, participant_rtc) = test_utils::create_participant();
//
//             room_handle
//                 .send_high(RoomMessage::AddParticipant(
//                     participant_id.clone(),
//                     participant_rtc,
//                 ))
//                 .await
//                 .unwrap();
//
//             rt::yield_now().await;
//
//             // Simulate a participant becoming unresponsive by terminating it.
//             let state = room_handle.get_state().await.unwrap();
//             let mut participant_handle = state
//                 .participants
//                 .get(&participant_id)
//                 .unwrap()
//                 .handle
//                 .clone();
//             participant_handle.terminate().await.unwrap();
//
//             // Publish a track, which triggers a broadcast to all participants.
//             let track = TrackMeta {
//                 id: Arc::new(TrackId::new(participant_id.clone(), Mid::new())),
//                 kind: str0m::media::MediaKind::Video,
//                 simulcast_rids: None,
//             };
//             room_handle
//                 .send_high(RoomMessage::PublishTrack(Arc::new(track)))
//                 .await
//                 .unwrap();
//
//             // Allow time for the broadcast to attempt sending to the terminated participant.
//             rt::sleep(Duration::from_millis(100)).await;
//
//             // Verify: The room should still be in a consistent state, even if the broadcast fails.
//             let state = room_handle.get_state().await.unwrap();
//             assert_eq!(
//                 state.participants.len(),
//                 0,
//                 "Participant should be removed due to termination"
//             );
//             assert_eq!(state.tracks.len(), 0, "No tracks should be added");
//
//             Ok(())
//         });
//
//         sim.run().unwrap();
//     }
// }
