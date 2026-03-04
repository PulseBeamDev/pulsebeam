use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

use pulsebeam_runtime::{
    actor::ActorKind,
    prelude::*,
};
use tokio::task::JoinSet;

use crate::{
    entity::{ConnectionId, ParticipantId, RoomId, TrackId},
    node,
    participant::{self, shard::{ParticipantSlot, ShardHandle, ShardMessage}},
    track::{self},
};
use pulsebeam_runtime::actor;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(derive_more::From)]
pub enum RoomMessage {
    PublishTrack(track::TrackReceiver),
    AddParticipant(AddParticipant),
    RemoveParticipant(RemoveParticipant),
    ParticipantExited(ParticipantId, ConnectionId),
}

pub struct AddParticipant {
    pub slot: ParticipantSlot,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
}

pub struct RemoveParticipant {
    pub participant_id: ParticipantId,
}

#[derive(Clone, Debug)]
struct ParticipantMeta {
    tracks: HashMap<TrackId, track::TrackReceiver>,
    connection_id: ConnectionId,
}

pub struct RoomMessageSet;

impl actor::MessageSet for RoomMessageSet {
    type Meta = RoomId;
    type Msg = RoomMessage;
    type ObservableState = RoomState;
}

pub struct RoomActor {
    node_ctx: node::NodeContext,
    room_id: RoomId,
    state: RoomState,
    shard_task: JoinSet<()>,
    shard: Option<ShardHandle>,
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
                Some(Ok(_)) = self.shard_task.join_next() => {
                    tracing::info!("RoomShard task exited");
                    self.shard = None;
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
                        m.slot,
                        old_connection_id,
                        m.connection_id,
                    )
                    .await;
                } else {
                    self.handle_participant_joined(ctx, m.slot, m.connection_id)
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
            RoomMessage::ParticipantExited(participant_id, connection_id) => {
                self.handle_participant_exited(participant_id, connection_id).await;
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
            shard_task: JoinSet::new(),
            shard: None,
        }
    }

    /// Returns the shard handle, creating and spawning the shard task if needed.
    fn get_or_create_shard(&mut self, room_handle: RoomHandle) -> &ShardHandle {
        if self.shard.is_none() {
            let (shard, handle) = crate::participant::shard::RoomShard::new(room_handle);
            self.shard_task.spawn(shard.run());
            self.shard = Some(handle);
        }
        self.shard.as_ref().unwrap()
    }

    async fn handle_participant_joined(
        &mut self,
        ctx: &mut actor::ActorContext<RoomMessageSet>,
        mut slot: ParticipantSlot,
        connection_id: ConnectionId,
    ) {
        let participant_id = slot.participant_id;
        // Deliver the current track snapshot on join.
        slot.initial_tracks = self.state.tracks.clone();

        let shard_tx = self.get_or_create_shard(ctx.handle.clone()).tx.clone();
        let _ = shard_tx.try_send(ShardMessage::AddParticipant(slot));

        self.state.participants.insert(
            (participant_id, connection_id),
            ParticipantMeta {
                tracks: HashMap::new(),
                connection_id,
            },
        );
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

        // Request removal from shard (fire and forget).
        if let Some(shard) = &self.shard {
            let _ = shard.tx.try_send(ShardMessage::RemoveParticipant(
                participant_id,
                connection_id,
            ));
        }

        // Remove tracks published by this connection
        for (track_id, _) in participant.tracks.iter_mut() {
            self.state.tracks.remove(track_id);
        }

        // Broadcast unpublish to other participants
        let tracks = Arc::new(participant.tracks);
        self.broadcast_to_shard(ShardMessage::TracksUnpublished(tracks));

        tracing::info!(
            ?participant_id,
            ?connection_id,
            "Participant connection removed"
        );
    }

    async fn handle_participant_exited(
        &mut self,
        participant_id: ParticipantId,
        connection_id: ConnectionId,
    ) {
        let key = (participant_id, connection_id);
        if self.state.tombstoned.contains_key(&key) {
            return;
        }
        let Some(participant) = self.state.participants.remove(&key) else {
            return;
        };
        self.state.tombstoned.insert(key, tokio::time::Instant::now());
        let tracks = Arc::new(participant.tracks);
        for track_id in tracks.keys() {
            self.state.tracks.remove(track_id);
        }
        let msg = ShardMessage::TracksUnpublished(tracks);
        self.broadcast_to_shard(msg);
        tracing::info!(?participant_id, ?connection_id, "Participant exited naturally");
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

        // Broadcast new track to shard (covers all participants).
        let mut new_tracks = HashMap::new();
        new_tracks.insert(track_id, track_handle.clone());
        let new_tracks = Arc::new(new_tracks);
        self.broadcast_to_shard(ShardMessage::TracksPublished(new_tracks));
    }

    async fn handle_replace_participant(
        &mut self,
        ctx: &mut actor::ActorContext<RoomMessageSet>,
        new_slot: ParticipantSlot,
        old_connection_id: ConnectionId,
        new_connection_id: ConnectionId,
    ) {
        let participant_id = new_slot.participant_id;

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
        self.handle_participant_joined(ctx, new_slot, new_connection_id)
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

    fn broadcast_to_shard(&self, msg: ShardMessage) {
        if let Some(shard) = &self.shard {
            let _ = shard.tx.try_send(msg);
        }
    }
}

pub type RoomHandle = actor::ActorHandle<RoomMessageSet>;
