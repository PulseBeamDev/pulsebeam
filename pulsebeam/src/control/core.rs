use std::collections::VecDeque;

use crate::{
    control::{controller::ParticipantState, registry::RoomRegistry},
    entity::{ParticipantId, RoomId},
    participant::ParticipantConfig,
    shard::worker::{ClusterCommand, ShardCommand, ShardEvent},
};
use indexmap::IndexMap;
use str0m::Rtc;

/// Maximum participants allowed per "slot" before hashing to a new shard epoch.
const MAX_PARTICIPANTS_PER_SHARD_SLOT: usize = 16;

pub enum ControllerEvent {
    ShardCommandBroadcasted(ClusterCommand),
    ShardCommandSent(usize, ShardCommand),
}

pub struct ControllerEventQueue {
    queue: VecDeque<ControllerEvent>,
}

impl ControllerEventQueue {
    pub fn default() -> Self {
        Self {
            queue: VecDeque::with_capacity(1024),
        }
    }

    pub fn push(&mut self, ev: ControllerEvent) {
        self.queue.push_back(ev);
    }

    pub fn pop(&mut self) -> Option<ControllerEvent> {
        self.queue.pop_front()
    }

    pub fn broadcast(&mut self, cmd: ClusterCommand) {
        self.push(ControllerEvent::ShardCommandBroadcasted(cmd));
    }

    pub fn send(&mut self, shard_id: usize, cmd: ShardCommand) {
        self.push(ControllerEvent::ShardCommandSent(shard_id, cmd));
    }

    pub fn send_cluster(&mut self, shard_id: usize, cmd: ClusterCommand) {
        self.push(ControllerEvent::ShardCommandSent(
            shard_id,
            ShardCommand::Cluster(cmd),
        ));
    }
}

pub struct ControllerCore {
    registry: RoomRegistry,
}

impl ControllerCore {
    pub fn new() -> Self {
        Self {
            registry: RoomRegistry::new(),
        }
    }

    pub fn process_shard_event(&mut self, ev: ShardEvent, eq: &mut ControllerEventQueue) {
        match ev {
            ShardEvent::TrackPublished(track) => {
                let origin = track.meta.origin;

                let (room_id, other_participants) = {
                    let Some(room) = self.registry.room_mut_for(&origin) else {
                        return;
                    };
                    room.publish_track(track.clone());

                    let ids: Vec<ParticipantId> = room
                        .participants_iter()
                        .filter(|&&id| id != origin)
                        .cloned()
                        .collect();

                    (room.room_id, ids)
                };

                // TODO: should we make room shard aware?
                let mut shard_ids: IndexMap<usize, ()> = IndexMap::new();
                for participant_id in other_participants {
                    if let Some(p) = self.registry.get_participant(&participant_id) {
                        shard_ids.entry(p.shard_id).or_default();
                    }
                }

                for (shard_id, _) in shard_ids {
                    eq.send_cluster(
                        shard_id,
                        ClusterCommand::PublishTrack(track.clone(), room_id),
                    );
                }
            }

            ShardEvent::ParticipantExited(participant_id) => {
                self.delete_participant(&participant_id, eq);
            }
            ShardEvent::KeyframeRequest(req) => {
                let Some(meta) = self.registry.get_participant(&req.origin) else {
                    tracing::warn!(origin = %req.origin, track = ?req.stream_id.0, "KeyframeRequest: origin participant not found in controller");
                    return;
                };
                eq.send_cluster(meta.shard_id, ClusterCommand::RequestKeyframe(req))
            }
        }
    }

    pub async fn next_expired(&mut self) {
        self.registry.next_expired().await;
    }

    pub fn routing_key(&self, room_id: &RoomId) -> String {
        let count = self
            .registry
            .get_room(room_id)
            .map(|r| r.participant_count())
            .unwrap_or_default();
        let epoch = count / MAX_PARTICIPANTS_PER_SHARD_SLOT;
        format!("{}-{}", room_id, epoch)
    }

    pub fn create_participant(
        &mut self,
        rtc: Rtc,
        state: ParticipantState,
        shard_id: usize,
    ) -> ParticipantConfig {
        let tracks = {
            let room = self.registry.get_or_create_room(state.room_id);
            room.tracks_for(&state.participant_id).cloned().collect()
        };
        self.registry
            .add_participant(state.participant_id, state.room_id, shard_id);
        ParticipantConfig {
            manual_sub: state.manual_sub,
            room_id: state.room_id,
            participant_id: state.participant_id,
            rtc,
            available_tracks: tracks,
        }
    }

    pub fn delete_participant(
        &mut self,
        participant_id: &ParticipantId,
        eq: &mut ControllerEventQueue,
    ) {
        // Collect track IDs before removing from registry so we can notify all shards.
        let track_ids: Vec<_> = self
            .registry
            .get_participant(participant_id)
            .and_then(|meta| self.registry.get_room(&meta.room_id))
            .map(|room| {
                room.tracks_published_by(participant_id)
                    .into_iter()
                    .map(|t| t.meta.id)
                    .collect()
            })
            .unwrap_or_default();

        self.registry.remove_participant(participant_id);
        eq.broadcast(ClusterCommand::UnregisterParticipant {
            participant_id: *participant_id,
        });
        if !track_ids.is_empty() {
            eq.broadcast(ClusterCommand::UnpublishTracks {
                origin: *participant_id,
                track_ids,
            });
        }
    }
}
