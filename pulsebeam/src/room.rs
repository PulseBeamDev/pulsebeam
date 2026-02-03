use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

use pulsebeam_runtime::{
    actor::{ActorKind, ActorStatus, RunnerConfig},
    prelude::*,
};
use tokio::task::JoinSet;

use crate::{
    entity::{ParticipantId, RoomId, TrackId},
    node,
    participant::{self, ParticipantActor},
    track::{self},
};
use pulsebeam_runtime::actor;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(derive_more::From)]
pub enum RoomMessage {
    PublishTrack(track::TrackReceiver),
    AddParticipant(AddParticipant),
    RemoveParticipant(ParticipantId),
}

pub struct AddParticipant {
    pub participant: ParticipantActor,
    pub reconnect: bool,
    pub version: u64,
}

#[derive(Clone, Debug)]
pub struct ParticipantMeta {
    handle: participant::ParticipantHandle,
    tracks: HashMap<TrackId, track::TrackReceiver>,
    version: u64,
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

    participant_tasks: JoinSet<(ParticipantId, u64, ActorStatus)>,
}

#[derive(Default, Clone, Debug)]
pub struct RoomState {
    participants: BTreeMap<(ParticipantId, u64), ParticipantMeta>,
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
                Some(Ok((participant_id, session_id, _))) = self.participant_tasks.join_next() => {
                    self.handle_participant_left(participant_id, session_id).await;
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
            RoomMessage::AddParticipant(m) => {
                if m.reconnect {
                    self.handle_replace_participant(ctx, m.participant, m.version)
                        .await;
                } else {
                    self.handle_participant_joined(ctx, m.participant, m.version)
                        .await
                }
            }
            RoomMessage::RemoveParticipant(participant_id) => {
                let versions_to_remove: Vec<_> = self
                    .state
                    .participants
                    .range((participant_id, u64::MIN)..=(participant_id, u64::MAX))
                    .map(|(_, meta)| meta.version)
                    .collect();

                for version in versions_to_remove {
                    self.handle_participant_left(participant_id, version).await;
                }
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
        version: u64,
    ) {
        let (mut participant_handle, participant_task) =
            actor::prepare(participant_actor, RunnerConfig::default());
        let participant_id = participant_handle.meta;

        self.participant_tasks.spawn(async move {
            let (id, status) = participant_task.await;
            (id, version, status)
        });

        self.state.participants.insert(
            (participant_id, version),
            ParticipantMeta {
                handle: participant_handle.clone(),
                tracks: HashMap::new(),
                version,
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

    async fn handle_participant_left(&mut self, participant_id: ParticipantId, version: u64) {
        if let Some(participant) = self.state.participants.get(&(participant_id, version)) {
            if participant.version != version {
                tracing::info!(%participant_id, %version, current_version = %participant.version, "Ignoring stale participant exit");
                return;
            }
        } else {
            return;
        }

        let Some(mut participant) = self.state.participants.remove(&(participant_id, version))
        else {
            return;
        };

        // if it's closed, then the participant has exited
        let _ = participant.handle.terminate().await;
        for (track_id, _) in participant.tracks.iter_mut() {
            // Remove the track from the central registry.
            self.state.tracks.remove(track_id);
        }

        // mark this tracks to be shared and immutable
        let tracks = Arc::new(participant.tracks);
        let msg = participant::ParticipantControlMessage::TracksUnpublished(tracks);
        self.broadcast_message(msg).await;
    }

    async fn handle_track_published(&mut self, track_handle: track::TrackReceiver) {
        let target_id = track_handle.meta.origin_participant;

        // TODO: handle versioning properly here
        let latest_entry = self
            .state
            .participants
            .range_mut((target_id, u64::MIN)..=(target_id, u64::MAX))
            .next_back();

        let Some(((_id, _version), origin)) = latest_entry else {
            tracing::warn!(
                "{} is missing from participants, ignoring track",
                track_handle.meta.id
            );
            return;
        };

        let track_id = track_handle.meta.id;
        tracing::info!(
            "{} published a track, added: {}",
            origin.handle.meta,
            track_id
        );

        origin.tracks.insert(track_id, track_handle.clone());
        self.state.tracks.insert(track_id, track_handle.clone());

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
        version: u64,
    ) {
        let participant_id = new_participant.meta();

        if let Some(p) = self.state.participants.get(&(participant_id, version)) {
            tracing::info!(%participant_id, "Replacing existing participant session (takeover)");
            self.handle_participant_left(participant_id, p.version)
                .await;
        }

        self.handle_participant_joined(ctx, new_participant, version)
            .await;
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
