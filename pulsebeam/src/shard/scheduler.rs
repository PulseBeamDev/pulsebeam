use crate::entity::{ParticipantId, ParticipantKey};
use crate::participant::ParticipantCore;
use ahash::AHashMap;
use bitvec::prelude::*;

pub const MAX_PARTICIPANTS: usize = 4096;

pub type DirtyBits = BitArray<[u64; 64], Lsb0>;

pub struct ParticipantScheduler {
    pub participants: Vec<Option<ParticipantCore>>,
    id_to_slot: AHashMap<ParticipantId, ParticipantKey>,
    pub input_dirty: DirtyBits,
    pub fanout_dirty: DirtyBits,
    free_slots: Vec<ParticipantKey>,
}

impl ParticipantScheduler {
    pub fn new() -> Self {
        Self {
            participants: (0..MAX_PARTICIPANTS).map(|_| None).collect(),
            id_to_slot: AHashMap::with_capacity(MAX_PARTICIPANTS),
            input_dirty: BitArray::ZERO,
            fanout_dirty: BitArray::ZERO,
            free_slots: (0..MAX_PARTICIPANTS as ParticipantKey).rev().collect(),
        }
    }

    pub fn insert(&mut self, id: ParticipantId, mut core: ParticipantCore) -> Option<ParticipantKey> {
        let slot = self.free_slots.pop()?;
        core.key = slot;
        self.id_to_slot.insert(id, slot);
        self.participants[slot as usize] = Some(core);
        Some(slot)
    }

    pub fn remove(&mut self, key: ParticipantKey) -> Option<ParticipantCore> {
        let core = self.participants[key as usize].take()?;
        self.id_to_slot.remove(&core.participant_id);
        self.input_dirty.set(key as usize, false);
        self.fanout_dirty.set(key as usize, false);
        self.free_slots.push(key);
        Some(core)
    }

    #[inline]
    pub fn get_slot(&self, id: &ParticipantId) -> Option<ParticipantKey> {
        self.id_to_slot.get(id).copied()
    }
}
