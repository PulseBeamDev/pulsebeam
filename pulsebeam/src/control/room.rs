use std::collections::HashMap;

use indexmap::IndexMap;

use crate::entity::{ParticipantId, RoomId, TrackId};
use crate::id::ShardId;
use crate::track::Track;

pub struct Room {
    pub room_id: RoomId,
    participants: IndexMap<ParticipantId, Vec<Track>>,
    participant_shards: HashMap<ShardId, usize>,
}

impl Room {
    pub fn new(room_id: RoomId) -> Self {
        Self {
            room_id,
            participants: IndexMap::new(),
            participant_shards: HashMap::new(),
        }
    }

    pub fn add_participant(&mut self, participant_id: &ParticipantId, shard_id: ShardId) -> bool {
        if self.participants.contains_key(participant_id) {
            tracing::warn!(%participant_id, "duplicate participant in room");
            return false;
        }
        self.participants.insert(*participant_id, Vec::new());
        debug_assert!(self.participants.contains_key(participant_id));
        *self.participant_shards.entry(shard_id).or_insert(0) += 1;
        true
    }

    pub fn remove_participant(
        &mut self,
        participant_id: &ParticipantId,
        shard_id: ShardId,
    ) -> bool {
        if self.participants.swap_remove(participant_id).is_none() {
            tracing::debug!(%participant_id, "removing unknown participant from room");
            return false;
        }
        if let Some(count) = self.participant_shards.get_mut(&shard_id) {
            debug_assert!(*count > 0);
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.participant_shards.remove(&shard_id);
            }
        }
        true
    }

    pub(super) fn add_track(&mut self, track: Track) -> Result<(), ()> {
        let tracks = self.participants.entry(track.meta.origin).or_default();

        if let Some(existing) = tracks.iter().find(|t| t.meta.id == track.meta.id) {
            if existing.meta != track.meta {
                tracing::error!(track = %track.meta.id, "conflicting track metadata in room");
                return Err(());
            }
            return Ok(());
        }
        let track_id = track.meta.id;
        tracks.push(track);
        debug_assert!(tracks.iter().any(|candidate| candidate.meta.id == track_id));
        Ok(())
    }

    pub(super) fn remove_track(&mut self, origin: &ParticipantId, track_id: &TrackId) -> bool {
        let Some(tracks) = self.participants.get_mut(origin) else {
            return false;
        };

        let before = tracks.len();
        tracks.retain(|t| t.meta.id != *track_id);
        before != tracks.len()
    }

    pub fn recipient_shard_ids(
        &self,
        origin_shard_id: ShardId,
    ) -> impl Iterator<Item = ShardId> + '_ {
        self.participant_shards
            .iter()
            .filter(move |(shard_id, count)| **shard_id != origin_shard_id || **count > 1)
            .map(|(shard_id, _)| *shard_id)
    }

    pub fn participant_count(&self) -> usize {
        self.participants.len()
    }

    pub fn tracks(&self) -> impl Iterator<Item = &Track> {
        self.participants.values().flatten()
    }

    pub fn tracks_published_by(&self, participant_id: &ParticipantId) -> Vec<Track> {
        self.participants
            .get(participant_id)
            .cloned()
            .unwrap_or_default()
    }
}
