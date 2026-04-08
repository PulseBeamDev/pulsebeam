use crate::entity::ParticipantId;
use crate::participant::ParticipantCore;
use ahash::AHashMap;
use bitvec::prelude::*;

pub const MAX_PARTICIPANTS: usize = 4096;

pub type ParticipantKey = u16;
pub type DirtyBits = BitArray<[u64; 64], Lsb0>;

pub struct ParticipantScheduler {
    pub participants: Box<[Option<ParticipantCore>; MAX_PARTICIPANTS]>,
    id_to_slot: AHashMap<ParticipantId, ParticipantKey>,
    pub input_dirty: DirtyBits,
    pub fanout_dirty: DirtyBits,
    free_slots: Vec<ParticipantKey>,
}

impl ParticipantScheduler {
    pub fn new() -> Self {
        Self {
            participants: Box::new(std::array::from_fn(|_| None)),
            id_to_slot: AHashMap::with_capacity(MAX_PARTICIPANTS),
            input_dirty: BitArray::ZERO,
            fanout_dirty: BitArray::ZERO,
            free_slots: (0..MAX_PARTICIPANTS as ParticipantKey).rev().collect(),
        }
    }

    pub fn insert(&mut self, id: ParticipantId, core: ParticipantCore) -> Option<ParticipantKey> {
        let slot = self.free_slots.pop()?;
        self.id_to_slot.insert(id, slot);
        self.participants[slot as usize] = Some(core);
        Some(slot)
    }

    pub fn remove(&mut self, id: &ParticipantId) -> Option<(ParticipantKey, ParticipantCore)> {
        let slot = self.id_to_slot.remove(id)?;
        let core = self.participants[slot as usize].take()?;
        self.input_dirty.set(slot as usize, false);
        self.fanout_dirty.set(slot as usize, false);
        self.free_slots.push(slot);
        Some((slot, core))
    }

    #[inline]
    pub fn get_slot(&self, id: &ParticipantId) -> Option<ParticipantKey> {
        self.id_to_slot.get(id).copied()
    }
}
