use indexmap::IndexMap;

use crate::entity::ParticipantId;
use crate::id::ShardId;

struct RoomPartition {
    shard_id: ShardId,
    participants: usize,
}

/// Node-local portion of a room. Its partitions are placement groups on
/// execution shards; the cluster-wide RoomShard is a different ownership level.
pub struct Room {
    participants: IndexMap<ParticipantId, usize>,
    partitions: Vec<RoomPartition>,
}

impl Room {
    pub fn new() -> Self {
        Self {
            participants: IndexMap::new(),
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
        self.partitions
            .iter()
            .position(|partition| partition.participants < capacity)
            .unwrap_or(self.partitions.len())
    }

    #[allow(
        clippy::expect_used,
        reason = "a controller-selected partition index must exist or be the next index"
    )]
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
        if partition_index == self.partitions.len() {
            self.partitions.push(RoomPartition {
                shard_id,
                participants: 0,
            });
        }
        let partition = self
            .partitions
            .get_mut(partition_index)
            .expect("partition indices must be allocated in order");
        // An emptied partition can be reassigned if the node-local placement
        // policy changes. A nonempty partition cannot switch owners.
        if partition.participants == 0 {
            partition.shard_id = shard_id;
        }
        assert_eq!(
            partition.shard_id, shard_id,
            "a live room partition cannot change execution shard"
        );
        partition.participants = partition.participants.saturating_add(1);
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
            debug_assert!(partition.participants > 0);
            partition.participants = partition.participants.saturating_sub(1);
        }
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
    fn partition_capacity_survives_departures_and_reconnects() {
        let mut room = Room::new();
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
