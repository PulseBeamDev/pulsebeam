use std::collections::VecDeque;

use crate::{
    control::{controller::ParticipantState, registry::RoomRegistry},
    entity::{ParticipantId, RoomId},
    id::ShardId,
    participant::ParticipantConfig,
    shard::worker::{ShardCommand, ShardEvent, ShardEventMessage},
};
use str0m::Rtc;

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
            queue: VecDeque::with_capacity(64),
            shard_count,
        }
    }

    pub fn push(&mut self, event: ControllerEvent) {
        self.queue.push_back(event);
    }

    pub fn pop(&mut self) -> Option<ControllerEvent> {
        self.queue.pop_front()
    }

    pub fn send(&mut self, shard_id: ShardId, command: ShardCommand) {
        debug_assert!(shard_id.index() < self.shard_count);
        self.push(ControllerEvent::ShardCommandSent(shard_id, command));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomPlacement {
    Hashed,
    RoundRobin,
}

pub struct ControllerCore {
    pub(crate) registry: RoomRegistry,
    room_shard_slot: usize,
    placement: RoomPlacement,
}

impl ControllerCore {
    pub fn with_placement(room_shard_slot: usize, placement: RoomPlacement) -> Self {
        debug_assert!(room_shard_slot > 0);
        Self {
            registry: RoomRegistry::new(),
            room_shard_slot,
            placement,
        }
    }

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

    pub fn process_shard_event(&mut self, event: ShardEventMessage) {
        match event.1 {
            ShardEvent::TrackPublished { track, .. } => {
                let _ = self.registry.add_track(*track);
            }
            ShardEvent::TrackUnpublished { origin, track_id } => {
                let _ = self.registry.remove_track(origin, track_id);
            }
            ShardEvent::ParticipantClosed {
                participant: participant_id,
            } => {
                self.delete_participant(&participant_id);
            }
            // Facts the controller records elsewhere, or that only the shard
            // acts on. Listed rather than wildcarded so a new event has to be
            // considered here.
            ShardEvent::TransportAuthenticated { .. }
            | ShardEvent::TrackSubscribed { .. }
            | ShardEvent::TrackUnsubscribed { .. }
            | ShardEvent::DataTopicPublished { .. }
            | ShardEvent::DataTopicUnpublished { .. }
            | ShardEvent::DataTopicSubscribed { .. }
            | ShardEvent::DataTopicUnsubscribed { .. }
            | ShardEvent::ReliableDataTopicPublished { .. }
            | ShardEvent::ReliableDataTopicUnpublished { .. }
            | ShardEvent::ReliableDataTopicSubscribed { .. }
            | ShardEvent::ReliableDataTopicUnsubscribed { .. } => {}
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
        let tracks = self
            .registry
            .get_or_create_room(state.room_id)
            .tracks()
            .cloned()
            .collect();
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

    pub fn delete_participant(&mut self, participant_id: &ParticipantId) {
        if self.registry.get_participant(participant_id).is_none() {
            return;
        }
        let _ = self.registry.remove_participant(participant_id);
    }
}
