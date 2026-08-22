use std::collections::HashMap;

use indexmap::{IndexMap, IndexSet};

use crate::entity::ParticipantId;
use crate::id::ShardId;

pub struct Room {
    participants: IndexMap<ParticipantId, ()>,
    participants_by_shard: HashMap<ShardId, IndexSet<ParticipantId>>,
}

impl Room {
    pub fn new() -> Self {
        Self {
            participants: IndexMap::new(),
            participants_by_shard: HashMap::new(),
        }
    }

    pub fn add_participant(&mut self, participant_id: &ParticipantId, shard_id: ShardId) {
        let previous = self.participants.insert(*participant_id, ());
        debug_assert!(
            previous.is_none(),
            "a room cannot contain a participant twice"
        );
        if previous.is_some() {
            return;
        }
        let inserted = self
            .participants_by_shard
            .entry(shard_id)
            .or_default()
            .insert(*participant_id);
        debug_assert!(inserted, "a participant must have one room shard entry");
    }

    pub fn remove_participant(&mut self, participant_id: &ParticipantId, shard_id: ShardId) {
        if self.participants.swap_remove(participant_id).is_some()
            && let Some(participants) = self.participants_by_shard.get_mut(&shard_id)
        {
            let removed = participants.swap_remove(participant_id);
            debug_assert!(removed, "a participant must have one room shard entry");
            if participants.is_empty() {
                self.participants_by_shard.remove(&shard_id);
            }
        }
    }

    pub fn participant_count(&self) -> usize {
        self.participants.len()
    }

    pub fn participant_ids(&self) -> impl Iterator<Item = &ParticipantId> {
        self.participants.keys()
    }
}
