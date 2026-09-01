use std::collections::HashMap;

use crate::{
    control::room::Room,
    entity::{ConnectionId, ParticipantId, RoomId},
    id::ShardId,
    route::TransportHandle,
    shard::participants::ParticipantKey,
};

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
    pub connection_id: Option<ConnectionId>,
    pub connected: bool,
}

pub struct RoomRegistry {
    rooms: HashMap<RoomId, Room>,
    participants: HashMap<ParticipantId, ParticipantMeta>,
}

impl RoomRegistry {
    pub fn new() -> Self {
        Self {
            rooms: HashMap::new(),
            participants: HashMap::new(),
        }
    }

    pub fn get_room(&self, room_id: &RoomId) -> Option<&Room> {
        self.rooms.get(room_id)
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
                connection_id: None,
                connected: true,
            },
        ) {
            self.remove_from_room(&previous.room_id, &participant_id, previous.shard_id);
        }
        let room = self.rooms.entry(room_id).or_insert_with(Room::new);
        room.add_participant(&participant_id, shard_id);
    }

    pub fn get_participant(&self, participant_id: &ParticipantId) -> Option<&ParticipantMeta> {
        self.participants.get(participant_id)
    }

    pub fn participant_ids_in_room(&self, room_id: &RoomId) -> Vec<ParticipantId> {
        self.rooms
            .get(room_id)
            .into_iter()
            .flat_map(Room::participant_ids)
            .copied()
            .collect()
    }

    pub fn set_connection_id(
        &mut self,
        participant_id: &ParticipantId,
        connection_id: ConnectionId,
    ) {
        let Some(meta) = self.participants.get_mut(participant_id) else {
            debug_assert!(
                false,
                "connection id must be assigned to a registered participant"
            );
            return;
        };
        meta.connection_id = Some(connection_id);
    }

    /// Record the arena key the owning shard reported for this participant.
    /// Idempotent: a retry after a lost acknowledgement must not create a
    /// second binding.
    pub fn bind_participant(&mut self, participant_id: &ParticipantId, binding: ParticipantKey) {
        let Some(meta) = self.participants.get_mut(participant_id) else {
            return;
        };
        debug_assert!(meta.connected, "a disconnected participant cannot be bound");
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
    /// Returns the shard_id that was hosting the participant, if found.
    pub fn remove_participant(&mut self, participant_id: &ParticipantId) -> Option<ShardId> {
        let meta = self.participants.remove(participant_id)?;
        self.remove_from_room(&meta.room_id, participant_id, meta.shard_id);
        Some(meta.shard_id)
    }

    pub fn disconnect_participant(
        &mut self,
        participant_id: &ParticipantId,
    ) -> Option<(ShardId, Option<TransportHandle>, Option<ParticipantKey>)> {
        let (result, room_id, shard_id) = {
            let meta = self.participants.get_mut(participant_id)?;
            let result = (meta.shard_id, meta.transport.take(), meta.binding.take());
            meta.connected = false;
            (result, meta.room_id, meta.shard_id)
        };
        self.remove_from_room(&room_id, participant_id, shard_id);
        Some(result)
    }

    fn remove_from_room(
        &mut self,
        room_id: &RoomId,
        participant_id: &ParticipantId,
        shard_id: ShardId,
    ) {
        let empty = if let Some(room) = self.rooms.get_mut(room_id) {
            room.remove_participant(participant_id, shard_id);
            room.participant_count() == 0
        } else {
            false
        };
        if empty {
            self.rooms.remove(room_id);
        }
    }
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;
    use crate::entity::ExternalRoomId;

    fn room_id(s: &str) -> RoomId {
        RoomId::from_external(&ExternalRoomId::new(s).unwrap())
    }

    fn participant_id() -> ParticipantId {
        ParticipantId::new()
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

    #[test]
    fn add_participant_moves_existing_participant_to_new_room() {
        let mut reg = RoomRegistry::new();
        let old_room = room_id("room-b-old");
        let new_room = room_id("room-b-new");
        let pid = participant_id();

        reg.add_participant(pid, old_room, ShardId::new(0), None);
        reg.add_participant(pid, new_room, ShardId::new(1), None);

        assert!(reg.get_room(&old_room).is_none());
        assert_eq!(reg.get_room(&new_room).unwrap().participant_count(), 1);
        let meta = reg.get_participant(&pid).unwrap();
        assert_eq!(meta.room_id, new_room);
        assert_eq!(meta.shard_id, ShardId::new(1));
    }

    #[test]
    fn remove_participant_returns_shard_id() {
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

    #[test]
    fn room_is_immediately_deleted_after_last_participant_leaves() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-d");
        let pid = participant_id();

        reg.add_participant(pid, rid, ShardId::new(0), None);
        reg.remove_participant(&pid);

        assert!(reg.get_room(&rid).is_none());
    }

    #[test]
    fn participant_can_rejoin_a_deleted_room() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-f");
        let pid1 = participant_id();
        let pid2 = participant_id();

        reg.add_participant(pid1, rid, ShardId::new(0), None);
        reg.remove_participant(&pid1);

        reg.add_participant(pid2, rid, ShardId::new(1), None);

        reg.get_room(&rid).unwrap();
        assert_eq!(reg.get_room(&rid).unwrap().participant_count(), 1);
    }

    #[test]
    fn participant_removed_from_registry_after_remove() {
        let mut reg = RoomRegistry::new();
        let rid = room_id("room-h");
        let pid = participant_id();

        reg.add_participant(pid, rid, ShardId::new(0), None);
        reg.remove_participant(&pid);

        assert!(reg.get_participant(&pid).is_none());
    }

    #[test]
    fn multiple_rooms_are_independent() {
        let mut reg = RoomRegistry::new();
        let rid1 = room_id("room-x");
        let rid2 = room_id("room-y");
        let pid1 = participant_id();
        let pid2 = participant_id();

        reg.add_participant(pid1, rid1, ShardId::new(0), None);
        reg.add_participant(pid2, rid2, ShardId::new(1), None);
        reg.remove_participant(&pid1);
        assert!(reg.get_room(&rid1).is_none());
        assert!(reg.get_room(&rid2).is_some());
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
