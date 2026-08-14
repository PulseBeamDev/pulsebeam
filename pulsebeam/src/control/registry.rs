use std::{collections::HashMap, time::Duration};

use crate::{
    control::room::Room,
    entity::{ParticipantId, RoomId, TrackId},
    id::ShardId,
    route::TransportHandle,
    shard::participants::ParticipantKey,
    track::Track,
};
use futures_lite::StreamExt;
use tokio_util::time::DelayQueue;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything the control plane knows about one participant.
///
/// The single owner of `participant -> (shard, room)`. It used to be three
/// indexes — this registry, the lifecycle state, and a copy on every shard —
/// which is three chances for them to disagree about where somebody is.
pub struct ParticipantMeta {
    pub shard_id: ShardId,
    pub room_id: RoomId,
    /// The client's ICE association, kept so teardown can retire it. The
    /// route outlives the negotiation that produced it, so something has to
    /// remember it, and this is the record that already knows who it belongs
    /// to.
    pub transport: Option<TransportHandle>,
    /// The owning shard's own arena key, opaque here. Stored only long enough
    /// to compile into that shard's view; never dereferenced.
    pub binding: Option<ParticipantKey>,
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
        shard_id: ShardId,
        transport: Option<TransportHandle>,
    ) {
        let binding = self
            .participants
            .get(&participant_id)
            .and_then(|meta| meta.binding);
        if let Some(previous) = self.participants.insert(
            participant_id,
            ParticipantMeta {
                shard_id,
                room_id,
                transport,
                binding,
            },
        ) && let Some(room) = self.rooms.get_mut(&previous.room_id)
        {
            room.remove_participant(&participant_id, previous.shard_id);
            if room.participant_count() == 0 {
                self.sweeper.insert(previous.room_id, EMPTY_ROOM_TIMEOUT);
            }
        }
        let room = self
            .rooms
            .entry(room_id)
            .or_insert_with(|| Room::new(room_id));
        room.add_participant(&participant_id, shard_id);
    }

    pub fn get_participant(&self, participant_id: &ParticipantId) -> Option<&ParticipantMeta> {
        self.participants.get(participant_id)
    }

    pub fn participants_in_room(
        &self,
        room_id: &RoomId,
    ) -> Vec<(ParticipantId, ShardId, Option<ParticipantKey>)> {
        let Some(room) = self.rooms.get(room_id) else {
            return Vec::new();
        };
        room.participant_ids()
            .filter_map(|participant| {
                let meta = self.participants.get(participant)?;
                Some((*participant, meta.shard_id, meta.binding))
            })
            .collect()
    }

    /// Record the arena key the owning shard reported for this participant.
    /// Idempotent: a retry after a lost acknowledgement must not create a
    /// second binding.
    pub fn bind_participant(&mut self, participant_id: &ParticipantId, binding: ParticipantKey) {
        let Some(meta) = self.participants.get_mut(participant_id) else {
            return;
        };
        if let Some(existing) = meta.binding {
            debug_assert_eq!(
                existing, binding,
                "a repeated prepare must report the same binding"
            );
            return;
        }
        meta.binding = Some(binding);
    }

    /// The transport route to retire when this participant goes away.
    pub fn transport_of(
        &self,
        participant_id: &ParticipantId,
    ) -> Option<(ShardId, TransportHandle)> {
        let meta = self.participants.get(participant_id)?;
        Some((meta.shard_id, meta.transport?))
    }

    /// Returns the shard_id that was hosting the participant, if found.
    pub fn remove_participant(&mut self, participant_id: &ParticipantId) -> Option<ShardId> {
        let meta = self.participants.remove(participant_id)?;
        if let Some(room) = self.rooms.get_mut(&meta.room_id) {
            room.remove_participant(participant_id, meta.shard_id);
            if room.participant_count() == 0 {
                self.sweeper.insert(meta.room_id, EMPTY_ROOM_TIMEOUT);
            }
        }
        Some(meta.shard_id)
    }

    pub fn add_track(&mut self, track: Track) -> Option<(RoomId, Vec<ShardId>)> {
        let origin_shard = self.participants.get(&track.meta.origin)?.shard_id;
        let room = self.room_mut_for(&track.meta.origin)?;
        room.add_track(track.clone());
        let ids = room.recipient_shard_ids(origin_shard).collect();
        let room_id = room.room_id;

        Some((room_id, ids))
    }

    pub fn remove_track(
        &mut self,
        origin: ParticipantId,
        track_id: TrackId,
    ) -> Option<(RoomId, Vec<ShardId>)> {
        let origin_shard = self.participants.get(&origin)?.shard_id;
        let room = self.room_mut_for(&origin)?;
        if !room.remove_track(&origin, &track_id) {
            return None;
        }

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
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::entity::ExternalRoomId;
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

        reg.add_participant(pid, rid, ShardId::new(0), None);

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

        reg.add_participant(pid1, rid, ShardId::new(0), None);
        reg.add_participant(pid2, rid, ShardId::new(1), None);

        let room = reg.get_room(&rid).unwrap();
        assert_eq!(room.participant_count(), 2);
    }

    #[tokio::test]
    async fn add_participant_moves_existing_participant_to_new_room() {
        let mut reg = RoomRegistry::new();
        let old_room = room_id("room-b-old");
        let new_room = room_id("room-b-new");
        let pid = participant_id();

        reg.add_participant(pid, old_room, ShardId::new(0), None);
        reg.add_participant(pid, new_room, ShardId::new(1), None);

        assert_eq!(reg.get_room(&old_room).unwrap().participant_count(), 0);
        assert_eq!(reg.get_room(&new_room).unwrap().participant_count(), 1);
        let meta = reg.get_participant(&pid).unwrap();
        assert_eq!(meta.room_id, new_room);
        assert_eq!(meta.shard_id, ShardId::new(1));
    }

    #[tokio::test]
    async fn remove_participant_returns_shard_id() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-c");
        let pid = participant_id();

        reg.add_participant(pid, rid, ShardId::new(3), None);
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

        reg.add_participant(pid, rid, ShardId::new(0), None);
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

        reg.add_participant(pid, rid, ShardId::new(0), None);
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

        reg.add_participant(pid1, rid, ShardId::new(0), None);
        reg.remove_participant(&pid1);

        // A new participant joins before the sweeper fires.
        reg.add_participant(pid2, rid, ShardId::new(1), None);

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

        reg.add_participant(pid, rid, ShardId::new(0), None);
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

        reg.add_participant(pid1, rid1, ShardId::new(0), None);
        reg.add_participant(pid2, rid2, ShardId::new(1), None);
        reg.remove_participant(&pid1);
        reg.maybe_delete_room(&rid1);

        assert!(reg.get_room(&rid1).is_none());
        assert!(reg.get_room(&rid2).is_some());
    }

    /// The registry is the one place that knows where a participant is, and
    /// that has to include the transport route its teardown needs. It used to
    /// be split across three indexes — this one, the lifecycle state, and a
    /// copy on every shard — so "where is Alice" had three answers that could
    /// drift apart.
    #[tokio::test]
    async fn the_registry_is_the_only_index_of_where_a_participant_is() {
        let mut reg = RoomRegistry::new();
        let participant = participant_id();
        let room = room_id("one-index");
        let transport =
            TransportHandle::new(crate::route::TransportRoute::new(ShardId::new(3), 7), 2);

        reg.add_participant(participant, room, ShardId::new(3), Some(transport));

        assert_eq!(
            reg.transport_of(&participant),
            Some((ShardId::new(3), transport)),
            "teardown finds the route to retire without a second index"
        );

        reg.remove_participant(&participant);
        assert_eq!(
            reg.transport_of(&participant),
            None,
            "and it goes away with the participant"
        );
    }

    /// A repeated prepare must report the same key rather than minting a
    /// second endpoint for the same participant.
    #[test]
    fn binding_a_participant_twice_keeps_the_first_key() {
        use slotmap::KeyData;
        let mut reg = RoomRegistry::new();
        let participant = participant_id();
        reg.add_participant(participant, room_id("bind-twice"), ShardId::new(0), None);

        let first = ParticipantKey::from(KeyData::from_ffi(1 | (1 << 32)));
        reg.bind_participant(&participant, first);
        reg.bind_participant(&participant, first);

        assert_eq!(
            reg.get_participant(&participant).and_then(|m| m.binding),
            Some(first)
        );
    }
}
