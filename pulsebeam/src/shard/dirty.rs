#[cfg(test)]
use crate::entity::ParticipantId;

use super::participants::{ParticipantHandle, ParticipantMeta};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirtyEntry {
    pub handle: ParticipantHandle,
}

pub(crate) struct DirtyTracker {
    participants: Vec<DirtyEntry>,
    cursor: usize,
    #[cfg(debug_assertions)]
    active: bool,
}

impl DirtyTracker {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            participants: Vec::with_capacity(capacity),
            cursor: 0,
            #[cfg(debug_assertions)]
            active: false,
        }
    }

    pub fn mark(&mut self, handle: ParticipantHandle, participant: &mut ParticipantMeta) {
        #[cfg(debug_assertions)]
        debug_assert!(!self.active, "cannot dirty a participant during polling");
        debug_assert_eq!(participant.participant_id, handle.participant_id());
        debug_assert_eq!(participant.generation, handle.generation());
        if participant.queued_dirty {
            return;
        }
        debug_assert!(
            !self.participants[self.cursor..]
                .iter()
                .any(|entry| entry.handle == handle)
        );
        participant.queued_dirty = true;
        self.participants.push(DirtyEntry { handle });
    }

    pub fn begin_phase(&mut self) {
        debug_assert_eq!(self.cursor, 0);
        #[cfg(debug_assertions)]
        {
            debug_assert!(!self.active);
            self.active = true;
        }
    }

    pub fn next(&mut self) -> Option<DirtyEntry> {
        #[cfg(debug_assertions)]
        debug_assert!(self.active);
        let entry = self.participants.get(self.cursor).copied()?;
        self.cursor += 1;
        Some(entry)
    }

    pub fn finish_phase(&mut self) {
        debug_assert_eq!(self.cursor, self.participants.len());
        self.participants.clear();
        self.cursor = 0;
        #[cfg(debug_assertions)]
        {
            debug_assert!(self.active);
            self.active = false;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.participants.is_empty()
    }

    #[cfg(test)]
    pub fn contains(&self, id: &ParticipantId) -> bool {
        self.participants[self.cursor..]
            .iter()
            .any(|entry| entry.handle.participant_id() == *id)
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    #![allow(clippy::disallowed_types)]
    use super::*;

    fn id(value: u8) -> ParticipantId {
        ParticipantId::from_bytes([value; 16])
    }

    fn handle(id: ParticipantId, generation: u64) -> ParticipantHandle {
        use slotmap::SlotMap;

        let mut slots = SlotMap::<crate::shard::participants::LocalParticipantKey, ()>::with_key();
        ParticipantHandle::new(slots.insert(()), id, generation)
    }

    #[test]
    fn phase_iteration_preserves_order_and_capacity() {
        let mut dirty = DirtyTracker::with_capacity(8);
        dirty.participants.extend([
            DirtyEntry {
                handle: handle(id(1), 10),
            },
            DirtyEntry {
                handle: handle(id(2), 20),
            },
        ]);
        let capacity = dirty.participants.capacity();

        dirty.begin_phase();
        assert_eq!(dirty.next().unwrap().handle.participant_id(), id(1));
        assert_eq!(dirty.next().unwrap().handle.participant_id(), id(2));
        assert!(dirty.next().is_none());
        dirty.finish_phase();

        assert!(dirty.is_empty());
        assert_eq!(dirty.participants.capacity(), capacity);
    }

    #[test]
    fn stale_and_replacement_generations_remain_distinct() {
        let participant = id(1);
        let mut dirty = DirtyTracker::with_capacity(8);
        dirty.participants.extend([
            DirtyEntry {
                handle: handle(participant, 10),
            },
            DirtyEntry {
                handle: handle(participant, 11),
            },
        ]);

        dirty.begin_phase();
        assert_eq!(dirty.next().unwrap().handle.generation(), 10);
        assert_eq!(dirty.next().unwrap().handle.generation(), 11);
        assert!(dirty.next().is_none());
        dirty.finish_phase();
    }
}
