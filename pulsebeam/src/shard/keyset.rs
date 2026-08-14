//! Membership for dense arena keys, without a linear scan.
//!
//! Room and per-topic subscriber sets are read on the forwarding path and
//! mutated on the teardown path, and both can hold hundreds of entries. A
//! `Vec` plus `contains`/`swap_remove` makes each of those O(n), which is
//! affordable at ten members and is not at two hundred.
//!
//! The keys are already dense integers, so the index is the membership test:
//! `sparse[key.index()]` says where the key sits in `dense`, or that it is
//! absent. Insert, remove and lookup are all O(1); iteration walks `dense`
//! and touches nothing else.
#![deny(clippy::arithmetic_side_effects)]

use slotmap::Key;

/// Sentinel for "this index is not a member". `u32::MAX` positions cannot
/// occur: `dense` is bounded by the arena, which is bounded by the 32-bit key
/// index it is built from.
const ABSENT: u32 = u32::MAX;

/// A set of arena keys with O(1) insert, remove and membership.
///
/// Iteration order is insertion order with the usual swap-remove caveat — a
/// removal moves the last member into the hole. That is deterministic given
/// the same sequence of operations, which is what the simulator needs; it is
/// not sorted, and nothing may depend on it being so.
#[derive(Debug, Clone)]
pub(crate) struct KeySet<K: Key> {
    dense: Vec<K>,
    sparse: Vec<u32>,
}

impl<K: Key> Default for KeySet<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Key> KeySet<K> {
    pub fn new() -> Self {
        Self {
            dense: Vec::new(),
            sparse: Vec::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            dense: Vec::with_capacity(capacity),
            sparse: Vec::with_capacity(capacity),
        }
    }

    /// The arena index a key occupies. Slotmap keys are `index | version << 32`,
    /// so the low half is the dense part and the high half distinguishes
    /// incarnations that reused it.
    fn index_of(key: K) -> usize {
        // Truncation to the low 32 bits is the point: that half *is* the index.
        #[allow(clippy::cast_possible_truncation)]
        let idx = key.data().as_ffi() as u32;
        idx as usize
    }

    fn position_of(&self, key: K) -> Option<usize> {
        let pos = *self.sparse.get(Self::index_of(key))?;
        if pos == ABSENT {
            return None;
        }
        let pos = pos as usize;
        // The version guard: an index can be reused by a later incarnation, and
        // a stale key must not read as a member of the set its predecessor was
        // in.
        if self.dense.get(pos) == Some(&key) {
            Some(pos)
        } else {
            None
        }
    }

    /// A key still occupying `idx`'s sparse slot that is not `idx`'s current
    /// occupant — the residue of an arena index handed out again.
    fn stale_at(&self, idx: usize) -> Option<K> {
        let pos = *self.sparse.get(idx)?;
        if pos == ABSENT {
            return None;
        }
        self.dense.get(pos as usize).copied()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.position_of(*key).is_some()
    }

    /// Returns whether the key was newly added.
    pub fn insert(&mut self, key: K) -> bool {
        if self.contains(&key) {
            return false;
        }
        let idx = Self::index_of(key);
        // The index may still be recorded for a previous incarnation that was
        // dropped from the arena without being removed from here. Overwriting
        // the sparse slot would strand that key in `dense` — invisible to
        // `contains`, but still iterated, so it would keep receiving fanout
        // forever. Evict it first.
        if let Some(stale) = self.stale_at(idx) {
            self.remove(&stale);
        }
        if idx >= self.sparse.len() {
            self.sparse.resize(idx.saturating_add(1), ABSENT);
        }
        let Ok(pos) = u32::try_from(self.dense.len()) else {
            debug_assert!(false, "a membership set outgrew the key index space");
            return false;
        };
        let Some(slot) = self.sparse.get_mut(idx) else {
            debug_assert!(false, "the resize above guarantees this slot exists");
            return false;
        };
        *slot = pos;
        self.dense.push(key);
        true
    }

    /// Returns whether the key was present.
    pub fn remove(&mut self, key: &K) -> bool {
        let Some(pos) = self.position_of(*key) else {
            return false;
        };
        self.dense.swap_remove(pos);
        if let Some(slot) = self.sparse.get_mut(Self::index_of(*key)) {
            *slot = ABSENT;
        }
        // `swap_remove` moved whatever was last into `pos`, so that key's
        // recorded position is now wrong. Nothing else moved.
        if let Some(&moved) = self.dense.get(pos) {
            let Ok(pos) = u32::try_from(pos) else {
                debug_assert!(false, "a membership set outgrew the key index space");
                return true;
            };
            if let Some(slot) = self.sparse.get_mut(Self::index_of(moved)) {
                *slot = pos;
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        self.dense.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dense.is_empty()
    }

    #[cfg(test)]
    pub fn as_slice(&self) -> &[K] {
        &self.dense
    }

    /// Alias matching the `VecSet` vocabulary the call sites already use, so
    /// swapping the container in did not touch fifty call sites.
    pub fn insert_unique(&mut self, key: K) -> bool {
        self.insert(key)
    }

    pub fn remove_value(&mut self, key: &K) -> bool {
        self.remove(key)
    }

}

impl<'a, K: Key> IntoIterator for &'a KeySet<K> {
    type Item = &'a K;
    type IntoIter = std::slice::Iter<'a, K>;

    fn into_iter(self) -> Self::IntoIter {
        self.dense.iter()
    }
}

impl<K: Key> IntoIterator for KeySet<K> {
    type Item = K;
    type IntoIter = std::vec::IntoIter<K>;

    fn into_iter(self) -> Self::IntoIter {
        self.dense.into_iter()
    }
}

impl<K: Key> FromIterator<K> for KeySet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(iter: I) -> Self {
        let mut set = Self::new();
        for key in iter {
            set.insert(key);
        }
        set
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::shard::participants::ParticipantKey;
    use slotmap::SlotMap;

    fn keys(n: usize) -> Vec<ParticipantKey> {
        let mut arena = SlotMap::<ParticipantKey, ()>::with_key();
        (0..n).map(|_| arena.insert(())).collect()
    }

    #[test]
    fn insert_is_idempotent_and_membership_is_exact() {
        let members = keys(4);
        let mut set = KeySet::new();

        for key in &members {
            assert!(set.insert(*key));
        }
        for key in &members {
            assert!(!set.insert(*key), "a second insert must not duplicate");
        }

        assert_eq!(set.len(), members.len());
        for key in &members {
            assert!(set.contains(key));
        }
    }

    #[test]
    fn removal_leaves_every_other_member_reachable() {
        let members = keys(64);
        let mut set: KeySet<ParticipantKey> = members.iter().copied().collect();

        // Remove every third, then check the rest are all still exactly right —
        // a sparse set that mis-fixes the swapped position loses a member
        // silently, which on a fanout path is a subscriber that receives
        // nothing while everyone else is served.
        let (removed, kept): (Vec<_>, Vec<_>) = members
            .iter()
            .enumerate()
            .partition(|(n, _)| n % 3 == 0);

        for (_, key) in &removed {
            assert!(set.remove(key));
        }

        assert_eq!(set.len(), kept.len());
        for (_, key) in &kept {
            assert!(set.contains(key), "a surviving member went missing");
        }
        for (_, key) in &removed {
            assert!(!set.contains(key));
        }
    }

    #[test]
    fn removing_an_absent_key_is_harmless() {
        let members = keys(2);
        let mut set = KeySet::new();
        set.insert(members[0]);

        assert!(!set.remove(&members[1]));
        assert!(set.remove(&members[0]));
        assert!(!set.remove(&members[0]), "a repeated removal is a no-op");
        assert!(set.is_empty());
    }

    /// A slotmap index is reused once its occupant is gone. The set must tell
    /// the two incarnations apart, or a new participant would inherit the
    /// subscriptions of whoever held its index before.
    #[test]
    fn a_reused_index_does_not_inherit_the_previous_incarnation_s_membership() {
        let mut arena = SlotMap::<ParticipantKey, ()>::with_key();
        let first = arena.insert(());
        let mut set = KeySet::new();
        set.insert(first);

        arena.remove(first);
        let second = arena.insert(());
        assert_ne!(first, second);

        assert!(
            !set.contains(&second),
            "a recycled index must not read as a member"
        );
        assert!(!set.remove(&second));

        // Adding the new incarnation must evict the old one outright rather
        // than leave it stranded in the iteration order, where it would keep
        // being served while reading as absent.
        assert!(set.insert(second));
        assert_eq!(set.len(), 1, "the recycled index holds one member, not two");
        assert_eq!(set.as_slice(), [second]);
        assert!(!set.contains(&first));
    }


    #[test]
    fn iteration_yields_every_member_exactly_once() {
        let members = keys(32);
        let mut set: KeySet<ParticipantKey> = members.iter().copied().collect();
        set.remove(&members[7]);
        set.remove(&members[31]);

        let mut seen: Vec<_> = set.as_slice().to_vec();
        assert_eq!(seen.len(), 30);
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 30, "no member is yielded twice");
    }
}
