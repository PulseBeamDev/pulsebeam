use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch;

use pulsebeam_runtime::{
    actor::{ActorKind, ActorStatus, RunnerConfig},
    prelude::*,
};
use tokio::task::JoinSet;

use crate::{
    entity::{ConnectionId, ParticipantId, RoomId, TrackId},
    node,
    participant::{self, ParticipantActor},
    track::{self},
};
use futures_util::FutureExt;
use pulsebeam_runtime::actor;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

/// Complete snapshot of all active tracks in the room.
///
/// Written by the room actor (once per track-lifecycle event), read by
/// every participant actor via a `watch::Receiver`. Always reflects the
/// current authoritative state — no delta messages are sent to participants.
pub type TrackMap = HashMap<TrackId, track::TrackReceiver>;
/// The write-half of the room-wide track watch channel. Owned by `RoomActor`
/// and also held (as `Arc`) by the controller so it can hand a `Receiver` to
/// each new participant at spawn time.
pub type TrackWatchSender = Arc<watch::Sender<Arc<TrackMap>>>;
/// The read-half handed to each `ParticipantActor` at construction.
pub type TrackWatchReceiver = watch::Receiver<Arc<TrackMap>>;

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

pub struct RoomActor {
    node_ctx: node::NodeContext,
    room_id: RoomId,
    state: RoomState,
    participant_tasks: JoinSet<(ParticipantId, ConnectionId, ActorStatus)>,
    /// Write-half of the room-wide track watch channel. One `send_modify` call
    /// per track event — O(1) regardless of participant count.
    track_watch: TrackWatchSender,
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
                Some(Ok((participant_id, connection_id, _))) = self.participant_tasks.join_next() => {
                    self.handle_participant_left(participant_id, connection_id);
                }
                _ = tokio::time::sleep(EMPTY_ROOM_TIMEOUT), if self.state.participants.is_empty() => {
                    tracing::info!("room has been empty for: {EMPTY_ROOM_TIMEOUT:?}, exiting.");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(300)) => {
                    self.cleanup_old_tombstones();
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
                    );
                } else {
                    self.handle_participant_joined(ctx, m.participant, m.connection_id);
                }
            }
            RoomMessage::RemoveParticipant(m) => {
                self.remove_all_participant_connections(m.participant_id);
            }
            RoomMessage::PublishTrack(track_handle) => {
                self.handle_track_published(track_handle);
            }
        };
    }
}

impl RoomActor {
    pub fn new(node_ctx: node::NodeContext, room_id: RoomId, track_watch: TrackWatchSender) -> Self {
        Self {
            node_ctx,
            room_id,
            state: RoomState::default(),
            participant_tasks: JoinSet::new(),
            track_watch,
        }
    }

    fn handle_participant_joined(
        &mut self,
        _ctx: &mut actor::ActorContext<RoomMessageSet>,
        participant_actor: ParticipantActor,
        connection_id: ConnectionId,
    ) {
        let (participant_handle, participant_task) =
            actor::prepare(participant_actor, RunnerConfig::default());
        let participant_id = participant_handle.meta;

        // Use .map() instead of an async block to avoid the Rust compiler storing
        // participant_task twice at the Suspend0 point (once as upvar, once as __awaitee),
        // which doubles the per-entry size in the JoinSet from ~73KB to ~37KB.
        self.participant_tasks.spawn(
            participant_task.map(move |(id, status)| (id, connection_id, status)),
        );

        self.state.participants.insert(
            (participant_id, connection_id),
            ParticipantMeta {
                handle: participant_handle,
                tracks: HashMap::new(),
                connection_id,
            },
        );
        // Track distribution is handled via the watch::Receiver the participant
        // received at construction time — no TracksSnapshot message needed.
    }

    fn handle_participant_left(
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

        let Some(participant) = self.state.participants.remove(&key) else {
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

        // Signal the participant task to terminate.
        // try_send is safe here: the sys_tx channel has capacity 1. If it's already
        // full (e.g. a GetState is in flight), the participant will drain the channel
        // shortly and then process our next attempt — or it will exit on its own when
        // its room handle closes. Either way, the task will not leak.
        let _ = participant
            .handle
            .sys_tx
            .try_send(actor::SystemMsg::Terminate);

        // Remove this participant's tracks from the room state and update the watch.
        let removed_ids: Vec<TrackId> = participant.tracks.keys().copied().collect();
        if !removed_ids.is_empty() {
            for id in &removed_ids {
                self.state.tracks.remove(id);
            }
            // O(1) write: all participant watch::Receivers see the new snapshot atomically.
            self.track_watch.send_modify(|map| {
                let m = Arc::make_mut(map);
                for id in &removed_ids {
                    m.remove(id);
                }
            });
        }

        tracing::info!(
            ?participant_id,
            ?connection_id,
            "Participant connection removed"
        );
    }

    fn handle_track_published(&mut self, track_handle: track::TrackReceiver) {
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

        // O(1) write: all participant watch::Receivers see the new snapshot atomically.
        // Arc::make_mut performs a clone-on-write only if other Arc handles exist,
        // which only happens during the brief window a participant is reading it.
        self.track_watch.send_modify(|map| {
            Arc::make_mut(map).insert(track_id, track_handle);
        });
    }

    fn handle_replace_participant(
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

        self.handle_participant_left(participant_id, old_connection_id);
        self.handle_participant_joined(ctx, new_participant, new_connection_id);
    }

    fn remove_all_participant_connections(&mut self, participant_id: ParticipantId) {
        let start_bound = (participant_id, ConnectionId::MIN);
        let end_bound = (participant_id, ConnectionId::MAX);

        let keys_to_remove: Vec<_> = self
            .state
            .participants
            .range(start_bound..=end_bound)
            .map(|(key, _)| *key)
            .collect();

        for (_, connection_id) in keys_to_remove {
            self.handle_participant_left(participant_id, connection_id);
        }
    }

    fn cleanup_old_tombstones(&mut self) {
        let cutoff = tokio::time::Instant::now() - Duration::from_secs(3600); // 1 hour
        self.state
            .tombstoned
            .retain(|_, timestamp| *timestamp > cutoff);
    }
}

pub type RoomHandle = actor::ActorHandle<RoomMessageSet>;
