use std::{collections::HashMap, time::Duration};

use crate::{
    control::room::Room,
    entity::{ParticipantId, RoomId},
};
use futures_lite::StreamExt;
use tokio_util::time::DelayQueue;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ParticipantMeta {
    pub shard_id: usize,
    pub room_id: RoomId,
}

pub struct RoomRegistry {
    sweeper: DelayQueue<RoomId>,
    rooms: HashMap<RoomId, Room>,
    participants: HashMap<ParticipantId, ParticipantMeta>,
}

impl RoomRegistry {
    pub fn new() -> Self {
        Self {
            sweeper: DelayQueue::with_capacity(1024),
            rooms: HashMap::new(),
            participants: HashMap::new(),
        }
    }

    pub fn get_room(&self, room_id: &RoomId) -> Option<&Room> {
        self.rooms.get(room_id)
    }

    pub fn get_or_create_room(&mut self, room_id: RoomId) -> &Room {
        self.rooms
            .entry(room_id)
            .or_insert_with(|| Room::new(room_id))
    }

    pub fn room_mut_for(&mut self, participant_id: &ParticipantId) -> Option<&mut Room> {
        let meta = self.participants.get(participant_id).or_else(|| {
            tracing::warn!(%participant_id, "participant not found in reigstry, dropping");
            None
        })?;
        self.rooms.get_mut(&meta.room_id).or_else(|| {
            tracing::warn!(%participant_id, room = %meta.room_id, "room not found in registry, dropping");
            None
        })
    }

    pub fn add_participant(
        &mut self,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard_id: usize,
    ) {
        let room = self
            .rooms
            .entry(room_id)
            .or_insert_with(|| Room::new(room_id));
        room.add_participant(&participant_id);
        self.participants
            .insert(participant_id, ParticipantMeta { shard_id, room_id });
    }

    pub fn get_participant(&self, participant_id: &ParticipantId) -> Option<&ParticipantMeta> {
        self.participants.get(participant_id)
    }

    /// Returns the shard_id that was hosting the participant, if found.
    pub fn remove_participant(&mut self, participant_id: &ParticipantId) -> Option<usize> {
        let meta = self.participants.remove(participant_id)?;
        if let Some(room) = self.rooms.get_mut(&meta.room_id) {
            room.remove_participant(participant_id);
            if room.participant_count() == 0 {
                self.sweeper.insert(meta.room_id, EMPTY_ROOM_TIMEOUT);
            }
        }
        Some(meta.shard_id)
    }

    pub async fn next_expired(&mut self) {
        // DelayQueue returns Poll::Ready(None) immediately when empty, which
        // would cause the select! caller to spin at 100% CPU. Park forever
        // when there is nothing scheduled.
        if self.sweeper.is_empty() {
            std::future::pending::<()>().await;
        }
        if let Some(entry) = self.sweeper.next().await {
            self.maybe_delete_room(entry.get_ref());
        }
    }

    fn maybe_delete_room(&mut self, room_id: &RoomId) {
        if let Some(room) = self.rooms.get(room_id)
            && room.participant_count() == 0
        {
            self.rooms.remove(room_id);
        }
    }

    pub fn room_exists(&self, room_id: &RoomId) -> bool {
        self.rooms.contains_key(room_id)
    }
}
