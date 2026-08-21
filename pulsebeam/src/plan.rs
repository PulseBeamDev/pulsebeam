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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RemoteRoutePlan {
    pub handle: RouteHandle,
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
            return;
        }
        let index = self.values.len();
        self.values.push(value);
        let previous = self.positions.insert(value, index);
        debug_assert!(previous.is_none(), "the dense membership index is unique");
    }

    pub(crate) fn remove(&mut self, value: K) {
        let Some(index) = self.positions.remove(&value) else {
            return;
        };
        let Some(last) = self.values.pop() else {
            debug_assert!(false, "the dense membership index cannot outlive its value");
            return;
        };
        if index < self.values.len() {
            let Some(value) = self.values.get_mut(index) else {
                debug_assert!(false, "the dense membership index must be in bounds");
                return;
            };
            *value = last;
            let previous = self.positions.insert(last, index);
            debug_assert_eq!(previous, Some(self.values.len()));
        } else {
            debug_assert_eq!(index, self.values.len());
            debug_assert!(last == value);
        }
    }

    pub(crate) fn values(&self) -> &[K] {
        &self.values
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    pub(crate) fn contains(&self, value: K) -> bool {
        self.positions.contains_key(&value)
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
    pub remote: DenseMembership<RemoteRoutePlan>,
    pub reverse_route: Option<RemoteRoutePlan>,
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
                let _ = self.tracks.insert(key, plan);
            }
            PlanKey::Unreliable(key) => {
                let _ = self.unreliable.insert(key, plan);
            }
            PlanKey::Reliable(key) => {
                let _ = self.reliable.insert(key, plan);
            }
        }
    }

    fn remove(&mut self, key: PlanKey) {
        match key {
            PlanKey::Track(key) => {
                let _ = self.tracks.remove(key);
            }
            PlanKey::Unreliable(key) => {
                let _ = self.unreliable.remove(key);
            }
            PlanKey::Reliable(key) => {
                let _ = self.reliable.remove(key);
            }
        }
    }
}

pub(crate) fn diff(
    key: PlanKey,
    old: Option<&FlatTrackPlan>,
    new: Option<&FlatTrackPlan>,
) -> Vec<FlatPlanOp> {
    let Some(new) = new else {
        return old
            .is_some()
            .then_some(vec![FlatPlanOp::RemovePlan(key)])
            .unwrap_or_default();
    };
    let mut operations = Vec::new();
    if old.is_none() {
        operations.push(FlatPlanOp::CreatePlan(key));
    }
    let empty = FlatTrackPlan::default();
    let old = old.unwrap_or(&empty);
    for &participant in old.local.values() {
        if !new.local.contains(participant) {
            operations.push(FlatPlanOp::RemoveLocal { key, participant });
        }
    }
    for &participant in new.local.values() {
        if !old.local.contains(participant) {
            operations.push(FlatPlanOp::AddLocal { key, participant });
        }
    }
    for &route in old.remote.values() {
        if !new.remote.contains(route) {
            operations.push(FlatPlanOp::RemoveRemote { key, route });
        }
    }
    for &route in new.remote.values() {
        if !old.remote.contains(route) {
            operations.push(FlatPlanOp::AddRemote { key, route });
        }
    }
    if old.reverse_route != new.reverse_route {
        operations.push(FlatPlanOp::SetReverse {
            key,
            route: new.reverse_route,
        });
    }
    operations
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlatPlanOp {
    CreatePlan(PlanKey),
    AddLocal {
        key: PlanKey,
        participant: ParticipantKey,
    },
    RemoveLocal {
        key: PlanKey,
        participant: ParticipantKey,
    },
    AddRemote {
        key: PlanKey,
        route: RemoteRoutePlan,
    },
    RemoveRemote {
        key: PlanKey,
        route: RemoteRoutePlan,
    },
    SetReverse {
        key: PlanKey,
        route: Option<RemoteRoutePlan>,
    },
    RemovePlan(PlanKey),
}

impl FlatPlans {
    fn apply(&mut self, op: FlatPlanOp) {
        let key = match op {
            FlatPlanOp::AddLocal { key, .. }
            | FlatPlanOp::RemoveLocal { key, .. }
            | FlatPlanOp::AddRemote { key, .. }
            | FlatPlanOp::RemoveRemote { key, .. }
            | FlatPlanOp::SetReverse { key, .. }
            | FlatPlanOp::CreatePlan(key)
            | FlatPlanOp::RemovePlan(key) => key,
        };
        if let FlatPlanOp::CreatePlan(key) = op {
            if self.get(key).is_none() {
                self.insert(key, FlatTrackPlan::default());
            }
            return;
        }
        if matches!(op, FlatPlanOp::RemovePlan(_)) {
            self.remove(key);
            return;
        }
        let Some(plan) = self.get_mut(key) else {
            debug_assert!(false, "a plan mutation must follow plan creation");
            return;
        };
        match op {
            FlatPlanOp::AddLocal { participant, .. } => plan.local.insert(participant),
            FlatPlanOp::RemoveLocal { participant, .. } => plan.local.remove(participant),
            FlatPlanOp::AddRemote { route, .. } => plan.remote.insert(route),
            FlatPlanOp::RemoveRemote { route, .. } => plan.remote.remove(route),
            FlatPlanOp::SetReverse { route, .. } => plan.reverse_route = route,
            FlatPlanOp::CreatePlan(_) | FlatPlanOp::RemovePlan(_) => {
                debug_assert!(false, "plan lifecycle operations are handled above");
            }
        }
    }
}

impl Absorb<FlatPlanOp> for FlatPlans {
    fn absorb_first(&mut self, operation: &mut FlatPlanOp, _: &Self) {
        self.apply(*operation);
    }

    fn absorb_second(&mut self, operation: FlatPlanOp, _: &Self) {
        self.apply(operation);
    }

    fn sync_with(&mut self, first: &Self) {
        *self = first.clone();
    }
}

pub(crate) struct FlatPlanPublisher {
    writer: left_right::WriteHandle<FlatPlans, FlatPlanOp>,
    reader: PlanReader,
    #[cfg(test)]
    desired: FlatPlans,
}

pub(crate) type PlanReader = left_right::ReadHandle<FlatPlans>;

impl FlatPlanPublisher {
    pub(crate) fn new() -> Self {
        let (writer, reader) = left_right::new_from_empty(FlatPlans::default());
        Self {
            writer,
            reader,
            #[cfg(test)]
            desired: FlatPlans::default(),
        }
    }

    pub(crate) fn append(&mut self, op: FlatPlanOp) {
        self.writer.append(op);
    }

    #[cfg(test)]
    pub(crate) fn set(&mut self, key: PlanKey, plan: FlatTrackPlan) {
        for op in diff(key, self.desired.get(key), Some(&plan)) {
            self.append(op);
        }
        self.desired.insert(key, plan);
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
        assert_eq!(members.len(), 3);
        assert!(members.contains(second));

        members.remove(second);

        assert_eq!(members.len(), 2);
        assert!(!members.contains(second));
        assert!(members.contains(first));
        assert!(members.contains(third));
    }

    #[test]
    fn duplicate_membership_is_not_added() {
        let [key] = participant_keys(1).try_into().unwrap();
        let mut members = DenseMembership::default();
        members.insert(key);
        members.insert(key);
        assert_eq!(members.values(), &[key]);
    }

    #[test]
    fn left_right_publishes_incremental_membership_without_copying_the_hot_loop() {
        let [first, second] = participant_keys(2).try_into().unwrap();
        let mut tracks = slotmap::SlotMap::<TrackKey, ()>::with_key();
        let track = tracks.insert(());
        let key = PlanKey::Track(track);
        let mut publisher = FlatPlanPublisher::new();

        let mut plan = FlatTrackPlan::default();
        plan.local.insert(first);
        publisher.set(key, plan);
        publisher.publish();
        {
            let plans = publisher.read().expect("the reader is alive");
            let plan = plans.get(key).expect("the track plan is published");
            assert_eq!(plan.local.values(), &[first]);
        }

        let mut plan = FlatTrackPlan::default();
        plan.local.insert(first);
        plan.local.insert(second);
        publisher.set(key, plan);
        publisher.publish();
        let plans = publisher.read().expect("the reader is alive");
        let plan = plans.get(key).expect("the track plan remains published");
        assert_eq!(plan.local.values().len(), 2);
        assert!(plan.local.contains(first));
        assert!(plan.local.contains(second));
    }
}
