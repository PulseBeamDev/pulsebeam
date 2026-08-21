use std::collections::HashMap;

use indexmap::{IndexMap, IndexSet};

use crate::entity::{ParticipantId, RoomId, TrackId};
use crate::id::ShardId;
use crate::track::Track;

pub struct Room {
    pub room_id: RoomId,
    participants: IndexMap<ParticipantId, IndexMap<TrackId, Track>>,
    participants_by_shard: HashMap<ShardId, IndexSet<ParticipantId>>,
}

impl Room {
    pub fn new(room_id: RoomId) -> Self {
        Self {
            room_id,
            participants: IndexMap::new(),
            participants_by_shard: HashMap::new(),
        }
    }

    pub fn add_participant(&mut self, participant_id: &ParticipantId, shard_id: ShardId) {
        let previous = self.participants.insert(*participant_id, IndexMap::new());
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

    pub(super) fn add_track(&mut self, track: Track) {
        let tracks = self.participants.entry(track.meta().origin).or_default();
        let track_id = track.id();
        let previous = tracks.insert(track_id, track);
        debug_assert!(
            previous.is_none(),
            "a participant cannot publish a track twice"
        );
    }

    pub(super) fn remove_track(&mut self, origin: &ParticipantId, track_id: &TrackId) -> bool {
        let Some(tracks) = self.participants.get_mut(origin) else {
            return false;
        };

        tracks.shift_remove(track_id).is_some()
    }

    pub fn recipient_shard_ids(
        &self,
        origin_shard_id: ShardId,
    ) -> impl Iterator<Item = ShardId> + '_ {
        self.participants_by_shard
            .keys()
            .filter(move |shard_id| {
                **shard_id != origin_shard_id
                    || self
                        .participants_by_shard
                        .get(*shard_id)
                        .is_some_and(|participants| participants.len() > 1)
            })
            .copied()
    }

    pub fn shard_ids(&self) -> impl Iterator<Item = ShardId> + '_ {
        self.participants_by_shard.keys().copied()
    }

    pub fn participant_count(&self) -> usize {
        self.participants.len()
    }

    pub fn participant_ids(&self) -> impl Iterator<Item = &ParticipantId> {
        self.participants.keys()
    }

    pub fn participant_ids_on_shard(
        &self,
        shard_id: ShardId,
    ) -> impl Iterator<Item = &ParticipantId> {
        self.participants_by_shard
            .get(&shard_id)
            .into_iter()
            .flat_map(IndexSet::iter)
    }
}
