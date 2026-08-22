use std::collections::HashMap;

use slotmap::SecondaryMap;

use crate::{
    keys::{ParticipantKey, TrackKey},
    route::RouteHandle,
};

#[derive(Debug, Clone)]
pub(crate) struct MembershipDelta<K> {
    pub added: Vec<K>,
    pub removed: Vec<(K, usize)>,
}

impl<K> Default for MembershipDelta<K> {
    fn default() -> Self {
        Self {
            added: Vec::new(),
            removed: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReverseRouteChange {
    Unchanged,
    Set(Option<RouteHandle>),
}

#[derive(Debug, Clone)]
pub(crate) struct PlanChange {
    pub key: TrackKey,
    pub create: bool,
    pub remove: bool,
    pub local: MembershipDelta<ParticipantKey>,
    pub remote: MembershipDelta<RouteHandle>,
    pub reverse: ReverseRouteChange,
}

impl PlanChange {
    pub(crate) fn is_empty(&self) -> bool {
        !self.create
            && !self.remove
            && self.local.added.is_empty()
            && self.local.removed.is_empty()
            && self.remote.added.is_empty()
            && self.remote.removed.is_empty()
            && matches!(self.reverse, ReverseRouteChange::Unchanged)
    }

    pub(crate) fn between(
        key: TrackKey,
        old: Option<&ControlPlan>,
        new: Option<&ControlPlan>,
    ) -> Self {
        let Some(new) = new else {
            return Self {
                key,
                create: false,
                remove: true,
                local: MembershipDelta::default(),
                remote: MembershipDelta::default(),
                reverse: ReverseRouteChange::Unchanged,
            };
        };
        let create = old.is_none();
        let empty = ControlPlan::default();
        let old = old.unwrap_or(&empty);
        Self {
            key,
            create,
            remove: false,
            local: old.local.delta_to(&new.local),
            remote: old.remote.delta_to(&new.remote),
            reverse: if old.reverse_route != new.reverse_route {
                ReverseRouteChange::Set(new.reverse_route)
            } else {
                ReverseRouteChange::Unchanged
            },
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PlanBatch {
    pub changes: Vec<PlanChange>,
}

impl PlanBatch {
    pub(crate) fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub(crate) fn push(&mut self, change: PlanChange) {
        debug_assert!(!(change.create && change.remove));
        self.changes.push(change);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DenseMembership<K> {
    values: Vec<K>,
}

impl<K> Default for DenseMembership<K> {
    fn default() -> Self {
        Self { values: Vec::new() }
    }
}

impl<K> DenseMembership<K>
where
    K: Copy + Eq + std::hash::Hash + std::fmt::Debug,
{
    pub(crate) fn from_values(values: impl IntoIterator<Item = K>) -> Self {
        let mut membership = Self::default();
        for value in values {
            if membership.values.contains(&value) {
                debug_assert!(false, "a compiled membership cannot contain duplicates");
                continue;
            }
            membership.values.push(value);
        }
        membership
    }

    pub(crate) fn insert(&mut self, value: K) {
        self.values.push(value);
    }

    #[cfg(test)]
    pub(crate) fn remove(&mut self, value: K) {
        let Some(index) = self.values.iter().position(|candidate| *candidate == value) else {
            debug_assert!(false, "a compiled membership removal must name a member");
            return;
        };
        self.values.swap_remove(index);
    }

    fn remove_at(&mut self, value: K, index: usize) {
        debug_assert_eq!(
            self.values.get(index).copied(),
            Some(value),
            "membership index mismatch: index={index} len={} values={:?}",
            self.values.len(),
            self.values,
        );
        if self.values.get(index).copied() != Some(value) {
            return;
        }
        self.values.swap_remove(index);
    }

    pub(crate) fn apply(&mut self, delta: &MembershipDelta<K>, touched: &mut usize) {
        for &(value, index) in &delta.removed {
            self.remove_at(value, index);
            *touched = touched.saturating_add(1);
        }
        for &value in &delta.added {
            self.insert(value);
            *touched = touched.saturating_add(1);
        }
    }

    pub(crate) fn values(&self) -> &[K] {
        &self.values
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ControlMembership<K> {
    values: Vec<K>,
    positions: HashMap<K, usize>,
}

impl<K> Default for ControlMembership<K> {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            positions: HashMap::new(),
        }
    }
}

impl<K> ControlMembership<K>
where
    K: Copy + Eq + std::hash::Hash + std::fmt::Debug,
{
    fn from_dense(dense: &DenseMembership<K>) -> Self {
        let values = dense.values.clone();
        let positions = values
            .iter()
            .copied()
            .enumerate()
            .map(|(index, value)| (value, index))
            .collect();
        Self { values, positions }
    }

    fn from_target(previous: &Self, target: &DenseMembership<K>) -> Self {
        let target_values: HashMap<K, ()> = target
            .values
            .iter()
            .copied()
            .map(|value| (value, ()))
            .collect();
        let mut values = previous.values.clone();
        let mut positions = previous.positions.clone();
        let mut removals: Vec<_> = values
            .iter()
            .enumerate()
            .filter_map(|(index, value)| (!target_values.contains_key(value)).then_some(index))
            .collect();
        removals.sort_unstable_by_key(|index| std::cmp::Reverse(*index));
        for index in removals {
            let value = values.swap_remove(index);
            debug_assert_eq!(positions.get(&value).copied(), Some(index));
            positions.remove(&value);
            if let Some(moved) = values.get(index).copied() {
                positions.insert(moved, index);
            }
        }
        for value in target.values.iter().copied() {
            if positions.contains_key(&value) {
                continue;
            }
            positions.insert(value, values.len());
            values.push(value);
        }
        Self { values, positions }
    }

    fn delta_to(&self, next: &ControlMembership<K>) -> MembershipDelta<K> {
        let mut removed: Vec<_> = self
            .values
            .iter()
            .copied()
            .filter_map(|value| {
                if next.positions.contains_key(&value) {
                    return None;
                }
                let Some(&index) = self.positions.get(&value) else {
                    debug_assert!(false, "control membership must index every value");
                    return None;
                };
                Some((value, index))
            })
            .collect();
        removed.sort_unstable_by_key(|(_, index)| std::cmp::Reverse(*index));
        MembershipDelta {
            removed,
            added: next
                .values
                .iter()
                .copied()
                .filter(|value| !self.positions.contains_key(value))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ControlPlan {
    local: ControlMembership<ParticipantKey>,
    remote: ControlMembership<RouteHandle>,
    reverse_route: Option<RouteHandle>,
}

impl ControlPlan {
    pub(crate) fn from_flat(plan: &FlatTrackPlan) -> Self {
        Self {
            local: ControlMembership::from_dense(&plan.local),
            remote: ControlMembership::from_dense(&plan.remote),
            reverse_route: plan.reverse_route,
        }
    }

    pub(crate) fn from_target(previous: &Self, plan: &FlatTrackPlan) -> Self {
        Self {
            local: ControlMembership::from_target(&previous.local, &plan.local),
            remote: ControlMembership::from_target(&previous.remote, &plan.remote),
            reverse_route: plan.reverse_route,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FlatTrackPlan {
    pub local: DenseMembership<ParticipantKey>,
    pub remote: DenseMembership<RouteHandle>,
    pub reverse_route: Option<RouteHandle>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FlatPlans {
    tracks: SecondaryMap<TrackKey, FlatTrackPlan>,
}

impl FlatPlans {
    pub(crate) fn get(&self, key: TrackKey) -> Option<&FlatTrackPlan> {
        self.tracks.get(key)
    }

    fn get_mut(&mut self, key: TrackKey) -> Option<&mut FlatTrackPlan> {
        self.tracks.get_mut(key)
    }

    fn insert(&mut self, key: TrackKey, plan: FlatTrackPlan) {
        let previous = self.tracks.insert(key, plan);
        debug_assert!(previous.is_none());
    }

    fn remove(&mut self, key: TrackKey) {
        debug_assert!(self.tracks.remove(key).is_some());
    }

    fn apply(&mut self, change: &PlanChange, touched: &mut usize) {
        if change.remove {
            debug_assert!(!change.create);
            debug_assert!(self.get(change.key).is_some());
            self.remove(change.key);
            *touched = touched.saturating_add(1);
            return;
        }
        if change.create {
            debug_assert!(self.get(change.key).is_none());
            self.insert(change.key, FlatTrackPlan::default());
            *touched = touched.saturating_add(1);
        }
        let Some(plan) = self.get_mut(change.key) else {
            debug_assert!(false, "a plan change must name a live plan");
            return;
        };
        debug_assert!(
            change
                .remote
                .removed
                .iter()
                .all(|(value, index)| { plan.remote.values.get(*index).copied() == Some(*value) }),
            "remote membership delta does not match plan {:?}: values={:?} removed={:?}",
            change.key,
            plan.remote.values,
            change.remote.removed,
        );
        plan.local.apply(&change.local, touched);
        plan.remote.apply(&change.remote, touched);
        match change.reverse {
            ReverseRouteChange::Unchanged => {}
            ReverseRouteChange::Set(route) => {
                plan.reverse_route = route;
                *touched = touched.saturating_add(1);
            }
        }
    }

    pub(crate) fn apply_change(&mut self, change: &PlanChange, touched: &mut usize) {
        self.apply(change, touched);
    }

    #[cfg(test)]
    pub(crate) fn apply_batch(&mut self, batch: &PlanBatch, touched: &mut usize) {
        for change in &batch.changes {
            self.apply(change, touched);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant_keys(count: usize) -> Vec<ParticipantKey> {
        let mut keys = slotmap::SlotMap::with_key();
        (0..count).map(|_| keys.insert(())).collect()
    }

    #[test]
    fn dense_membership_keeps_the_forwarding_array_compact() {
        let [first, second, third] = participant_keys(3).try_into().unwrap();
        let mut members = DenseMembership::default();
        members.insert(first);
        members.insert(second);
        members.insert(third);
        members.remove(second);
        assert_eq!(members.len(), 2);
        assert!(!members.values().contains(&second));
        assert!(members.values().contains(&first));
        assert!(members.values().contains(&third));
    }

    #[test]
    fn a_plan_batch_touches_only_changed_members() {
        let [first, second, third] = participant_keys(3).try_into().unwrap();
        let mut keys = slotmap::SlotMap::<TrackKey, ()>::with_key();
        let track = keys.insert(());
        let key = track;
        let mut plans = FlatPlans::default();
        let mut initial = PlanBatch::default();
        initial.push(PlanChange {
            key,
            create: true,
            remove: false,
            local: MembershipDelta {
                added: vec![first, second, third],
                removed: Vec::new(),
            },
            remote: MembershipDelta::default(),
            reverse: ReverseRouteChange::Unchanged,
        });
        let mut touched = 0;
        plans.apply_batch(&initial, &mut touched);
        let mut update = PlanBatch::default();
        update.push(PlanChange {
            key,
            create: false,
            remove: false,
            local: MembershipDelta {
                added: Vec::new(),
                removed: vec![(second, 1)],
            },
            remote: MembershipDelta::default(),
            reverse: ReverseRouteChange::Unchanged,
        });
        plans.apply_batch(&update, &mut touched);
        let plan = plans.get(key).unwrap();
        assert_eq!(plan.local.len(), 2);
        assert!(!plan.local.values().contains(&second));
    }

    #[test]
    fn applying_a_single_membership_change_does_not_scan_the_plan() {
        let keys = participant_keys(1024);
        let mut slots = slotmap::SlotMap::<TrackKey, ()>::with_key();
        let track = slots.insert(());
        let key = track;
        let mut plans = FlatPlans::default();
        let mut initial = PlanBatch::default();
        initial.push(PlanChange {
            key,
            create: true,
            remove: false,
            local: MembershipDelta {
                added: keys.clone(),
                removed: Vec::new(),
            },
            remote: MembershipDelta::default(),
            reverse: ReverseRouteChange::Unchanged,
        });
        let mut touched = 0;
        plans.apply_batch(&initial, &mut touched);
        assert_eq!(touched, keys.len() + 1);

        let mut update = PlanBatch::default();
        update.push(PlanChange {
            key,
            create: false,
            remove: false,
            local: MembershipDelta {
                added: vec![ParticipantKey::default()],
                removed: vec![(keys[0], 0)],
            },
            remote: MembershipDelta::default(),
            reverse: ReverseRouteChange::Unchanged,
        });
        touched = 0;
        plans.apply_batch(&update, &mut touched);
        assert_eq!(touched, 2);
        assert_eq!(plans.get(key).unwrap().local.len(), keys.len());
    }

    #[test]
    fn control_indexes_compile_swap_remove_positions() {
        let [first, second, third] = participant_keys(3).try_into().unwrap();
        let mut keys = slotmap::SlotMap::<TrackKey, ()>::with_key();
        let track = keys.insert(());
        let key = track;
        let old = FlatTrackPlan {
            local: DenseMembership::from_values([first, second, third]),
            remote: DenseMembership::default(),
            reverse_route: None,
        };
        let next = FlatTrackPlan {
            local: DenseMembership::from_values([first, third]),
            remote: DenseMembership::default(),
            reverse_route: None,
        };
        let change = PlanChange::between(
            key,
            Some(&ControlPlan::from_flat(&old)),
            Some(&ControlPlan::from_flat(&next)),
        );
        assert_eq!(change.local.removed, vec![(second, 1)]);

        let mut plans = FlatPlans::default();
        let mut touched = 0;
        plans.insert(key, old);
        plans.apply(&change, &mut touched);
        assert_eq!(plans.get(key).unwrap().local.values(), &[first, third]);
        assert_eq!(touched, 1);
    }

    #[test]
    fn control_membership_keeps_positions_valid_when_targets_reorder() {
        let [first, second, third] = participant_keys(3).try_into().unwrap();
        let old = ControlPlan::from_flat(&FlatTrackPlan {
            local: DenseMembership::from_values([first, second, third]),
            remote: DenseMembership::default(),
            reverse_route: None,
        });
        let reordered = ControlPlan::from_target(
            &old,
            &FlatTrackPlan {
                local: DenseMembership::from_values([third, first]),
                remote: DenseMembership::default(),
                reverse_route: None,
            },
        );
        let next = ControlPlan::from_target(
            &reordered,
            &FlatTrackPlan {
                local: DenseMembership::from_values([first]),
                remote: DenseMembership::default(),
                reverse_route: None,
            },
        );
        let change = PlanChange::between(
            slotmap::SlotMap::<TrackKey, ()>::with_key().insert(()),
            Some(&reordered),
            Some(&next),
        );
        let mut dense = DenseMembership::from_values([first, third]);
        let mut touched = 0;
        dense.apply(&change.local, &mut touched);
        assert_eq!(dense.values(), &[first]);
        assert_eq!(touched, 1);
    }
}
