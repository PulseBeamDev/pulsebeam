use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

use pulsebeam_runtime::prelude::*;
use tokio::sync::mpsc as tokio_mpsc;

use crate::{
    entity::{ConnectionId, ParticipantId, RoomId, TrackId},
    node,
    participant::{self, ParticipantActor},
    shard::{self, ShardControlMessage, ShardHandle, ShardToRoomMessage},
    track,
};
use pulsebeam_runtime::actor;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

// ─── Room messages (external API) ────────────────────────────────────────────

#[derive(derive_more::From)]
pub enum RoomMessage {
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

// ─── Room state ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ParticipantMeta {
    /// Which shard this participant lives in.
    shard_idx: usize,
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
    /// All shards managed by this room.
    shards: Vec<ShardHandle>,
    /// Next shard ID counter.
    next_shard_id: usize,
    /// Receives events from all shards (track published, participant exited).
    shard_rx: tokio_mpsc::Receiver<ShardToRoomMessage>,
    /// Cloned and given to each new shard so it can send back to the room.
    shard_tx: tokio_mpsc::Sender<ShardToRoomMessage>,
}

#[derive(Default, Clone, Debug)]
pub struct RoomState {
    participants: BTreeMap<(ParticipantId, ConnectionId), ParticipantMeta>,
    tracks: HashMap<TrackId, track::TrackReceiver>,
    tombstoned: HashMap<(ParticipantId, ConnectionId), tokio::time::Instant>,
}

// ─── Actor impl ───────────────────────────────────────────────────────────────

impl actor::Actor<RoomMessageSet> for RoomActor {
    fn monitor() -> Arc<tokio_metrics::TaskMonitor> {
        static MONITOR: Lazy<Arc<TaskMonitor>> = Lazy::new(|| Arc::new(TaskMonitor::new()));
        MONITOR.clone()
    }

    fn kind() -> actor::ActorKind {
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
        pulsebeam_runtime::actor_loop!(self, ctx,
            pre_select: {},
            select: {
                // Shard → Room events: track publications and participant exits.
                Some(shard_msg) = self.shard_rx.recv() => {
                    self.handle_shard_message(shard_msg).await;
                }
                // Idle room timeout.
                _ = tokio::time::sleep(EMPTY_ROOM_TIMEOUT), if self.state.participants.is_empty() => {
                    tracing::info!("room has been empty for {EMPTY_ROOM_TIMEOUT:?}, exiting");
                    break;
                }
                // Periodic tombstone cleanup.
                _ = tokio::time::sleep(Duration::from_secs(300)) => {
                    self.cleanup_old_tombstones();
                }
            }
        );

        // Shut down all shards gracefully.
        for shard in &self.shards {
            shard.send(ShardControlMessage::Shutdown).await;
        }

        Ok(())
    }

    async fn on_msg(
        &mut self,
        ctx: &mut actor::ActorContext<RoomMessageSet>,
        msg: RoomMessage,
    ) {
        match msg {
            RoomMessage::AddParticipant(m) => {
                if let Some(old_connection_id) = m.old_connection_id {
                    self.handle_replace_participant(ctx, m.participant, old_connection_id, m.connection_id)
                        .await;
                } else {
                    self.handle_participant_joined(m.participant, m.connection_id).await;
                }
            }
            RoomMessage::RemoveParticipant(m) => {
                self.remove_all_participant_connections(m.participant_id).await;
            }
        }
    }
}

// ─── RoomActor methods ────────────────────────────────────────────────────────

impl RoomActor {
    pub fn new(node_ctx: node::NodeContext, room_id: RoomId) -> Self {
        let (shard_tx, shard_rx) = tokio_mpsc::channel(256);
        Self {
            node_ctx,
            room_id,
            state: RoomState::default(),
            shards: Vec::new(),
            next_shard_id: 0,
            shard_rx,
            shard_tx,
        }
    }

    /// Pick the shard with the fewest participants, or spawn a new one if all
    /// are full or none exist yet.
    fn get_or_create_shard(&mut self) -> (usize, &ShardHandle) {
        // Find the shard with minimum load that is not yet full.
        let best = self
            .shards
            .iter()
            .enumerate()
            .filter(|(_, s)| s.participant_count() < shard::MAX_PARTICIPANTS_PER_SHARD)
            .min_by_key(|(_, s)| s.participant_count())
            .map(|(i, _)| i);

        if let Some(idx) = best {
            return (idx, &self.shards[idx]);
        }

        // All shards full (or none exist) — spawn a new one.
        let id = self.next_shard_id;
        self.next_shard_id += 1;
        let handle = shard::spawn(id, self.node_ctx.gateway.clone(), self.shard_tx.clone());
        self.shards.push(handle);
        let idx = self.shards.len() - 1;
        tracing::info!(room_id = %self.room_id, shard_id = id, "Spawned new shard");
        (idx, &self.shards[idx])
    }

    async fn handle_participant_joined(
        &mut self,
        participant: ParticipantActor,
        connection_id: ConnectionId,
    ) {
        let participant_id = participant.meta();
        let (shard_idx, shard) = self.get_or_create_shard();

        shard
            .send(ShardControlMessage::AddParticipant {
                actor: participant,
                connection_id,
                current_tracks: self.state.tracks.clone(),
            })
            .await;

        self.state.participants.insert(
            (participant_id, connection_id),
            ParticipantMeta {
                shard_idx,
                connection_id,
            },
        );

        tracing::info!(
            room_id = %self.room_id,
            ?participant_id,
            ?connection_id,
            shard_idx,
            "Participant joined shard"
        );
    }

    async fn handle_participant_left(
        &mut self,
        participant_id: ParticipantId,
        connection_id: ConnectionId,
    ) {
        let key = (participant_id, connection_id);

        if self.state.tombstoned.contains_key(&key) {
            tracing::debug!(?participant_id, ?connection_id, "Connection already tombstoned");
            return;
        }

        let Some(meta) = self.state.participants.remove(&key) else {
            tracing::warn!(?participant_id, ?connection_id, "Participant not found for removal");
            return;
        };

        self.state.tombstoned.insert(key, tokio::time::Instant::now());

        // Tell the shard to evict the participant.
        if let Some(shard) = self.shards.get(meta.shard_idx) {
            shard
                .send(ShardControlMessage::RemoveParticipant {
                    participant_id,
                    connection_id,
                })
                .await;
        }

        // Remove tracks published by this participant.
        let removed_tracks: HashMap<TrackId, track::TrackReceiver> = self
            .state
            .tracks
            .extract_if(|_, v| v.meta.origin_participant == participant_id)
            .collect();

        if !removed_tracks.is_empty() {
            let removed = Arc::new(removed_tracks);
            self.broadcast_to_shards(|| ShardControlMessage::TracksUnpublished(removed.clone())).await;
            // Also remove from our own state (already done by extract_if above).
            let _ = removed;
        }

        tracing::info!(
            room_id = %self.room_id,
            ?participant_id,
            ?connection_id,
            "Participant left"
        );
    }

    /// Handle a message arriving from a shard.
    async fn handle_shard_message(&mut self, msg: ShardToRoomMessage) {
        match msg {
            ShardToRoomMessage::TrackPublished {
                participant_id,
                connection_id,
                rx,
            } => {
                self.handle_track_published(participant_id, connection_id, rx).await;
            }
            ShardToRoomMessage::ParticipantExited {
                participant_id,
                connection_id,
            } => {
                // Participant disconnected within its shard — clean up room state.
                self.handle_participant_left(participant_id, connection_id).await;
            }
        }
    }

    async fn handle_track_published(
        &mut self,
        origin_participant_id: ParticipantId,
        _connection_id: ConnectionId,
        rx: track::TrackReceiver,
    ) {
        let track_id = rx.meta.id;

        tracing::info!(
            room_id = %self.room_id,
            ?origin_participant_id,
            ?track_id,
            "Track published"
        );

        self.state.tracks.insert(track_id, rx.clone());

        let mut new_tracks = HashMap::new();
        new_tracks.insert(track_id, rx);
        let new_tracks = Arc::new(new_tracks);

        self.broadcast_to_shards(|| ShardControlMessage::TracksPublished(new_tracks.clone())).await;
    }

    async fn handle_replace_participant(
        &mut self,
        _ctx: &mut actor::ActorContext<RoomMessageSet>,
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
        self.handle_participant_left(participant_id, old_connection_id).await;
        self.handle_participant_joined(new_participant, new_connection_id).await;
    }

    async fn remove_all_participant_connections(&mut self, participant_id: ParticipantId) {
        let start = (participant_id, ConnectionId::MIN);
        let end = (participant_id, ConnectionId::MAX);
        let keys: Vec<_> = self
            .state
            .participants
            .range(start..=end)
            .map(|(k, _)| *k)
            .collect();
        for (_, connection_id) in keys {
            self.handle_participant_left(participant_id, connection_id).await;
        }
    }

    /// Send a message to every active shard.
    ///
    /// Accept a closure so callers can construct each `ShardControlMessage`
    /// independently (avoiding a `Clone` bound on the enum, which can't be
    /// satisfied for variants that carry non-Clone data like `AddParticipant`).
    async fn broadcast_to_shards<F>(&self, make_msg: F)
    where
        F: Fn() -> ShardControlMessage,
    {
        for shard in &self.shards {
            shard.send(make_msg()).await;
        }
    }

    fn cleanup_old_tombstones(&mut self) {
        let cutoff = tokio::time::Instant::now() - Duration::from_secs(3600);
        self.state.tombstoned.retain(|_, ts| *ts > cutoff);
    }
}

pub type RoomHandle = actor::ActorHandle<RoomMessageSet>;


