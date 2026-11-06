use indexmap::IndexMap;
use std::collections::{BTreeSet, HashMap};

use crate::entity::{ConnectionId, ParticipantId};
use crate::id::ShardId;

struct RoomPartition {
    shard_id: ShardId,
    occupants: usize,
}

struct PendingPlacement {
    partition_index: usize,
    connections: BTreeSet<ConnectionId>,
}

/// Node-owned portion of a logical room. Its partitions are placement
/// groups on local execution shards. Cluster synchronization is separate.
pub struct RoomShard {
    participants: IndexMap<ParticipantId, usize>,
    reservations: HashMap<ParticipantId, PendingPlacement>,
    partitions: Vec<RoomPartition>,
}

impl RoomShard {
    pub fn new() -> Self {
        Self {
            participants: IndexMap::new(),
            reservations: HashMap::new(),
            partitions: Vec::new(),
        }
    }

    /// Preserve an existing participant's placement on reconnect. Otherwise
    /// fill a partition with capacity before creating a new one. Empty slots
    /// are reusable until the entire room leaves this node.
    pub fn next_partition_index(&self, participant_id: &ParticipantId, capacity: usize) -> usize {
        debug_assert!(capacity > 0);
        if let Some(index) = self.participants.get(participant_id) {
            return *index;
        }
        if let Some(pending) = self.reservations.get(participant_id) {
            return pending.partition_index;
        }
        self.partitions
            .iter()
            .position(|partition| partition.occupants < capacity)
            .unwrap_or(self.partitions.len())
    }

    pub fn add_participant(
        &mut self,
        participant_id: &ParticipantId,
        shard_id: ShardId,
        partition_index: usize,
    ) {
        debug_assert!(
            !self.participants.contains_key(participant_id),
            "a room cannot contain a participant twice"
        );
        if self.participants.contains_key(participant_id) {
            return;
        }
        let reserved = self.reservations.get(participant_id);
        if let Some(pending) = reserved {
            assert_eq!(pending.partition_index, partition_index);
        }
        let held = reserved.is_some();
        let partition = self.partition_mut(shard_id, partition_index);
        if !held {
            partition.occupants = partition.occupants.saturating_add(1);
        }
        self.participants.insert(*participant_id, partition_index);
    }

    #[allow(
        clippy::expect_used,
        reason = "every admitted participant has a live local partition"
    )]
    pub fn remove_participant(&mut self, participant_id: &ParticipantId, shard_id: ShardId) {
        if let Some(index) = self.participants.swap_remove(participant_id) {
            let partition = self
                .partitions
                .get_mut(index)
                .expect("a participant must reference a known partition");
            debug_assert_eq!(partition.shard_id, shard_id);
            debug_assert!(partition.occupants > 0);
            if !self.reservations.contains_key(participant_id) {
                partition.occupants = partition.occupants.saturating_sub(1);
            }
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "a reserved partition is allocated before materialization"
    )]
    fn partition_mut(&mut self, shard_id: ShardId, index: usize) -> &mut RoomPartition {
        if index == self.partitions.len() {
            self.partitions.push(RoomPartition {
                shard_id,
                occupants: 0,
            });
        }
        let partition = self
            .partitions
            .get_mut(index)
            .expect("partition indices must be allocated in order");
        if partition.occupants == 0 {
            partition.shard_id = shard_id;
        }
        assert_eq!(
            partition.shard_id, shard_id,
            "a held room partition cannot change execution shard"
        );
        partition
    }

    pub fn partition_owner(&self, index: usize) -> Option<ShardId> {
        self.partitions
            .get(index)
            .filter(|partition| partition.occupants > 0)
            .map(|partition| partition.shard_id)
    }

    pub fn reserve(
        &mut self,
        participant: ParticipantId,
        connection: ConnectionId,
        shard: ShardId,
        index: usize,
    ) {
        if let Some(existing) = self.participants.get(&participant) {
            assert_eq!(*existing, index);
        }
        if let Some(pending) = self.reservations.get(&participant) {
            assert_eq!(pending.partition_index, index);
        }
        let held = self.participants.contains_key(&participant)
            || self.reservations.contains_key(&participant);
        let partition = self.partition_mut(shard, index);
        if !held {
            partition.occupants = partition.occupants.saturating_add(1);
        }
        self.reservations
            .entry(participant)
            .or_insert_with(|| PendingPlacement {
                partition_index: index,
                connections: BTreeSet::new(),
            })
            .connections
            .insert(connection);
    }

    pub fn reserved(
        &self,
        participant: ParticipantId,
        connection: ConnectionId,
        shard: ShardId,
        index: usize,
    ) -> bool {
        self.reservations.get(&participant).is_some_and(|pending| {
            pending.partition_index == index
                && pending.connections.contains(&connection)
                && self.partition_owner(index) == Some(shard)
        })
    }

    pub fn release(&mut self, participant: ParticipantId, connection: ConnectionId) {
        let Some(pending) = self.reservations.get_mut(&participant) else {
            return;
        };
        if !pending.connections.remove(&connection) || !pending.connections.is_empty() {
            return;
        }
        let index = pending.partition_index;
        self.reservations.remove(&participant);
        if !self.participants.contains_key(&participant)
            && let Some(partition) = self.partitions.get_mut(index)
        {
            partition.occupants = partition.occupants.saturating_sub(1);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.participants.is_empty() && self.reservations.is_empty()
    }

    pub fn participant_count(&self) -> usize {
        self.participants.len()
    }

    pub fn participant_ids(&self) -> impl Iterator<Item = &ParticipantId> {
        self.participants.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_claims_hold_capacity_through_out_of_order_commit_and_departure() {
        let mut room = RoomShard::new();
        let first = ParticipantId::new();
        let second = ParticipantId::new();
        let a = ConnectionId::new();
        let b = ConnectionId::new();
        room.reserve(first, a, ShardId::new(0), 0);
        assert_eq!(room.next_partition_index(&second, 1), 1);
        room.reserve(second, b, ShardId::new(1), 1);
        assert_eq!(room.participant_count(), 0);
        room.add_participant(&second, ShardId::new(1), 1);
        room.release(second, b);
        assert!(room.reserved(first, a, ShardId::new(0), 0));
        assert!(!room.reserved(first, a, ShardId::new(1), 0));
        room.remove_participant(&second, ShardId::new(1));
        assert!(!room.is_empty());
        assert_eq!(room.partition_owner(0), Some(ShardId::new(0)));
        room.release(first, a);
        assert!(room.is_empty());
        assert_eq!(room.next_partition_index(&ParticipantId::new(), 1), 0);
        room.reserve(second, b, ShardId::new(2), 0);
        assert!(!room.reserved(first, a, ShardId::new(0), 0));
        assert!(room.reserved(second, b, ShardId::new(2), 0));
        room.release(second, b);
        assert!(room.is_empty());
    }

    #[test]
    fn pending_replacements_share_one_placement_until_commit_or_cancellation() {
        let mut room = RoomShard::new();
        let participant = ParticipantId::new();
        let a = ConnectionId::new();
        let b = ConnectionId::new();
        let shard = ShardId::new(0);
        room.add_participant(&participant, shard, 0);
        room.reserve(participant, a, shard, 0);
        room.reserve(participant, b, shard, 0);
        assert_eq!(room.next_partition_index(&ParticipantId::new(), 1), 1);
        room.remove_participant(&participant, shard);
        room.release(participant, a);
        assert_eq!(room.participant_count(), 0);
        assert!(room.reserved(participant, b, shard, 0));
        room.add_participant(&participant, shard, 0);
        room.release(participant, b);
        room.release(participant, a);
        assert_eq!(room.next_partition_index(&ParticipantId::new(), 1), 1);
        room.remove_participant(&participant, shard);
        assert!(room.is_empty());
        assert_eq!(room.partition_owner(0), None);
    }

    #[test]
    fn partition_capacity_survives_departures_and_reconnects() {
        let mut room = RoomShard::new();
        let participants: Vec<_> = (0..5).map(|_| ParticipantId::new()).collect();
        for (index, participant) in participants.iter().enumerate() {
            let slot = room.next_partition_index(participant, 2);
            assert_eq!(slot, index / 2);
            room.add_participant(participant, ShardId::new(slot % 2), slot);
        }
        assert_eq!(room.next_partition_index(&participants[4], 2), 2);

        room.remove_participant(&participants[0], ShardId::new(0));
        assert_eq!(room.next_partition_index(&participants[4], 2), 2);
        let newcomer = ParticipantId::new();
        assert_eq!(room.next_partition_index(&newcomer, 2), 0);
        room.add_participant(&newcomer, ShardId::new(0), 0);

        room.remove_participant(&participants[1], ShardId::new(0));
        room.remove_participant(&newcomer, ShardId::new(0));
        let another = ParticipantId::new();
        assert_eq!(room.next_partition_index(&another, 2), 0);
        room.add_participant(&another, ShardId::new(0), 0);
        assert_eq!(room.participant_count(), 4);
    }
}
