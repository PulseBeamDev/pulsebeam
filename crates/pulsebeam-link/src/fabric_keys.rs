use std::num::NonZeroU16;

use crate::{LinkTime, NodeRouteKey};

const REUSE_DELAY_NS: u64 = 2_000_000_000;

/// Receive keys are owned by one authenticated association. A slot that has
/// used all its generations stays exhausted until that association is retired.
pub(super) struct FabricKeys {
    slots: Vec<Slot>,
    limit: NonZeroU16,
}

enum Slot {
    Live(NodeRouteKey),
    Cooling { generation: u8, until: LinkTime },
    Exhausted,
}

impl FabricKeys {
    pub(super) fn new(limit: NonZeroU16) -> Self {
        Self {
            slots: Vec::new(),
            limit,
        }
    }

    /// Commit a key only after its receive binding can be published. Rejected
    /// installations do not consume generations or change the reuse floor.
    pub(super) fn install(
        &mut self,
        now: LinkTime,
        shard: u8,
        publish: impl FnOnce(NodeRouteKey) -> bool,
    ) -> Option<NodeRouteKey> {
        let reused = self
            .slots
            .iter()
            .enumerate()
            .find_map(|(slot, state)| match *state {
                Slot::Cooling { generation, until } if until <= now => Some((slot, generation)),
                _ => None,
            });
        let (offset, generation) = if let Some(reused) = reused {
            reused
        } else {
            if self.slots.len() >= usize::from(self.limit.get()) {
                return None;
            }
            self.slots.try_reserve(1).ok()?;
            (self.slots.len(), 1)
        };
        let [slot_hi, slot_lo] = u16::try_from(offset).ok()?.to_be_bytes();
        let key = NodeRouteKey::from_bytes([generation, shard, slot_hi, slot_lo]);
        if !publish(key) {
            return None;
        }
        if let Some(slot) = self.slots.get_mut(offset) {
            *slot = Slot::Live(key);
        } else {
            self.slots.push(Slot::Live(key));
        }
        Some(key)
    }

    /// Call after the exact binding has been fenced. A stale teardown never
    /// changes the state or reuse deadline of a replacement key.
    pub(super) fn retire(&mut self, now: LinkTime, key: NodeRouteKey) -> bool {
        let [generation, _, slot_hi, slot_lo] = key.to_bytes();
        let offset = usize::from(u16::from_be_bytes([slot_hi, slot_lo]));
        let Some(slot @ Slot::Live(_)) = self.slots.get_mut(offset) else {
            return false;
        };
        if !matches!(slot, Slot::Live(bound) if *bound == key) {
            return false;
        }
        *slot = match (generation.checked_add(1), now.0.checked_add(REUSE_DELAY_NS)) {
            (Some(generation), Some(until)) => Slot::Cooling {
                generation,
                until: LinkTime(until),
            },
            _ => Slot::Exhausted,
        };
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_slot_generation_and_exact_cutoff() {
        let mut keys = FabricKeys::new(NonZeroU16::MIN);
        let first = NodeRouteKey::from_bytes([1, 9, 0, 0]);
        assert_eq!(
            keys.install(LinkTime(0), 9, |key| {
                assert_eq!(key, first);
                false
            }),
            None
        );
        assert_eq!(keys.install(LinkTime(0), 9, |_| true), Some(first));
        assert_eq!(keys.install(LinkTime(0), 9, |_| true), None);
        assert!(!keys.retire(LinkTime(0), NodeRouteKey::from_bytes([1, 8, 0, 0])));
        assert!(keys.retire(LinkTime(0), first));
        assert_eq!(
            keys.install(LinkTime(REUSE_DELAY_NS.saturating_sub(1)), 8, |_| true),
            None
        );
        let successor = NodeRouteKey::from_bytes([2, 8, 0, 0]);
        assert_eq!(
            keys.install(LinkTime(REUSE_DELAY_NS), 8, |_| true),
            Some(successor)
        );
        assert!(!keys.retire(LinkTime(REUSE_DELAY_NS), first));
        assert!(keys.retire(LinkTime(REUSE_DELAY_NS), successor));
    }

    #[test]
    fn exhausted_generation_never_wraps_or_repeats_on_the_association() {
        let mut keys = FabricKeys::new(NonZeroU16::MIN);
        let mut now = LinkTime(0);
        for generation in 1..=u8::MAX {
            let expected = NodeRouteKey::from_bytes([generation, 12, 0, 0]);
            assert_eq!(keys.install(now, 12, |_| true), Some(expected));
            assert!(keys.retire(now, expected));
            now = LinkTime(now.0.saturating_add(REUSE_DELAY_NS));
        }
        assert_eq!(keys.install(now, 12, |_| true), None);
        let mut next_association = FabricKeys::new(NonZeroU16::MIN);
        assert_eq!(
            next_association.install(now, 12, |_| true),
            Some(NodeRouteKey::from_bytes([1, 12, 0, 0]))
        );
    }
}
