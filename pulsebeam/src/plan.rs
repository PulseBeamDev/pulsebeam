#![allow(
    clippy::disallowed_types,
    reason = "left-right is the isolated publication primitive for one shard's plan"
)]

use std::collections::HashMap;

use left_right::Absorb;
use slotmap::SecondaryMap;

use crate::{
    keys::{ParticipantKey, ReliableStreamKey, TrackKey, UnreliableStreamKey},
    route::RouteHandle,
};

#[derive(Debug, Clone)]
pub(crate) struct MembershipDelta<K> {
    pub added: Vec<K>,
    pub removed: Vec<K>,
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
    pub key: PlanKey,
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
        key: PlanKey,
        old: Option<&FlatTrackPlan>,
        new: Option<&FlatTrackPlan>,
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
        let empty = FlatTrackPlan::default();
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
    positions: HashMap<K, usize>,
}

impl<K> Default for DenseMembership<K> {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            positions: HashMap::new(),
        }
    }
}

impl<K> DenseMembership<K>
where
    K: Copy + Eq + std::hash::Hash,
{
    pub(crate) fn from_values(values: impl IntoIterator<Item = K>) -> Self {
        let mut membership = Self::default();
        for value in values {
            membership.insert(value);
        }
        membership
    }

    pub(crate) fn insert(&mut self, value: K) {
        if self.positions.contains_key(&value) {
            debug_assert!(false, "a compiled membership cannot contain duplicates");
            return;
        }
        let index = self.values.len();
        self.values.push(value);
        let previous = self.positions.insert(value, index);
        debug_assert!(previous.is_none(), "the dense membership index is unique");
    }

    pub(crate) fn remove(&mut self, value: K) {
        let Some(index) = self.positions.remove(&value) else {
            debug_assert!(false, "a compiled membership removal must name a member");
            return;
        };
        let Some(last) = self.values.pop() else {
            debug_assert!(false, "the dense membership index cannot outlive its value");
            return;
        };
        if index < self.values.len() {
            let Some(slot) = self.values.get_mut(index) else {
                debug_assert!(false, "the dense membership index must be in bounds");
                return;
            };
            *slot = last;
            let previous = self.positions.insert(last, index);
            debug_assert_eq!(previous, Some(self.values.len()));
        } else {
            debug_assert_eq!(index, self.values.len());
            debug_assert!(last == value);
        }
    }

    pub(crate) fn apply(&mut self, delta: &MembershipDelta<K>, touched: &mut usize) {
        for &value in &delta.removed {
            self.remove(value);
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

    fn delta_to(&self, next: &Self) -> MembershipDelta<K> {
        MembershipDelta {
            removed: self
                .values
                .iter()
                .copied()
                .filter(|value| !next.positions.contains_key(value))
                .collect(),
            added: next
                .values
                .iter()
                .copied()
                .filter(|value| !self.positions.contains_key(value))
                .collect(),
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PlanKey {
    Track(TrackKey),
    Unreliable(UnreliableStreamKey),
    Reliable(ReliableStreamKey),
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
    unreliable: SecondaryMap<UnreliableStreamKey, FlatTrackPlan>,
    reliable: SecondaryMap<ReliableStreamKey, FlatTrackPlan>,
}

impl FlatPlans {
    pub(crate) fn get(&self, key: PlanKey) -> Option<&FlatTrackPlan> {
        match key {
            PlanKey::Track(key) => self.tracks.get(key),
            PlanKey::Unreliable(key) => self.unreliable.get(key),
            PlanKey::Reliable(key) => self.reliable.get(key),
        }
    }

    fn get_mut(&mut self, key: PlanKey) -> Option<&mut FlatTrackPlan> {
        match key {
            PlanKey::Track(key) => self.tracks.get_mut(key),
            PlanKey::Unreliable(key) => self.unreliable.get_mut(key),
            PlanKey::Reliable(key) => self.reliable.get_mut(key),
        }
    }

    fn insert(&mut self, key: PlanKey, plan: FlatTrackPlan) {
        match key {
            PlanKey::Track(key) => {
                let previous = self.tracks.insert(key, plan);
                debug_assert!(previous.is_none());
            }
            PlanKey::Unreliable(key) => {
                let previous = self.unreliable.insert(key, plan);
                debug_assert!(previous.is_none());
            }
            PlanKey::Reliable(key) => {
                let previous = self.reliable.insert(key, plan);
                debug_assert!(previous.is_none());
            }
        }
    }

    fn remove(&mut self, key: PlanKey) {
        match key {
            PlanKey::Track(key) => debug_assert!(self.tracks.remove(key).is_some()),
            PlanKey::Unreliable(key) => debug_assert!(self.unreliable.remove(key).is_some()),
            PlanKey::Reliable(key) => debug_assert!(self.reliable.remove(key).is_some()),
        }
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

    pub(crate) fn apply_batch(&mut self, batch: &PlanBatch, touched: &mut usize) {
        for change in &batch.changes {
            self.apply(change, touched);
        }
    }
}

impl Absorb<PlanBatch> for FlatPlans {
    fn absorb_first(&mut self, operation: &mut PlanBatch, _: &Self) {
        let mut touched = 0;
        self.apply_batch(operation, &mut touched);
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_routing_work("plan_entries_touched", touched);
    }

    fn absorb_second(&mut self, operation: PlanBatch, _: &Self) {
        let mut touched = 0;
        self.apply_batch(&operation, &mut touched);
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_routing_work("plan_entries_touched", touched);
    }

    fn sync_with(&mut self, first: &Self) {
        *self = first.clone();
    }
}

pub(crate) struct FlatPlanPublisher {
    writer: left_right::WriteHandle<FlatPlans, PlanBatch>,
    reader: PlanReader,
}

pub(crate) type PlanReader = left_right::ReadHandle<FlatPlans>;

impl FlatPlanPublisher {
    pub(crate) fn new() -> Self {
        let (writer, reader) = left_right::new_from_empty(FlatPlans::default());
        Self { writer, reader }
    }

    pub(crate) fn append(&mut self, batch: PlanBatch) {
        if !batch.is_empty() {
            self.writer.append(batch);
        }
    }

    pub(crate) fn publish(&mut self) {
        self.writer.publish();
    }

    #[cfg(test)]
    pub(crate) fn read(&self) -> Option<left_right::ReadGuard<'_, FlatPlans>> {
        self.reader.enter()
    }

    pub(crate) fn reader(&self) -> PlanReader {
        self.reader.clone()
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
        let key = PlanKey::Track(track);
        let mut publisher = FlatPlanPublisher::new();
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
        publisher.append(initial);
        publisher.publish();
        let mut update = PlanBatch::default();
        update.push(PlanChange {
            key,
            create: false,
            remove: false,
            local: MembershipDelta {
                added: Vec::new(),
                removed: vec![second],
            },
            remote: MembershipDelta::default(),
            reverse: ReverseRouteChange::Unchanged,
        });
        publisher.append(update);
        publisher.publish();
        let plans = publisher.read().unwrap();
        let plan = plans.get(key).unwrap();
        assert_eq!(plan.local.len(), 2);
        assert!(!plan.local.values().contains(&second));
    }

    #[test]
    fn applying_a_single_membership_change_does_not_scan_the_plan() {
        let keys = participant_keys(1024);
        let mut slots = slotmap::SlotMap::<TrackKey, ()>::with_key();
        let track = slots.insert(());
        let key = PlanKey::Track(track);
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
                removed: vec![keys[0]],
            },
            remote: MembershipDelta::default(),
            reverse: ReverseRouteChange::Unchanged,
        });
        touched = 0;
        plans.apply_batch(&update, &mut touched);
        assert_eq!(touched, 2);
        assert_eq!(plans.get(key).unwrap().local.len(), keys.len());
    }
}
