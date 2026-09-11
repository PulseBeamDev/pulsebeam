use std::collections::HashMap;

use crate::{
    control::room::Room,
    entity::{ConnectionId, ParticipantId, RoomId},
    id::ShardId,
    route::NodeTransportAddress,
};

/// Everything the control plane knows about one participant.
///
/// The single owner of `participant -> (shard, room)`. It used to be three
/// indexes — this registry, the lifecycle state, and a copy on every shard —
/// which is three chances for them to disagree about where somebody is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParticipantMeta {
    pub shard_id: ShardId,
    pub room_id: RoomId,
    /// The client's ICE association, kept so teardown can retire it. The
    /// route outlives the negotiation that produced it, so something has to
    /// remember it, and this is the record that already knows who it belongs
    /// to.
    pub transport: Option<NodeTransportAddress>,
    pub connection_id: ConnectionId,
    pub authorization: Option<super::controller::AuthorizationLease>,
    pub materialized: bool,
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

    #[cfg(test)]
    pub fn add_participant(
        &mut self,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard_id: ShardId,
        transport: Option<NodeTransportAddress>,
    ) {
        if let Some(previous) = self.participants.insert(
            participant_id,
            ParticipantMeta {
                shard_id,
                room_id,
                transport,
                connection_id: ConnectionId::new(),
                authorization: None,
                materialized: false,
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

    /// Atomically installs a prepared local incarnation when it wins UUIDv7
    /// ordering. The ordering is only a local convergence discriminator: this
    /// registry is neither replicated nor durable across process restarts.
    pub fn commit_candidate(
        &mut self,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard_id: ShardId,
        transport: NodeTransportAddress,
        connection_id: ConnectionId,
    ) -> Result<Option<ParticipantMeta>, CommitCandidateError> {
        if let Some(current) = self.participants.get(&participant_id)
            && current.connection_id >= connection_id
        {
            return Err(CommitCandidateError::Superseded);
        }

        let previous = self.participants.insert(
            participant_id,
            ParticipantMeta {
                shard_id,
                room_id,
                transport: Some(transport),
                connection_id,
                authorization: None,
                materialized: true,
            },
        );
        if let Some(previous) = previous {
            self.remove_from_room(&previous.room_id, &participant_id, previous.shard_id);
        }
        self.rooms
            .entry(room_id)
            .or_insert_with(Room::new)
            .add_participant(&participant_id, shard_id);
        Ok(previous)
    }

    #[cfg(test)]
    pub fn mark_materialized(&mut self, participant_id: &ParticipantId) {
        if let Some(meta) = self.participants.get_mut(participant_id) {
            meta.materialized = true;
        }
    }

    /// The transport route to retire when this participant goes away.
    /// Returns the shard_id that was hosting the participant, if found.
    #[cfg(test)]
    pub fn remove_participant(&mut self, participant_id: &ParticipantId) -> Option<ShardId> {
        let meta = self.participants.remove(participant_id)?;
        self.remove_from_room(&meta.room_id, participant_id, meta.shard_id);
        Some(meta.shard_id)
    }

    pub fn remove_incarnation(
        &mut self,
        participant_id: &ParticipantId,
        connection_id: ConnectionId,
    ) -> Option<ParticipantMeta> {
        if self
            .participants
            .get(participant_id)
            .map(|meta| meta.connection_id)
            != Some(connection_id)
        {
            return None;
        }
        let meta = self.participants.remove(participant_id)?;
        self.remove_from_room(&meta.room_id, participant_id, meta.shard_id);
        Some(meta)
    }

    pub fn update_authorization(
        &mut self,
        participant_id: &ParticipantId,
        connection_id: ConnectionId,
        authorization: super::controller::AuthorizationLease,
    ) {
        if let Some(meta) = self.participants.get_mut(participant_id)
            && meta.connection_id == connection_id
        {
            meta.authorization = Some(authorization);
        }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitCandidateError {
    Superseded,
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;
    use crate::{entity::RoomExternalId, route::TransportRoute};

    fn room_id(s: &str) -> RoomId {
        RoomId::from_external(&RoomExternalId::new(s).unwrap())
    }

    fn participant_id() -> ParticipantId {
        ParticipantId::new()
    }

    fn connection_id(sequence: u8) -> ConnectionId {
        ConnectionId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, sequence])
    }

    fn transport(shard: usize, slot: u32) -> NodeTransportAddress {
        NodeTransportAddress::new(TransportRoute::new(ShardId::new(shard), slot), 1)
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

    #[test]
    fn prepared_candidate_does_not_replace_current_until_commit() {
        let mut reg = RoomRegistry::new();
        let room = room_id("prepare");
        let participant = participant_id();
        let current = connection_id(1);
        reg.commit_candidate(participant, room, ShardId::new(0), transport(0, 1), current)
            .unwrap();

        let _prepared = (connection_id(2), transport(0, 2));

        assert_eq!(
            reg.get_participant(&participant).unwrap().connection_id,
            current
        );
    }

    #[test]
    fn failed_candidate_leaves_current_installed() {
        let mut reg = RoomRegistry::new();
        let room = room_id("failed-prepare");
        let participant = participant_id();
        let current = connection_id(1);
        reg.commit_candidate(participant, room, ShardId::new(0), transport(0, 1), current)
            .unwrap();

        let _failed_candidate = (connection_id(2), transport(0, 2));

        assert_eq!(
            reg.get_participant(&participant).unwrap().connection_id,
            current
        );
    }

    #[test]
    fn candidates_committing_out_of_order_keep_largest_connection_id() {
        let mut reg = RoomRegistry::new();
        let room = room_id("ordering");
        let participant = participant_id();
        let older = connection_id(1);
        let newer = connection_id(2);

        reg.commit_candidate(participant, room, ShardId::new(0), transport(0, 2), newer)
            .unwrap();
        assert_eq!(
            reg.commit_candidate(participant, room, ShardId::new(0), transport(0, 1), older,),
            Err(CommitCandidateError::Superseded)
        );

        assert_eq!(
            reg.get_participant(&participant).unwrap().connection_id,
            newer
        );
    }

    #[test]
    fn stale_lifecycle_work_cannot_remove_replacement() {
        let mut reg = RoomRegistry::new();
        let room = room_id("fencing");
        let participant = participant_id();
        let old = connection_id(1);
        let current = connection_id(2);
        reg.commit_candidate(participant, room, ShardId::new(0), transport(0, 1), old)
            .unwrap();
        reg.commit_candidate(participant, room, ShardId::new(0), transport(0, 2), current)
            .unwrap();

        // DELETE, close, transport failure, and expiry all converge on this
        // exact-incarnation operation in the controller.
        for _ in 0..4 {
            assert!(reg.remove_incarnation(&participant, old).is_none());
        }
        assert_eq!(
            reg.get_participant(&participant).unwrap().connection_id,
            current
        );
        assert!(reg.remove_incarnation(&participant, current).is_some());
    }

    #[test]
    fn local_registry_restart_makes_no_durability_promise() {
        let mut before_restart = RoomRegistry::new();
        let room = room_id("restart");
        let participant = participant_id();
        before_restart
            .commit_candidate(
                participant,
                room,
                ShardId::new(0),
                transport(0, 1),
                connection_id(1),
            )
            .unwrap();

        let after_restart = RoomRegistry::new();
        assert!(after_restart.get_participant(&participant).is_none());
    }
}
