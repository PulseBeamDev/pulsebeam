use slotmap::SecondaryMap;

use crate::{
    keys::{ParticipantKey, TrackKey},
    route::RouteHandle,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TrackPlan {
    pub local: Vec<ParticipantKey>,
    pub remote: Vec<RouteHandle>,
    pub reverse_route: Option<RouteHandle>,
}

impl TrackPlan {
    pub(crate) fn new(
        local: impl IntoIterator<Item = ParticipantKey>,
        remote: impl IntoIterator<Item = RouteHandle>,
        reverse_route: Option<RouteHandle>,
    ) -> Self {
        Self {
            local: unique(local, "local"),
            remote: unique(remote, "remote"),
            reverse_route,
        }
    }

    fn touched(&self) -> usize {
        self.local
            .len()
            .saturating_add(self.remote.len())
            .saturating_add(usize::from(self.reverse_route.is_some()))
    }
}

fn unique<K>(values: impl IntoIterator<Item = K>, name: &str) -> Vec<K>
where
    K: Copy + Eq + std::hash::Hash + std::fmt::Debug,
{
    let mut result = Vec::new();
    for value in values {
        if result.contains(&value) {
            debug_assert!(false, "a track plan cannot contain duplicate {name} values");
            continue;
        }
        result.push(value);
    }
    result
}

#[derive(Debug, Clone)]
pub(crate) struct PlanOperation {
    pub key: TrackKey,
    pub plan: Option<TrackPlan>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PlanBatch {
    pub operations: Vec<PlanOperation>,
}

impl PlanBatch {
    pub(crate) fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    pub(crate) fn push(&mut self, operation: PlanOperation) {
        self.operations.push(operation);
    }
}

#[derive(Debug, Default)]
pub(crate) struct TrackPlans {
    tracks: SecondaryMap<TrackKey, TrackPlan>,
}

impl TrackPlans {
    pub(crate) fn get(&self, key: TrackKey) -> Option<&TrackPlan> {
        self.tracks.get(key)
    }

    pub(crate) fn apply(&mut self, operation: &PlanOperation, touched: &mut usize) {
        match &operation.plan {
            Some(plan) => {
                debug_assert!(
                    plan.local.iter().enumerate().all(|(index, value)| {
                        plan.local[..index]
                            .iter()
                            .all(|candidate| candidate != value)
                    }),
                    "a track plan cannot contain duplicate local recipients"
                );
                debug_assert!(
                    plan.remote.iter().enumerate().all(|(index, value)| {
                        plan.remote[..index]
                            .iter()
                            .all(|candidate| candidate != value)
                    }),
                    "a track plan cannot contain duplicate remote routes"
                );
                self.tracks.insert(operation.key, plan.clone());
                *touched = touched.saturating_add(plan.touched());
            }
            None => {
                debug_assert!(
                    self.tracks.contains_key(operation.key),
                    "a removed track plan must be owned by the shard"
                );
                if self.tracks.remove(operation.key).is_some() {
                    *touched = touched.saturating_add(1);
                }
            }
        }
    }
}
