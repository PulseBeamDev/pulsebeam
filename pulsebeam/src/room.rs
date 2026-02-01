use std::{collections::HashMap, sync::Arc, time::Duration};

use pulsebeam_runtime::{
    actor::{ActorKind, ActorStatus, RunnerConfig},
    prelude::*,
};
use tokio::task::JoinSet;

use crate::{
    entity::{ParticipantId, RoomId, TrackId},
    gateway, node,
    participant::{self, ParticipantActor},
    track::{self, TrackMeta},
};
use pulsebeam_runtime::actor;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

pub enum RoomMessage {
    PublishTrack(track::TrackReceiver),
    AddParticipant(ParticipantActor),
    RemoveParticipant(ParticipantId),
    ReplaceParticipant(ParticipantActor),
}

#[derive(Clone, Debug)]
pub struct ParticipantMeta {
    handle: participant::ParticipantHandle,
    tracks: HashMap<TrackId, track::TrackReceiver>,
}

pub struct RoomMessageSet;

impl actor::MessageSet for RoomMessageSet {
    type Meta = RoomId;
    type Msg = RoomMessage;
    type ObservableState = RoomState;
}

/// Reponsibilities:
/// * Manage Participant Lifecycle
/// * Manage Track Lifecycle
/// * Maintain Room State Registry: Keep an up-to-date list of current participants and available tracks
/// * Broadcast Room Events
/// * Mediate Subscriptions: Process subscription requests to tracks
pub struct RoomActor {
    node_ctx: node::NodeContext,
    // participant_factory: Box<dyn actor::ActorFactory<participant::ParticipantActor>>,
    room_id: RoomId,
    state: RoomState,

    participant_tasks: JoinSet<(ParticipantId, ActorStatus)>,
}

#[derive(Default, Clone, Debug)]
pub struct RoomState {
    participants: HashMap<ParticipantId, ParticipantMeta>,
    tracks: HashMap<TrackId, track::TrackReceiver>,
}

impl actor::Actor<RoomMessageSet> for RoomActor {
    fn monitor() -> Arc<tokio_metrics::TaskMonitor> {
        static MONITOR: Lazy<Arc<TaskMonitor>> = Lazy::new(|| Arc::new(TaskMonitor::new()));
        MONITOR.clone()
    }

    fn kind() -> ActorKind {
        "room"
    }

    fn meta(&self) -> RoomId {
        self.room_id.clone()
    }

    fn get_observable_state(&self) -> RoomState {
        self.state.clone()
    }

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<RoomMessageSet>,
    ) -> Result<(), actor::ActorError> {
        pulsebeam_runtime::actor_loop!(self, ctx, pre_select: {},
            select: {
                Some(Ok((participant_id, _))) = self.participant_tasks.join_next() => {
                    self.handle_participant_left(participant_id).await;
                }
                _ = tokio::time::sleep(EMPTY_ROOM_TIMEOUT), if self.state.participants.is_empty() => {
                    tracing::info!("room has been empty for: {EMPTY_ROOM_TIMEOUT:?}, exiting.");
                    break;
                }
            }
        );

        Ok(())
    }

    async fn on_msg(
        &mut self,
        ctx: &mut actor::ActorContext<RoomMessageSet>,
        msg: RoomMessage,
    ) -> () {
        match msg {
            RoomMessage::AddParticipant(participant) => {
                self.handle_participant_joined(ctx, participant).await
            }
            RoomMessage::RemoveParticipant(participant_id) => {
                self.handle_participant_left(participant_id).await;
            }
            RoomMessage::ReplaceParticipant(participant) => {
                self.handle_replace_participant(ctx, participant).await;
            }
            RoomMessage::PublishTrack(track_handle) => {
                self.handle_track_published(track_handle).await;
            }
        };
    }
}

impl RoomActor {
    pub fn new(node_ctx: node::NodeContext, room_id: RoomId) -> Self {
        Self {
            node_ctx,
            room_id,
            state: RoomState::default(),
            participant_tasks: JoinSet::new(),
        }
    }

    async fn handle_participant_joined(
        &mut self,
        _ctx: &mut actor::ActorContext<RoomMessageSet>,
        participant_actor: ParticipantActor,
    ) {
        let (mut participant_handle, participant_task) =
            actor::prepare(participant_actor, RunnerConfig::default());
        let participant_id = participant_handle.meta.clone();

        self.participant_tasks.spawn(participant_task);

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
            .send(participant::ParticipantControlMessage::TracksSnapshot(
                self.state.tracks.clone(),
            ))
            .await;
    }

    async fn handle_participant_left(&mut self, participant_id: ParticipantId) {
        let Some(mut participant) = self.state.participants.remove(&participant_id) else {
            return;
        };

        // if it's closed, then the participant has exited
        let _ = participant.handle.terminate().await;
        for (track_id, _) in participant.tracks.iter_mut() {
            // Remove the track from the central registry.
            self.state.tracks.remove(track_id);
        }

        self.node_ctx
            .gateway
            .send(gateway::GatewayControlMessage::RemoveParticipant(
                participant_id,
            ))
            .await;

        // mark this tracks to be shared and immutable
        let tracks = Arc::new(participant.tracks);
        let msg = participant::ParticipantControlMessage::TracksUnpublished(tracks);
        self.broadcast_message(msg).await;
    }

    async fn handle_track_published(&mut self, track_handle: track::TrackReceiver) {
        let Some(origin) = self
            .state
            .participants
            .get_mut(&track_handle.meta.origin_participant)
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

    async fn handle_replace_participant(
        &mut self,
        ctx: &mut actor::ActorContext<RoomMessageSet>,
        new_participant: ParticipantActor,
    ) {
        let participant_id = new_participant.meta();

        if self.state.participants.contains_key(&participant_id) {
            tracing::info!(%participant_id, "Replacing existing participant session (takeover)");
            self.handle_participant_left(participant_id).await;
        }

        self.handle_participant_joined(ctx, new_participant).await;
    }

    async fn broadcast_message(&mut self, msg: participant::ParticipantControlMessage) {
        // TODO: handle large scale room by batching with a fixed interval driven by the
        // room instead of reactive.
        for participant in self.state.participants.values_mut() {
            let _ = participant.handle.send(msg.clone()).await;
        }
    }
}

pub type RoomHandle = actor::ActorHandle<RoomMessageSet>;
