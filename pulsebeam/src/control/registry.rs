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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::ExternalRoomId;

    fn room_id(s: &str) -> RoomId {
        RoomId::from_external(&ExternalRoomId::new(s).unwrap())
    }

    fn participant_id() -> ParticipantId {
        ParticipantId::new()
    }

    #[test]
    fn get_or_create_room_creates_new_room() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("test-room");
        reg.get_or_create_room(rid);
        reg.get_room(&rid).unwrap();
    }

    #[test]
    fn get_or_create_room_is_idempotent() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("test-room");
        reg.get_or_create_room(rid);
        reg.get_or_create_room(rid);
        assert_eq!(reg.rooms.len(), 1);
    }

    #[test]
    fn add_participant_creates_room_and_entry() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-a");
        let pid = participant_id();

        reg.add_participant(pid, rid, 0);

        reg.get_room(&rid).unwrap();
        let meta = reg.get_participant(&pid).expect("participant should exist");
        assert_eq!(meta.room_id, rid);
        assert_eq!(meta.shard_id, 0);
    }

    #[test]
    fn add_participant_increments_room_count() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-b");
        let pid1 = participant_id();
        let pid2 = participant_id();

        reg.add_participant(pid1, rid, 0);
        reg.add_participant(pid2, rid, 1);

        let room = reg.get_room(&rid).unwrap();
        assert_eq!(room.participant_count(), 2);
    }

    #[tokio::test]
    async fn remove_participant_returns_shard_id() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-c");
        let pid = participant_id();

        reg.add_participant(pid, rid, 3);
        let shard = reg.remove_participant(&pid);

        assert_eq!(shard, Some(3));
    }

    #[test]
    fn remove_unknown_participant_returns_none() {
        let mut reg = RoomRegistry::new();
        let pid = participant_id();
        assert!(reg.remove_participant(&pid).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn room_not_immediately_deleted_after_last_participant_leaves() {
        // The room should remain until the sweeper fires, not be deleted inline.
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-d");
        let pid = participant_id();

        reg.add_participant(pid, rid, 0);
        reg.remove_participant(&pid);

        // Room still present; deletion is deferred via the sweeper.
        reg.get_room(&rid).unwrap();
        // But the sweeper has one pending entry.
        assert!(!reg.sweeper.is_empty());
        let e = reg.sweeper.next().await.unwrap();
        assert_eq!(*e.get_ref(), rid);
    }

    #[tokio::test]
    async fn maybe_delete_room_removes_empty_room() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-e");
        let pid = participant_id();

        reg.add_participant(pid, rid, 0);
        reg.remove_participant(&pid);

        // Simulate the sweeper firing.
        reg.maybe_delete_room(&rid);

        assert!(reg.get_room(&rid).is_none());
    }

    #[tokio::test]
    async fn maybe_delete_room_keeps_room_if_participant_rejoined() {
        // If a participant re-joins between the remove and the sweeper firing,
        // the room must NOT be deleted.
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-f");
        let pid1 = participant_id();
        let pid2 = participant_id();

        reg.add_participant(pid1, rid, 0);
        reg.remove_participant(&pid1);

        // A new participant joins before the sweeper fires.
        reg.add_participant(pid2, rid, 1);

        // Sweeper fires — room should survive because it is not empty.
        reg.maybe_delete_room(&rid);

        reg.get_room(&rid).unwrap();
        assert_eq!(reg.get_room(&rid).unwrap().participant_count(), 1);
    }

    #[tokio::test]
    async fn participant_removed_from_registry_after_remove() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-h");
        let pid = participant_id();

        reg.add_participant(pid, rid, 0);
        reg.remove_participant(&pid);

        assert!(reg.get_participant(&pid).is_none());
    }

    #[tokio::test]
    async fn multiple_rooms_are_independent() {
        let mut reg = RoomRegistry::new();
        let rid1 = room_id("room-x");
        let rid2 = room_id("room-y");
        let pid1 = participant_id();
        let pid2 = participant_id();

        reg.add_participant(pid1, rid1, 0);
        reg.add_participant(pid2, rid2, 1);
        reg.remove_participant(&pid1);
        reg.maybe_delete_room(&rid1);

        assert!(reg.get_room(&rid1).is_none());
        assert!(reg.get_room(&rid2).is_some());
    }
}
