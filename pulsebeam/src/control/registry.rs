use std::{collections::HashMap, time::Duration};

use crate::{
    control::room::Room,
    entity::{ConnectionId, ParticipantId, RoomId, TrackId},
    id::ShardId,
    track::Track,
};
use futures_lite::StreamExt;
use tokio_util::time::DelayQueue;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ParticipantMeta {
    pub shard_id: ShardId,
    pub room_id: RoomId,
    pub connection_id: ConnectionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticipantRegistrationError {
    Collision(ParticipantId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackRegistrationError {
    Collision(TrackId),
}

pub struct RoomRegistry {
    sweeper: DelayQueue<RoomId>,
    rooms: HashMap<RoomId, Room>,
    participants: HashMap<ParticipantId, ParticipantMeta>,
    tracks: HashMap<TrackId, (RoomId, ParticipantId, ShardId)>,
}

impl RoomRegistry {
    pub fn new() -> Self {
        Self {
            sweeper: DelayQueue::with_capacity(1024),
            rooms: HashMap::new(),
            participants: HashMap::new(),
            tracks: HashMap::new(),
        }
    }

    pub fn get_room(&self, room_id: &RoomId) -> Option<&Room> {
        self.rooms.get(room_id)
    }

    pub fn get_or_create_room(&mut self, room_id: RoomId) -> &Room {
        let room = self
            .rooms
            .entry(room_id)
            .or_insert_with(|| Room::new(room_id));
        debug_assert_eq!(room.room_id, room_id);
        room
    }

    #[cfg(test)]
    pub fn add_participant(
        &mut self,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard_id: ShardId,
    ) {
        let _ = self.add_participant_with_connection(
            participant_id,
            room_id,
            shard_id,
            ConnectionId::MIN,
        );
    }

    pub fn add_participant_with_connection(
        &mut self,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard_id: ShardId,
        connection_id: ConnectionId,
    ) -> Result<(), ParticipantRegistrationError> {
        if self.participants.contains_key(&participant_id) {
            tracing::warn!(%participant_id, "duplicate participant registry insertion");
            return Err(ParticipantRegistrationError::Collision(participant_id));
        }

        let room = self
            .rooms
            .entry(room_id)
            .or_insert_with(|| Room::new(room_id));
        debug_assert_eq!(room.room_id, room_id);
        let inserted = room.add_participant(&participant_id, shard_id);
        debug_assert!(inserted);
        if !inserted {
            return Err(ParticipantRegistrationError::Collision(participant_id));
        }
        self.participants.insert(
            participant_id,
            ParticipantMeta {
                shard_id,
                room_id,
                connection_id,
            },
        );
        debug_assert_eq!(
            self.participants.get(&participant_id).map(|meta| (
                meta.room_id,
                meta.shard_id,
                meta.connection_id
            )),
            Some((room_id, shard_id, connection_id))
        );
        Ok(())
    }

    pub fn get_participant(&self, participant_id: &ParticipantId) -> Option<&ParticipantMeta> {
        self.participants.get(participant_id)
    }

    /// Returns the shard_id that was hosting the participant, if found.
    pub fn remove_participant(&mut self, participant_id: &ParticipantId) -> Option<ShardId> {
        let meta = self.participants.remove(participant_id)?;
        if let Some(room) = self.rooms.get_mut(&meta.room_id) {
            let removed = room.remove_participant(participant_id, meta.shard_id);
            debug_assert!(removed);
            if room.participant_count() == 0 {
                self.sweeper.insert(meta.room_id, EMPTY_ROOM_TIMEOUT);
            }
        }
        self.tracks
            .retain(|_, (_, origin, _)| origin != participant_id);
        Some(meta.shard_id)
    }

    pub fn add_track(
        &mut self,
        track: Track,
    ) -> Result<Option<(RoomId, Vec<ShardId>)>, TrackRegistrationError> {
        let Some(participant) = self.participants.get(&track.meta.origin) else {
            tracing::warn!(origin = %track.meta.origin, "track publisher not found in registry, dropping");
            return Ok(None);
        };
        let room_id = participant.room_id;
        let origin_shard = participant.shard_id;
        if track.meta.shard_id != origin_shard {
            tracing::error!(
                track = %track.meta.id,
                origin = %track.meta.origin,
                expected_shard = %origin_shard,
                track_shard = %track.meta.shard_id,
                "track publication has unexpected owner shard"
            );
            return Err(TrackRegistrationError::Collision(track.meta.id));
        }
        let owner = (room_id, track.meta.origin, track.meta.shard_id);
        if let Some(existing) = self.tracks.get(&track.meta.id).copied() {
            if existing != owner {
                tracing::error!(track = %track.meta.id, origin = %track.meta.origin, "track id collision in controller registry");
                return Err(TrackRegistrationError::Collision(track.meta.id));
            }
            return Ok(None);
        }
        let Some(room) = self.rooms.get_mut(&room_id) else {
            tracing::warn!(%room_id, "track publisher room not found in registry, dropping");
            return Ok(None);
        };
        if room.add_track(track.clone()).is_err() {
            return Err(TrackRegistrationError::Collision(track.meta.id));
        }
        self.tracks.insert(track.meta.id, owner);
        debug_assert_eq!(self.tracks.get(&track.meta.id), Some(&owner));
        let ids = room.recipient_shard_ids(origin_shard).collect();
        let room_id = room.room_id;

        Ok(Some((room_id, ids)))
    }

    pub fn remove_track(
        &mut self,
        origin: ParticipantId,
        track_id: TrackId,
    ) -> Option<(RoomId, Vec<ShardId>)> {
        let participant = self.participants.get(&origin)?;
        let room_id = participant.room_id;
        let origin_shard = participant.shard_id;
        if let Some((owner_room, owner, _)) = self.tracks.get(&track_id)
            && (*owner_room != room_id || *owner != origin)
        {
            tracing::warn!(track = %track_id, %origin, "track unpublish owner mismatch");
            return None;
        }
        let room = self.rooms.get_mut(&room_id)?;
        if !room.remove_track(&origin, &track_id) {
            return None;
        }

        self.tracks.remove(&track_id);

        let ids = room.recipient_shard_ids(origin_shard).collect();
        let room_id = room.room_id;

        Some((room_id, ids))
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
    use crate::entity::{ExternalRoomId, TrackKind};
    use crate::track::TrackMeta;
    use pulsebeam_runtime::rand::seeded_rng;

    fn room_id(s: &str) -> RoomId {
        RoomId::from_external(&ExternalRoomId::new(s).unwrap())
    }

    fn participant_id() -> ParticipantId {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        ParticipantId::new(&mut seeded_rng(COUNTER.fetch_add(1, Ordering::Relaxed)))
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

        reg.add_participant(pid, rid, ShardId::new(0));

        reg.get_room(&rid).unwrap();
        let meta = reg.get_participant(&pid).expect("participant should exist");
        assert_eq!(meta.room_id, rid);
        assert_eq!(meta.shard_id, ShardId::new(0));
    }

    #[test]
    fn add_participant_increments_room_count() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-b");
        let pid1 = participant_id();
        let pid2 = participant_id();

        reg.add_participant(pid1, rid, ShardId::new(0));
        reg.add_participant(pid2, rid, ShardId::new(1));

        let room = reg.get_room(&rid).unwrap();
        assert_eq!(room.participant_count(), 2);
    }

    #[test]
    fn conflicting_track_id_does_not_replace_existing_route() {
        let mut reg = RoomRegistry::new();
        let room = room_id("track-collision");
        let first = participant_id();
        let second = participant_id();
        let track_id = first.derive_track_id(TrackKind::Video, "camera");

        reg.add_participant(first, room, ShardId::new(0));
        reg.add_participant(second, room, ShardId::new(1));

        let first_track = Track {
            meta: TrackMeta {
                shard_id: ShardId::new(0),
                id: track_id,
                origin: first,
            },
            layers: Vec::new(),
        };
        let conflicting_track = Track {
            meta: TrackMeta {
                shard_id: ShardId::new(1),
                id: track_id,
                origin: second,
            },
            layers: Vec::new(),
        };

        assert!(reg.add_track(first_track).unwrap().is_some());
        assert!(matches!(
            reg.add_track(conflicting_track),
            Err(TrackRegistrationError::Collision(id)) if id == track_id
        ));
        assert_eq!(
            reg.get_room(&room)
                .unwrap()
                .tracks_published_by(&first)
                .len(),
            1
        );
        assert!(
            reg.get_room(&room)
                .unwrap()
                .tracks_published_by(&second)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn add_participant_rejects_existing_participant_in_new_room() {
        let mut reg = RoomRegistry::new();
        let old_room = room_id("room-b-old");
        let new_room = room_id("room-b-new");
        let pid = participant_id();

        reg.add_participant(pid, old_room, ShardId::new(0));
        assert!(
            reg.add_participant_with_connection(pid, new_room, ShardId::new(1), ConnectionId::MIN)
                .is_err()
        );

        assert_eq!(reg.get_room(&old_room).unwrap().participant_count(), 1);
        assert!(reg.get_room(&new_room).is_none());
        let meta = reg.get_participant(&pid).unwrap();
        assert_eq!(meta.room_id, old_room);
        assert_eq!(meta.shard_id, ShardId::new(0));
    }

    #[tokio::test]
    async fn remove_participant_returns_shard_id() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-c");
        let pid = participant_id();

        reg.add_participant(pid, rid, ShardId::new(3));
        let shard = reg.remove_participant(&pid);

        assert_eq!(shard, Some(ShardId::new(3)));
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

        reg.add_participant(pid, rid, ShardId::new(0));
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

        reg.add_participant(pid, rid, ShardId::new(0));
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

        reg.add_participant(pid1, rid, ShardId::new(0));
        reg.remove_participant(&pid1);

        // A new participant joins before the sweeper fires.
        reg.add_participant(pid2, rid, ShardId::new(1));

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

        reg.add_participant(pid, rid, ShardId::new(0));
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

        reg.add_participant(pid1, rid1, ShardId::new(0));
        reg.add_participant(pid2, rid2, ShardId::new(1));
        reg.remove_participant(&pid1);
        reg.maybe_delete_room(&rid1);

        assert!(reg.get_room(&rid1).is_none());
        assert!(reg.get_room(&rid2).is_some());
    }
}
