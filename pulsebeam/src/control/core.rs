use std::collections::VecDeque;

use crate::{
    control::{controller::ParticipantState, registry::RoomRegistry},
    entity::{ParticipantId, RoomId},
    id::ShardId,
    participant::ParticipantConfig,
    shard::worker::{ShardCommand, ShardEvent, ShardEventWrapper, Topology},
};
use str0m::Rtc;

/// How many participants of one room share a shard before the next join hashes
/// to a different one.
///
/// Co-locating a room is what keeps its media on one core: below this, fanout
/// is pointer-passing between participants on the same shard and no route,
/// envelope or cross-shard queue is involved at all. Above it a room spills,
/// and that spill is the only thing that produces cross-shard media — which is
/// why the simulator lowers it rather than needing rooms of seventeen.
pub const DEFAULT_ROOM_SHARD_SLOT: usize = 16;

#[derive(Debug)]
pub enum ControllerEvent {
    ShardCommandSent(ShardId, ShardCommand),
}

pub struct ControllerEventQueue {
    queue: VecDeque<ControllerEvent>,
    shard_count: usize,
}

impl ControllerEventQueue {
    pub fn new(shard_count: usize) -> Self {
        debug_assert!(shard_count > 0);
        Self {
            queue: VecDeque::with_capacity(1024),
            shard_count,
        }
    }

    pub fn push(&mut self, ev: ControllerEvent) {
        self.queue.push_back(ev);
    }

    pub fn pop(&mut self) -> Option<ControllerEvent> {
        self.queue.pop_front()
    }

    /// Send one command to every shard, built fresh per shard rather than
    /// cloned: the targeted variants own an `Rtc` or a socket and cannot be.
    pub fn broadcast(&mut self, mut build: impl FnMut() -> ShardCommand) {
        for index in 0..self.shard_count {
            let cmd = build();
            self.push(ControllerEvent::ShardCommandSent(ShardId::new(index), cmd));
        }
    }

    pub fn send(&mut self, shard_id: ShardId, cmd: ShardCommand) {
        self.push(ControllerEvent::ShardCommandSent(shard_id, cmd));
    }

    /// Relay a topology change to one shard, stamping who raised it. The
    /// controller adds nothing else — it only decides who hears about it.
    pub fn relay(&mut self, shard_id: ShardId, from_shard_id: ShardId, topology: Topology) {
        self.push(ControllerEvent::ShardCommandSent(
            shard_id,
            ShardCommand::Relay {
                from_shard_id,
                topology,
            },
        ));
    }
}

/// How a room's participants are spread over shards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomPlacement {
    /// Rendezvous-hash each `(room, slot)` key. Keeps a room's shard stable as
    /// the cluster resizes, which is what production wants.
    Hashed,
    /// Walk the shards in order, one slot each.
    ///
    /// For tests that must reach the cross-shard path. Hashing chooses
    /// independently per slot, so a small room lands on one shard often enough
    /// that a plan relying on it is not a test but a coin flip — and one that
    /// passes when it comes up co-located.
    RoundRobin,
}

pub struct ControllerCore {
    pub(crate) registry: RoomRegistry,
    room_shard_slot: usize,
    placement: RoomPlacement,
}

impl ControllerCore {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_placement(DEFAULT_ROOM_SHARD_SLOT, RoomPlacement::Hashed)
    }

    pub fn with_placement(room_shard_slot: usize, placement: RoomPlacement) -> Self {
        debug_assert!(room_shard_slot > 0);
        Self {
            registry: RoomRegistry::new(),
            room_shard_slot,
            placement,
        }
    }

    /// Which slot the next join of this room falls in, and how to place it.
    pub fn room_slot(&self, room_id: &RoomId) -> (usize, RoomPlacement) {
        let count = self
            .registry
            .get_room(room_id)
            .map(super::room::Room::participant_count)
            .unwrap_or_default();
        (
            count.checked_div(self.room_shard_slot).unwrap_or(0),
            self.placement,
        )
    }

    pub fn process_shard_event(&mut self, e: ShardEventWrapper, eq: &mut ControllerEventQueue) {
        match e.ev {
            // Route lifecycle is a control-plane transaction, handled by the
            // actor itself before this sees the event — it needs to await a
            // publication barrier, which this synchronous projection cannot.
            ShardEvent::RouteNeeded { .. } | ShardEvent::RouteReleased { .. } => {
                debug_assert!(false, "route events are intercepted by the controller actor");
            }
            ShardEvent::TrackPublished(track) => {
                let Some((room_id, other_participants)) = self.registry.add_track(track.clone())
                else {
                    return;
                };

                for shard_id in other_participants {
                    eq.send(shard_id, ShardCommand::PublishTrack(track.clone(), room_id));
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
                    eq.send(
                        shard_id,
                        ShardCommand::UnpublishTracks {
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
            // Every remaining topology change is relayed verbatim; the only
            // decision left is who hears it.
            ShardEvent::Relay(topology) => {
                let from = e.from_shard_id;
                match &topology {
                    Topology::TrackSubscribed { track, .. }
                    | Topology::TrackUnsubscribed { track, .. } => {
                        // Straight to the publisher's shard. For a subscribe this
                        // is the acknowledgement: only now may media flow.
                        eq.relay(track.shard_id, from, topology);
                    }
                    Topology::DataTopicSubscribed { room_id, .. }
                    | Topology::DataTopicUnsubscribed { room_id, .. }
                    | Topology::DataTopicPublished { room_id, .. }
                    | Topology::ReliableTopicSubscribed { room_id, .. }
                    | Topology::ReliableTopicUnsubscribed { room_id, .. }
                    | Topology::ReliableTopicPublished { room_id, .. } => {
                        let room_id = *room_id;
                        let Some(room) = self.registry.get_room(&room_id) else {
                            return;
                        };
                        let recipients: Vec<ShardId> = room.recipient_shard_ids(from).collect();
                        for shard_id in recipients {
                            eq.relay(shard_id, from, topology.clone());
                        }
                    }
                }
            }
        }
    }

    pub async fn next_expired(&mut self) {
        self.registry.next_expired().await;
    }

    pub fn create_participant(
        &mut self,
        rtc: Rtc,
        state: ParticipantState,
        shard_id: ShardId,
        transport: Option<crate::route::TransportHandle>,
    ) -> ParticipantConfig {
        let tracks = {
            let room = self.registry.get_or_create_room(state.room_id);
            room.tracks().cloned().collect()
        };
        self.registry
            .add_participant(state.participant_id, state.room_id, shard_id, transport);
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
        let track_ids: Vec<_> = tracks.iter().map(|t| t.meta.id).collect();
        let shard_id = meta.shard_id;
        let room_id = meta.room_id;

        if let Some(removed_shard_id) = self.registry.remove_participant(participant_id) {
            eq.send(
                removed_shard_id,
                ShardCommand::RemoveParticipant(*participant_id),
            );
        }
        eq.broadcast(|| ShardCommand::UnregisterParticipant {
            shard_id,
            room_id,
            participant_id: *participant_id,
        });
        if !tracks.is_empty() {
            eq.broadcast(|| ShardCommand::UnpublishTracks {
                room_id,
                origin: *participant_id,
                track_ids: track_ids.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::{
        entity::{ExternalRoomId, ParticipantId, RoomId, TrackKind},
        route::RouteId,
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
        let mut eq = ControllerEventQueue::new(4);
        let track = track_meta(pid(1), ShardId::new(7));

        core.process_shard_event(
            ShardEventWrapper {
                from_shard_id: ShardId::new(3),
                ev: ShardEvent::Relay(Topology::TrackSubscribed {
                    track: track.clone(),
                    route: RouteId::from_raw(0),
                    epoch: 0,
                }),
            },
            &mut eq,
        );

        let Some(ControllerEvent::ShardCommandSent(
            shard_id,
            ShardCommand::Relay {
                from_shard_id,
                topology,
            },
        )) = eq.pop()
        else {
            panic!("expected one relayed topology change");
        };

        assert_eq!(shard_id, track.shard_id);
        assert_eq!(from_shard_id, ShardId::new(3));
        assert!(matches!(
            topology,
            Topology::TrackSubscribed { track: routed, route, epoch }
                if routed == track && route == RouteId::from_raw(0) && epoch == 0
        ));
    }

    #[test]
    fn track_unsubscribed_routes_unsubscribe_command() {
        let mut core = ControllerCore::new();
        let mut eq = ControllerEventQueue::new(4);
        let track = track_meta(pid(2), ShardId::new(9));

        core.process_shard_event(
            ShardEventWrapper {
                from_shard_id: ShardId::new(4),
                ev: ShardEvent::Relay(Topology::TrackUnsubscribed {
                    track: track.clone(),
                    route: RouteId::from_raw(0),
                    epoch: 0,
                }),
            },
            &mut eq,
        );

        let Some(ControllerEvent::ShardCommandSent(
            shard_id,
            ShardCommand::Relay {
                from_shard_id,
                topology,
            },
        )) = eq.pop()
        else {
            panic!("expected one relayed topology change");
        };

        assert_eq!(shard_id, track.shard_id);
        assert_eq!(from_shard_id, ShardId::new(4));
        assert!(matches!(
            topology,
            Topology::TrackUnsubscribed { track: routed, .. } if routed == track
        ));
    }

    #[test]
    fn track_published_targets_existing_participant_shards_once() {
        let mut core = ControllerCore::new();
        let mut eq = ControllerEventQueue::new(4);
        let room = room_id(1);
        let publisher = pid(10);
        let subscriber_a = pid(11);
        let subscriber_b = pid(12);

        core.registry
            .add_participant(publisher, room, ShardId::new(0), None);
        core.registry
            .add_participant(subscriber_a, room, ShardId::new(2), None);
        core.registry
            .add_participant(subscriber_b, room, ShardId::new(2), None);

        let track = crate::track::Track {
            meta: TrackMeta {
                shard_id: ShardId::new(0),
                id: publisher.derive_track_id(TrackKind::Audio, "a"),
                origin: publisher,
            },
            layers: Vec::new(),
            reverse: None,
        };

        core.process_shard_event(
            ShardEventWrapper {
                from_shard_id: ShardId::new(0),
                ev: ShardEvent::TrackPublished(track.clone()),
            },
            &mut eq,
        );

        let Some(ControllerEvent::ShardCommandSent(shard_id, cmd)) = eq.pop() else {
            panic!("expected a publish command");
        };

        assert_eq!(shard_id, ShardId::new(2));
        assert!(
            matches!(cmd, ShardCommand::PublishTrack(routed, routed_room) if routed.meta.id == track.meta.id && routed_room == room)
        );
        assert!(eq.pop().is_none());
    }

    #[test]
    fn track_unpublished_targets_existing_participant_shards_once() {
        let mut core = ControllerCore::new();
        let mut eq = ControllerEventQueue::new(4);
        let room = room_id(4);
        let publisher = pid(40);
        let subscriber_a = pid(41);
        let subscriber_b = pid(42);

        core.registry
            .add_participant(publisher, room, ShardId::new(0), None);
        core.registry
            .add_participant(subscriber_a, room, ShardId::new(2), None);
        core.registry
            .add_participant(subscriber_b, room, ShardId::new(2), None);

        let track_id = publisher.derive_track_id(TrackKind::Audio, "a");
        let track = crate::track::Track {
            meta: TrackMeta {
                shard_id: ShardId::new(0),
                id: track_id,
                origin: publisher,
            },
            layers: Vec::new(),
            reverse: None,
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

        let Some(ControllerEvent::ShardCommandSent(shard_id, cmd)) = eq.pop() else {
            panic!("expected an unpublish command");
        };

        assert_eq!(shard_id, ShardId::new(2));
        assert!(matches!(
            cmd,
            ShardCommand::UnpublishTracks { room_id, origin, track_ids }
                if room_id == room && origin == publisher && track_ids == vec![track_id]
        ));
        assert!(eq.pop().is_none());
    }

    #[tokio::test]
    async fn delete_participant_broadcasts_scoped_unregister() {
        let mut core = ControllerCore::new();
        let mut eq = ControllerEventQueue::new(4);
        let room = room_id(2);
        let participant = pid(20);

        core.registry
            .add_participant(participant, room, ShardId::new(6), None);
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

        // A broadcast is one targeted command per shard now, so every shard
        // sees the same unregister.
        for expected in 0..4 {
            let Some(ControllerEvent::ShardCommandSent(
                to,
                ShardCommand::UnregisterParticipant {
                    shard_id,
                    room_id,
                    participant_id,
                },
            )) = eq.pop()
            else {
                panic!("expected an unregister for every shard");
            };
            assert_eq!(to, ShardId::new(expected));
            assert_eq!(shard_id, ShardId::new(6));
            assert_eq!(room_id, room);
            assert_eq!(participant_id, participant);
        }
    }

    #[test]
    fn track_published_targets_latest_subscriber_shard_after_move() {
        let mut core = ControllerCore::new();
        let mut eq = ControllerEventQueue::new(4);
        let room = room_id(3);
        let publisher = pid(30);
        let subscriber = pid(31);

        core.registry
            .add_participant(publisher, room, ShardId::new(0), None);
        core.registry
            .add_participant(subscriber, room, ShardId::new(1), None);
        core.registry
            .add_participant(subscriber, room, ShardId::new(2), None);

        let track = crate::track::Track {
            meta: TrackMeta {
                shard_id: ShardId::new(0),
                id: publisher.derive_track_id(TrackKind::Audio, "a"),
                origin: publisher,
            },
            layers: Vec::new(),
            reverse: None,
        };

        core.process_shard_event(
            ShardEventWrapper {
                from_shard_id: ShardId::new(0),
                ev: ShardEvent::TrackPublished(track.clone()),
            },
            &mut eq,
        );

        let Some(ControllerEvent::ShardCommandSent(shard_id, cmd)) = eq.pop() else {
            panic!("expected a publish command");
        };

        assert_eq!(shard_id, ShardId::new(2));
        assert!(
            matches!(cmd, ShardCommand::PublishTrack(routed, routed_room) if routed.meta.id == track.meta.id && routed_room == room)
        );
        assert!(eq.pop().is_none());
    }
}
