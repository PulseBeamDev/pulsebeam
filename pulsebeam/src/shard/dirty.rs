use indexmap::IndexSet;
use pulsebeam_runtime::rand::{Rng, RngCore};

use crate::entity::ParticipantId;

pub(crate) struct DirtyTracker {
    participants: IndexSet<ParticipantId, ahash::RandomState>,
}

impl DirtyTracker {
    pub fn with_capacity(capacity: usize, rng: &mut Rng) -> Self {
        let state = ahash::RandomState::with_seeds(
            rng.next_u64(),
            rng.next_u64(),
            rng.next_u64(),
            rng.next_u64(),
        );
        Self {
            participants: IndexSet::with_capacity_and_hasher(capacity, state),
        }
    }

    pub fn mark(&mut self, id: ParticipantId) {
        self.participants.insert(id);
    }

    pub fn clear(&mut self, id: &ParticipantId) {
        self.participants.swap_remove(id);
    }

    pub fn is_empty(&self) -> bool {
        self.participants.is_empty()
    }

    #[cfg(test)]
    pub fn contains(&self, id: &ParticipantId) -> bool {
        self.participants.contains(id)
    }

    pub fn drain_into(&mut self, out: &mut Vec<ParticipantId>) {
        debug_assert!(
            out.is_empty(),
            "dirty participant scratch must be empty before draining"
        );
        out.extend(self.participants.drain(..));
        debug_assert!(self.participants.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn pid() -> ParticipantId {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        ParticipantId::new(&mut pulsebeam_runtime::rand::seeded_rng(
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ))
    }

    #[test]
    fn clear_prevents_repoll_after_exit() {
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut dirty = DirtyTracker::with_capacity(8, &mut rng);
        let id = pid();
        dirty.mark(id);
        dirty.clear(&id);
        assert!(dirty.is_empty());
    }

    #[test]
    fn drain_covers_each_participant_once() {
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut dirty = DirtyTracker::with_capacity(8, &mut rng);
        let a = pid();
        let b = pid();
        dirty.mark(a);
        dirty.mark(a);
        dirty.mark(b);
        let mut drained = Vec::new();
        dirty.drain_into(&mut drained);
        assert_eq!(drained.len(), 2);
        assert!(drained.contains(&a) && drained.contains(&b));
        assert!(dirty.is_empty());
    }
}
