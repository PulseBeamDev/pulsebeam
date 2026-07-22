use indexmap::IndexSet;
use pulsebeam_runtime::rand::{Rng, RngCore};

use crate::entity::ParticipantId;

/// `Input`  = something arrived (ingress packet, timer fire, remote command)
///            and the participant needs to process it.
/// `Fanout` = the participant received forwarded media/topology as a
///            consequence of someone *else's* input and needs to re-serialize
///            for egress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirtyKind {
    Input,
    Fanout,
}

pub(crate) struct DirtyTracker {
    input: IndexSet<ParticipantId, ahash::RandomState>,
    fanout: IndexSet<ParticipantId, ahash::RandomState>,
}

impl DirtyTracker {
    pub fn with_capacity(capacity: usize, rng: &mut Rng) -> Self {
        let mut seed = || {
            ahash::RandomState::with_seeds(
                rng.next_u64(),
                rng.next_u64(),
                rng.next_u64(),
                rng.next_u64(),
            )
        };
        Self {
            input: IndexSet::with_capacity_and_hasher(capacity, seed()),
            fanout: IndexSet::with_capacity_and_hasher(capacity, seed()),
        }
    }

    pub fn mark(&mut self, kind: DirtyKind, id: ParticipantId) {
        match kind {
            DirtyKind::Input => self.input.insert(id),
            DirtyKind::Fanout => self.fanout.insert(id),
        };
    }

    pub fn mark_input(&mut self, id: ParticipantId) {
        self.input.insert(id);
    }

    /// Removes `id` from the input set. Must be called on participant exit
    /// so a stale id isn't polled after removal.
    pub fn clear_input(&mut self, id: &ParticipantId) {
        self.input.swap_remove(id);
    }

    pub fn input(&self) -> &IndexSet<ParticipantId, ahash::RandomState> {
        &self.input
    }

    pub fn fanout(&self) -> &IndexSet<ParticipantId, ahash::RandomState> {
        &self.fanout
    }

    /// Drains both sets into reusable shard-owned storage, omitting ids that
    /// are present in both sets. Egress must flush a participant only once.
    pub fn drain_all_into(&mut self, out: &mut Vec<ParticipantId>) {
        debug_assert!(
            out.is_empty(),
            "egress scratch must be cleared before draining"
        );
        self.fanout.retain(|id| !self.input.contains(id));
        out.extend(self.input.drain(..));
        out.extend(self.fanout.drain(..));
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
    fn clear_input_prevents_repoll_after_exit() {
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut dirty = DirtyTracker::with_capacity(8, &mut rng);
        let id = pid();
        dirty.mark_input(id);
        dirty.clear_input(&id);
        assert!(!dirty.input().contains(&id));
    }

    #[test]
    fn drain_all_covers_both_sets_without_duplicating_work() {
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut dirty = DirtyTracker::with_capacity(8, &mut rng);
        let a = pid();
        let b = pid();
        dirty.mark(DirtyKind::Input, a);
        dirty.mark(DirtyKind::Fanout, a);
        dirty.mark(DirtyKind::Fanout, b);
        let mut drained = Vec::new();
        dirty.drain_all_into(&mut drained);
        assert_eq!(drained.len(), 2);
        assert!(drained.contains(&a) && drained.contains(&b));
        assert!(dirty.input().is_empty() && dirty.fanout().is_empty());
    }
}
