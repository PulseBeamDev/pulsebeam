use crate::entity::ParticipantId;

use super::participants::ParticipantMeta;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirtyEntry {
    pub participant_id: ParticipantId,
    pub generation: u64,
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

    pub fn mark(&mut self, id: ParticipantId, participant: &mut ParticipantMeta) {
        #[cfg(debug_assertions)]
        debug_assert!(!self.active, "cannot dirty a participant during polling");
        debug_assert_eq!(participant.participant_id, id);
        if participant.queued_dirty {
            return;
        }
        debug_assert!(
            !self.participants[self.cursor..]
                .iter()
                .any(|entry| entry.participant_id == id
                    && entry.generation == participant.generation)
        );
        participant.queued_dirty = true;
        self.participants.push(DirtyEntry {
            participant_id: id,
            generation: participant.generation,
        });
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
            .any(|entry| entry.participant_id == *id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u8) -> ParticipantId {
        ParticipantId::from_bytes([value; 16])
    }

    #[test]
    fn phase_iteration_preserves_order_and_capacity() {
        let mut dirty = DirtyTracker::with_capacity(8);
        dirty.participants.extend([
            DirtyEntry {
                participant_id: id(1),
                generation: 10,
            },
            DirtyEntry {
                participant_id: id(2),
                generation: 20,
            },
        ]);
        let capacity = dirty.participants.capacity();

        dirty.begin_phase();
        assert_eq!(dirty.next().unwrap().participant_id, id(1));
        assert_eq!(dirty.next().unwrap().participant_id, id(2));
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
                participant_id: participant,
                generation: 10,
            },
            DirtyEntry {
                participant_id: participant,
                generation: 11,
            },
        ]);

        dirty.begin_phase();
        assert_eq!(dirty.next().unwrap().generation, 10);
        assert_eq!(dirty.next().unwrap().generation, 11);
        assert!(dirty.next().is_none());
        dirty.finish_phase();
    }
}
