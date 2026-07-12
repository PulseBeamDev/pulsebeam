use std::collections::VecDeque;

use crate::{
    control::{controller::ParticipantState, registry::RoomRegistry},
    entity::{ParticipantId, RoomId},
    id::ShardId,
    participant::ParticipantConfig,
    shard::worker::{ClusterCommand, ShardCommand, ShardEvent, ShardEventWrapper},
};
use str0m::Rtc;

/// Maximum participants allowed per "slot" before hashing to a new shard epoch.
const MAX_PARTICIPANTS_PER_SHARD_SLOT: usize = 16;

#[derive(Debug)]
pub enum ControllerEvent {
    ShardCommandBroadcasted(ClusterCommand),
    ShardCommandSent(ShardId, ShardCommand),
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

    pub fn send(&mut self, shard_id: ShardId, cmd: ShardCommand) {
        self.push(ControllerEvent::ShardCommandSent(shard_id, cmd));
    }

    pub fn send_cluster(&mut self, shard_id: ShardId, cmd: ClusterCommand) {
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

    pub fn process_shard_event(&mut self, e: ShardEventWrapper, eq: &mut ControllerEventQueue) {
        match e.ev {
            ShardEvent::TrackPublished(track) => {
                let Some((room_id, other_participants)) = self.registry.add_track(track.clone())
                else {
                    return;
                };

                for shard_id in other_participants {
                    eq.send_cluster(
                        shard_id,
                        ClusterCommand::PublishTrack(track.clone(), room_id),
                    );
                }
            }
            ShardEvent::TrackUnpublished { origin, track_id } => {
                let Some((room_id, other_participants)) =
                    self.registry.remove_track(origin, track_id)
                else {
                    return;
                };

                let track_ids = vec![track_id];
                for shard_id in other_participants {
                    eq.send_cluster(
                        shard_id,
                        ClusterCommand::UnpublishTracks {
                            room_id,
                            origin,
                            track_ids: track_ids.clone(),
                        },
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

            ShardEvent::TrackSubscribed(track) => {
                eq.send_cluster(
                    track.shard_id,
                    ClusterCommand::SubscribeTrack {
                        from_shard_id: e.from_shard_id,
                        track,
                    },
                );
            }
            ShardEvent::TrackUnsubscribed(track) => {
                eq.send_cluster(
                    track.shard_id,
                    ClusterCommand::UnsubscribeTrack {
                        from_shard_id: e.from_shard_id,
                        track,
                    },
                );
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
        shard_id: ShardId,
    ) -> ParticipantConfig {
        let tracks = {
            let room = self.registry.get_or_create_room(state.room_id);
            room.tracks().cloned().collect()
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
        let Some(meta) = self.registry.get_participant(participant_id) else {
            return;
        };

        let Some(room) = self.registry.get_room(&meta.room_id) else {
            return;
        };
        // Collect track IDs before removing from registry so we can notify all shards.
        let tracks: Vec<_> = room.tracks_published_by(participant_id);
        let track_ids = tracks.iter().map(|t| t.meta.id).collect();
        let shard_id = meta.shard_id;
        let room_id = meta.room_id;

        if let Some(removed_shard_id) = self.registry.remove_participant(participant_id) {
            eq.send(
                removed_shard_id,
                ShardCommand::RemoveParticipant(*participant_id),
            );
        }
        eq.broadcast(ClusterCommand::UnregisterParticipant {
            shard_id,
            room_id,
            participant_id: *participant_id,
        });
        if !tracks.is_empty() {
            eq.broadcast(ClusterCommand::UnpublishTracks {
                room_id,
                origin: *participant_id,
                track_ids,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        entity::{ExternalRoomId, ParticipantId, RoomId, TrackKind},
        shard::worker::ClusterCommand,
        track::TrackMeta,
    };

    fn pid(seed: u8) -> ParticipantId {
        ParticipantId::from_bytes([seed; 16])
    }

    fn room_id(seed: u8) -> RoomId {
        let external = ExternalRoomId::new(&format!("room-{seed}")).unwrap();
        RoomId::from_external(&external)
    }

    fn track_meta(origin: ParticipantId, shard_id: ShardId) -> TrackMeta {
        TrackMeta {
            shard_id,
            id: origin.derive_track_id(TrackKind::Video, "v"),
            origin,
        }
    }

    #[test]
    fn track_subscribed_routes_subscribe_command() {
        let mut core = ControllerCore::new();
        let mut eq = ControllerEventQueue::default();
        let track = track_meta(pid(1), ShardId::new(7));

        core.process_shard_event(
            ShardEventWrapper {
                from_shard_id: ShardId::new(3),
                ev: ShardEvent::TrackSubscribed(track.clone()),
            },
            &mut eq,
        );

        let Some(ControllerEvent::ShardCommandSent(shard_id, ShardCommand::Cluster(cmd))) =
            eq.pop()
        else {
            panic!("expected one cluster command");
        };

        assert_eq!(shard_id, track.shard_id);
        assert!(matches!(
            cmd,
            ClusterCommand::SubscribeTrack { from_shard_id, track: routed }
                if from_shard_id == ShardId::new(3) && routed == track
        ));
    }

    #[test]
    fn track_unsubscribed_routes_unsubscribe_command() {
        let mut core = ControllerCore::new();
        let mut eq = ControllerEventQueue::default();
        let track = track_meta(pid(2), ShardId::new(9));

        core.process_shard_event(
            ShardEventWrapper {
                from_shard_id: ShardId::new(4),
                ev: ShardEvent::TrackUnsubscribed(track.clone()),
            },
            &mut eq,
        );

        let Some(ControllerEvent::ShardCommandSent(shard_id, ShardCommand::Cluster(cmd))) =
            eq.pop()
        else {
            panic!("expected one cluster command");
        };

        assert_eq!(shard_id, track.shard_id);
        assert!(matches!(
            cmd,
            ClusterCommand::UnsubscribeTrack { from_shard_id, track: routed }
                if from_shard_id == ShardId::new(4) && routed == track
        ));
    }

    #[test]
    fn track_published_targets_existing_participant_shards_once() {
        let mut core = ControllerCore::new();
        let mut eq = ControllerEventQueue::default();
        let room = room_id(1);
        let publisher = pid(10);
        let subscriber_a = pid(11);
        let subscriber_b = pid(12);

        core.registry
            .add_participant(publisher, room, ShardId::new(0));
        core.registry
            .add_participant(subscriber_a, room, ShardId::new(2));
        core.registry
            .add_participant(subscriber_b, room, ShardId::new(2));

        let track = crate::track::Track {
            meta: TrackMeta {
                shard_id: ShardId::new(0),
                id: publisher.derive_track_id(TrackKind::Audio, "a"),
                origin: publisher,
            },
            layers: Vec::new(),
        };

        core.process_shard_event(
            ShardEventWrapper {
                from_shard_id: ShardId::new(0),
                ev: ShardEvent::TrackPublished(track.clone()),
            },
            &mut eq,
        );

        let Some(ControllerEvent::ShardCommandSent(shard_id, ShardCommand::Cluster(cmd))) =
            eq.pop()
        else {
            panic!("expected publish cluster command");
        };

        assert_eq!(shard_id, ShardId::new(2));
        assert!(
            matches!(cmd, ClusterCommand::PublishTrack(routed, routed_room) if routed.meta.id == track.meta.id && routed_room == room)
        );
        assert!(eq.pop().is_none());
    }

    #[test]
    fn track_unpublished_targets_existing_participant_shards_once() {
        let mut core = ControllerCore::new();
        let mut eq = ControllerEventQueue::default();
        let room = room_id(4);
        let publisher = pid(40);
        let subscriber_a = pid(41);
        let subscriber_b = pid(42);

        core.registry
            .add_participant(publisher, room, ShardId::new(0));
        core.registry
            .add_participant(subscriber_a, room, ShardId::new(2));
        core.registry
            .add_participant(subscriber_b, room, ShardId::new(2));

        let track_id = publisher.derive_track_id(TrackKind::Audio, "a");
        let track = crate::track::Track {
            meta: TrackMeta {
                shard_id: ShardId::new(0),
                id: track_id,
                origin: publisher,
            },
            layers: Vec::new(),
        };

        core.process_shard_event(
            ShardEventWrapper {
                from_shard_id: ShardId::new(0),
                ev: ShardEvent::TrackPublished(track),
            },
            &mut eq,
        );
        let _ = eq.pop();

        core.process_shard_event(
            ShardEventWrapper {
                from_shard_id: ShardId::new(0),
                ev: ShardEvent::TrackUnpublished {
                    origin: publisher,
                    track_id,
                },
            },
            &mut eq,
        );

        let Some(ControllerEvent::ShardCommandSent(shard_id, ShardCommand::Cluster(cmd))) =
            eq.pop()
        else {
            panic!("expected unpublish cluster command");
        };

        assert_eq!(shard_id, ShardId::new(2));
        assert!(matches!(
            cmd,
            ClusterCommand::UnpublishTracks { room_id, origin, track_ids }
                if room_id == room && origin == publisher && track_ids == vec![track_id]
        ));
        assert!(eq.pop().is_none());
    }

    #[tokio::test]
    async fn delete_participant_broadcasts_scoped_unregister() {
        let mut core = ControllerCore::new();
        let mut eq = ControllerEventQueue::default();
        let room = room_id(2);
        let participant = pid(20);

        core.registry
            .add_participant(participant, room, ShardId::new(6));
        core.delete_participant(&participant, &mut eq);

        let Some(ControllerEvent::ShardCommandSent(
            shard_id,
            ShardCommand::RemoveParticipant(removed),
        )) = eq.pop()
        else {
            panic!("expected local shard removal command");
        };
        assert_eq!(shard_id, ShardId::new(6));
        assert_eq!(removed, participant);

        let Some(ControllerEvent::ShardCommandBroadcasted(ClusterCommand::UnregisterParticipant {
            shard_id,
            room_id,
            participant_id,
        })) = eq.pop()
        else {
            panic!("expected scoped unregister broadcast");
        };
        assert_eq!(shard_id, ShardId::new(6));
        assert_eq!(room_id, room);
        assert_eq!(participant_id, participant);
    }

    #[test]
    fn track_published_targets_latest_subscriber_shard_after_move() {
        let mut core = ControllerCore::new();
        let mut eq = ControllerEventQueue::default();
        let room = room_id(3);
        let publisher = pid(30);
        let subscriber = pid(31);

        core.registry
            .add_participant(publisher, room, ShardId::new(0));
        core.registry
            .add_participant(subscriber, room, ShardId::new(1));
        core.registry
            .add_participant(subscriber, room, ShardId::new(2));

        let track = crate::track::Track {
            meta: TrackMeta {
                shard_id: ShardId::new(0),
                id: publisher.derive_track_id(TrackKind::Audio, "a"),
                origin: publisher,
            },
            layers: Vec::new(),
        };

        core.process_shard_event(
            ShardEventWrapper {
                from_shard_id: ShardId::new(0),
                ev: ShardEvent::TrackPublished(track.clone()),
            },
            &mut eq,
        );

        let Some(ControllerEvent::ShardCommandSent(shard_id, ShardCommand::Cluster(cmd))) =
            eq.pop()
        else {
            panic!("expected publish cluster command");
        };

        assert_eq!(shard_id, ShardId::new(2));
        assert!(
            matches!(cmd, ClusterCommand::PublishTrack(routed, routed_room) if routed.meta.id == track.meta.id && routed_room == room)
        );
        assert!(eq.pop().is_none());
    }
}
