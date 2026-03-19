use ahash::{HashMap, HashMapExt};
use pulsebeam_runtime::sync::Arc;
use std::{collections::BTreeMap, time::Duration};

use pulsebeam_runtime::{
    actor::{ActorKind},
    prelude::*,
};

use crate::{
    audio_selector::{self, AudioSelectorHandle},
    entity::{ConnectionId, ParticipantId, RoomId, TrackId},
    gateway::GatewayControlMessage,
    node,
    participant::{self},
    track::{self},
};
use pulsebeam_runtime::actor;
use str0m::media::MediaKind;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(derive_more::From)]
pub enum RoomMessage {
    PublishTrack(track::TrackReceiver),
    AddParticipant(AddParticipant),
    RemoveParticipant(RemoveParticipant),
}

pub struct AddParticipant {
    pub participant_id: ParticipantId,
    pub ufrag: String,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
}

pub struct RemoveParticipant {
    pub participant_id: ParticipantId,
}

#[derive(Clone, Debug)]
pub struct ParticipantMeta {
    ufrag: String,
    tracks: HashMap<TrackId, track::TrackReceiver>,
    _connection_id: ConnectionId,
}

pub struct RoomMessageSet;

impl actor::MessageSet for RoomMessageSet {
    type Meta = RoomId;
    type Msg = RoomMessage;
    type ObservableState = RoomState;
}

pub struct RoomActor {
    _node_ctx: node::NodeContext,
    room_id: RoomId,
    state: RoomState,
    gateway: crate::gateway::GatewayHandle,
    /// Room-level Top-N audio selector.  Holds the command sender; dropping
    /// this field shuts down the background selector task.
    audio_selector: AudioSelectorHandle,
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
                        m.participant_id,
                        m.ufrag.clone(),
                        old_connection_id,
                        m.connection_id,
                    )
                    .await;
                } else {
                    self.handle_participant_joined(ctx, m.participant_id, m.ufrag.clone(), m.connection_id).await;
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
        let (audio_selector, selector_task) = audio_selector::create(64);
        tokio::task::spawn_local(selector_task);
        let gateway_handle = node_ctx.gateway.clone();
        Self {
            _node_ctx: node_ctx,
            room_id,
            state: RoomState::default(),
            gateway: gateway_handle,
            audio_selector,
        }
    }

    async fn handle_participant_joined(
        &mut self,
        _ctx: &mut actor::ActorContext<RoomMessageSet>,
        participant_id: ParticipantId,
        ufrag: String,
        connection_id: ConnectionId,
    ) {
        self.state.participants.insert(
            (participant_id, connection_id),
            ParticipantMeta {
                ufrag: ufrag.clone(),
                tracks: HashMap::new(),
                _connection_id: connection_id,
            },
        );

        let _ = self
            .gateway
            .send(GatewayControlMessage::ParticipantControl(
                participant_id,
                participant::ParticipantControlMessage::TracksSnapshot(self.state.tracks.clone()),
            ))
            .await;

        let sub = self.audio_selector.subscribe();
        let _ = self
            .gateway
            .send(GatewayControlMessage::ParticipantControl(
                participant_id,
                participant::ParticipantControlMessage::AudioSubscription(sub),
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

        let Some(participant_meta) = self.state.participants.remove(&key) else {
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

        // Notify gateway to cleanup participant state
        let _ = self
            .gateway
            .send(GatewayControlMessage::RemoveParticipant(participant_id))
            .await;

        // Remove tracks published by this connection.
        for (track_id, track) in participant_meta.tracks.iter() {
            self.state.tracks.remove(track_id);
            if track.meta.kind == MediaKind::Audio {
                let _ = self
                    .audio_selector
                    .cmd_tx
                    .try_send(audio_selector::AudioSelectorCmd::RemoveTrack(*track_id));
            }
        }

        let tracks = Arc::new(participant_meta.tracks.clone());
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

        // Forward audio tracks to the room-level selector.
        if track_handle.meta.kind == MediaKind::Audio {
            let _ =
                self.audio_selector
                    .cmd_tx
                    .try_send(audio_selector::AudioSelectorCmd::AddTrack(
                        track_handle.clone(),
                    ));
        }

        // Broadcast new track to all participants.
        let mut new_tracks = HashMap::new();
        new_tracks.insert(track_id, track_handle.clone());
        let new_tracks = Arc::new(new_tracks);
        let msg = participant::ParticipantControlMessage::TracksPublished(new_tracks);
        self.broadcast_message(msg).await;
    }

    async fn handle_replace_participant(
        &mut self,
        ctx: &mut actor::ActorContext<RoomMessageSet>,
        new_participant_id: ParticipantId,
        new_ufrag: String,
        old_connection_id: ConnectionId,
        new_connection_id: ConnectionId,
    ) {
        tracing::info!(
            ?new_participant_id,
            ?old_connection_id,
            ?new_connection_id,
            "Replacing participant connection"
        );

        // Evict old connection
        self.handle_participant_left(new_participant_id, old_connection_id)
            .await;

        // Add new connection
        self.handle_participant_joined(ctx, new_participant_id, new_ufrag, new_connection_id)
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
        for ((participant_id, _), _participant_meta) in self.state.participants.iter() {
            let _ = self
                .gateway
                .send(GatewayControlMessage::ParticipantControl(
                    *participant_id,
                    msg.clone(),
                ))
                .await;
        }
    }
}

pub type RoomHandle = actor::ActorHandle<RoomMessageSet>;
