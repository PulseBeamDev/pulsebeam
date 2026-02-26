use std::{
    collections::{BTreeMap, HashMap},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use futures_buffered::FuturesUnorderedBounded;
use pulsebeam_runtime::{
    actor::{ActorKind, ActorStatus, RunnerConfig},
    prelude::*,
};

use crate::{
    entity::{ConnectionId, ParticipantId, RoomId, TrackId},
    node,
    participant::{self, ParticipantActor},
    track::{self},
};
use futures_util::FutureExt;
use pulsebeam_runtime::actor;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(derive_more::From)]
pub enum RoomMessage {
    PublishTrack(track::TrackReceiver),
    AddParticipant(AddParticipant),
    RemoveParticipant(RemoveParticipant),
}

pub struct AddParticipant {
    pub participant: ParticipantActor,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
}

pub struct RemoveParticipant {
    pub participant_id: ParticipantId,
}

#[derive(Clone, Debug)]
pub struct ParticipantMeta {
    handle: participant::ParticipantHandle,
    tracks: HashMap<TrackId, track::TrackReceiver>,
    connection_id: ConnectionId,
}

pub struct RoomMessageSet;

impl actor::MessageSet for RoomMessageSet {
    type Meta = RoomId;
    type Msg = RoomMessage;
    type ObservableState = RoomState;
}

type ParticipantTask =
    dyn Future<Output = (ParticipantId, ConnectionId, ActorStatus)> + Send + 'static;
pub struct RoomActor {
    node_ctx: node::NodeContext,
    room_id: RoomId,
    state: RoomState,
    participant_tasks: FuturesUnorderedBounded<Pin<Box<ParticipantTask>>>,
}

#[derive(Default, Clone, Debug)]
pub struct RoomState {
    participants: BTreeMap<(ParticipantId, ConnectionId), ParticipantMeta>,
    tracks: HashMap<TrackId, track::TrackReceiver>,

    tombstoned: HashMap<(ParticipantId, ConnectionId), tokio::time::Instant>,
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
                Some((participant_id, connection_id, _)) = self.participant_tasks.next() => {
                    self.handle_participant_left(participant_id, connection_id).await;
                }
                _ = tokio::time::sleep(EMPTY_ROOM_TIMEOUT), if self.state.participants.is_empty() => {
                    tracing::info!("room has been empty for: {EMPTY_ROOM_TIMEOUT:?}, exiting.");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(300)) => {
                    self.cleanup_old_tombstones().await;
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
                if let Some(old_connection_id) = m.old_connection_id {
                    self.handle_replace_participant(
                        ctx,
                        m.participant,
                        old_connection_id,
                        m.connection_id,
                    )
                    .await;
                } else {
                    self.handle_participant_joined(ctx, m.participant, m.connection_id)
                        .await;
                }
            }
            RoomMessage::RemoveParticipant(m) => {
                self.remove_all_participant_connections(m.participant_id)
                    .await;
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
            participant_tasks: FuturesUnorderedBounded::new(128),
        }
    }

    async fn handle_participant_joined(
        &mut self,
        _ctx: &mut actor::ActorContext<RoomMessageSet>,
        participant_actor: ParticipantActor,
        connection_id: ConnectionId,
    ) {
        let (mut participant_handle, participant_task) =
            actor::prepare(participant_actor, RunnerConfig::default());
        let participant_id = participant_handle.meta;

        self.participant_tasks.try_push(
            participant_task
                .map(move |(id, status)| (id, connection_id, status))
                .boxed(),
        );

        self.state.participants.insert(
            (participant_id, connection_id),
            ParticipantMeta {
                handle: participant_handle.clone(),
                tracks: HashMap::new(),
                connection_id,
            },
        );

        // Send current tracks to new participant
        let _ = participant_handle
            .send(participant::ParticipantControlMessage::TracksSnapshot(
                self.state.tracks.clone(),
            ))
            .await;
    }

    async fn handle_participant_left(
        &mut self,
        participant_id: ParticipantId,
        connection_id: ConnectionId,
    ) {
        let key = (participant_id, connection_id);

        // Check if already tombstoned (eventual consistency - duplicate eviction)
        if self.state.tombstoned.contains_key(&key) {
            tracing::debug!(
                ?participant_id,
                ?connection_id,
                "Connection already tombstoned"
            );
            return;
        }

        let Some(mut participant) = self.state.participants.remove(&key) else {
            tracing::warn!(
                ?participant_id,
                ?connection_id,
                "Participant connection not found"
            );
            return;
        };

        // Mark as tombstoned for eventual consistency
        self.state
            .tombstoned
            .insert(key, tokio::time::Instant::now());

        // Terminate participant actor
        let _ = participant.handle.terminate().await;

        // Remove tracks published by this connection
        for (track_id, _) in participant.tracks.iter_mut() {
            self.state.tracks.remove(track_id);
        }

        // Broadcast unpublish to other participants
        let tracks = Arc::new(participant.tracks);
        let msg = participant::ParticipantControlMessage::TracksUnpublished(tracks);
        self.broadcast_message(msg).await;

        tracing::info!(
            ?participant_id,
            ?connection_id,
            "Participant connection removed"
        );
    }

    async fn handle_track_published(&mut self, track_handle: track::TrackReceiver) {
        let origin_participant_id = track_handle.meta.origin_participant;
        let track_id = track_handle.meta.id;

        let participant_entry = self
            .state
            .participants
            .iter_mut()
            .find(|((pid, _), _)| *pid == origin_participant_id);

        let Some(((_id, connection_id), origin)) = participant_entry else {
            tracing::warn!(
                ?track_id,
                ?origin_participant_id,
                "Participant not found, ignoring track"
            );
            return;
        };

        tracing::info!(
            ?origin_participant_id,
            ?connection_id,
            ?track_id,
            "Track published"
        );

        origin.tracks.insert(track_id, track_handle.clone());
        self.state.tracks.insert(track_id, track_handle.clone());

        // Broadcast new track to all participants
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
        old_connection_id: ConnectionId,
        new_connection_id: ConnectionId,
    ) {
        let participant_id = new_participant.meta();

        tracing::info!(
            ?participant_id,
            ?old_connection_id,
            ?new_connection_id,
            "Replacing participant connection"
        );

        // Evict old connection
        self.handle_participant_left(participant_id, old_connection_id)
            .await;

        // Add new connection
        self.handle_participant_joined(ctx, new_participant, new_connection_id)
            .await;
    }

    async fn remove_all_participant_connections(&mut self, participant_id: ParticipantId) {
        let start_bound = (participant_id, ConnectionId::MIN);
        let end_bound = (participant_id, ConnectionId::MAX);

        let keys_to_remove: Vec<_> = self
            .state
            .participants
            .range(start_bound..=end_bound)
            .map(|(key, _)| *key)
            .collect();

        for (_, connection_id) in keys_to_remove {
            self.handle_participant_left(participant_id, connection_id)
                .await;
        }
    }

    async fn cleanup_old_tombstones(&mut self) {
        let cutoff = tokio::time::Instant::now() - Duration::from_secs(3600); // 1 hour
        self.state
            .tombstoned
            .retain(|_, timestamp| *timestamp > cutoff);
    }

    async fn broadcast_message(&mut self, msg: participant::ParticipantControlMessage) {
        for participant in self.state.participants.values_mut() {
            let _ = participant.handle.send(msg.clone()).await;
        }
    }
}

pub type RoomHandle = actor::ActorHandle<RoomMessageSet>;
