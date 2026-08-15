#[cfg(test)]
use crate::entity::ParticipantId;

use super::participants::{ParticipantKey, ParticipantMeta};

pub(crate) struct DirtyTracker {
    participants: Vec<ParticipantKey>,
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

    /// `participant` is the object `key` already resolved to — passed in so
    /// marking never has to re-resolve it, not so this can re-check the key.
    /// A `&mut ParticipantMeta` obtained through `ParticipantRegistry` is
    /// already proof the key is current; there is nothing left here for a
    /// generation field to guard.
    pub fn mark(&mut self, key: ParticipantKey, participant: &mut ParticipantMeta) {
        #[cfg(debug_assertions)]
        debug_assert!(!self.active, "cannot dirty a participant during polling");
        if participant.queued_dirty {
            return;
        }
        participant.queued_dirty = true;
        self.participants.push(key);
    }

    pub fn begin_phase(&mut self) {
        debug_assert_eq!(self.cursor, 0);
        #[cfg(debug_assertions)]
        {
            debug_assert!(!self.active);
            self.active = true;
        }
    }

    pub fn next(&mut self) -> Option<ParticipantKey> {
        #[cfg(debug_assertions)]
        debug_assert!(self.active);
        let entry = self.participants.get(self.cursor).copied()?;
        self.cursor = self.cursor.saturating_add(1);
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

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.participants.is_empty()
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;

    fn id(value: u8) -> ParticipantId {
        ParticipantId::from_bytes([value; 16])
    }

    fn key(index: u32, version: u32) -> ParticipantKey {
        ParticipantKey::from(slotmap::KeyData::from_ffi(
            (u64::from(version) << 32) | u64::from(index),
        ))
    }

    #[test]
    fn phase_iteration_preserves_order_and_capacity() {
        let mut dirty = DirtyTracker::with_capacity(8);
        let a = key(0, 1);
        let b = key(1, 1);
        dirty.participants.extend([a, b]);
        let capacity = dirty.participants.capacity();

        dirty.begin_phase();
        assert_eq!(dirty.next().unwrap(), a);
        assert_eq!(dirty.next().unwrap(), b);
        assert!(dirty.next().is_none());
        dirty.finish_phase();

        assert!(dirty.is_empty());
        assert_eq!(dirty.participants.capacity(), capacity);
    }

    #[test]
    fn stale_and_replacement_generations_remain_distinct() {
        let participant = id(1);
        let _ = participant;
        let mut dirty = DirtyTracker::with_capacity(8);
        let stale = key(0, 1);
        let replacement = key(0, 3);
        dirty.participants.extend([stale, replacement]);

        dirty.begin_phase();
        assert_eq!(dirty.next().unwrap(), stale);
        assert_eq!(dirty.next().unwrap(), replacement);
        assert!(dirty.next().is_none());
        dirty.finish_phase();
    }
}
