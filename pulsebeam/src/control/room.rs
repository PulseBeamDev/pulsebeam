use std::{collections::BTreeMap, time::Duration};

use indexmap::IndexMap;

use crate::entity::{ParticipantId, RoomId};
use crate::track::TrackMeta;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Room {
    room_id: RoomId,
    participants: IndexMap<ParticipantId, Vec<TrackMeta>>,
}

impl Room {
    pub fn new(room_id: RoomId) -> Self {
        Self {
            room_id,
            participants: IndexMap::new(),
        }
    }

    pub fn add_participant(&mut self, participant_id: &ParticipantId) {
        self.participants.insert(*participant_id, Vec::new());
    }

    pub fn remove_participant(&mut self, participant_id: &ParticipantId) {
        self.participants.remove(participant_id);
    }

    pub fn publish_track(&mut self, track: TrackMeta) {
        let tracks = self
            .participants
            .entry(track.origin_participant)
            .or_default();

        if !tracks.iter().any(|t| t.id == track.id) {
            tracks.push(track);
        }
    }

    pub fn participants_iter(&self) -> impl Iterator<Item = &ParticipantId> {
        self.participants.keys()
    }

    pub fn participant_count(&self) -> usize {
        self.participants.len()
    }
}
