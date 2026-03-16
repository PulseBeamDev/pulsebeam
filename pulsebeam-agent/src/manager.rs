use pulsebeam_proto::signaling::VideoRequest;
use std::collections::{HashMap, HashSet};
use str0m::media::Mid;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Subscription {
    pub track_id: String,
    pub height: u32,
}

pub struct SubscriptionManager {
    desired: Vec<Subscription>,
    // current_mid -> Subscription
    active_assignments: HashMap<Mid, Subscription>,
    slots: Vec<Mid>,
}

impl SubscriptionManager {
    pub fn new(slots: Vec<Mid>) -> Self {
        Self {
            desired: Vec::new(),
            active_assignments: HashMap::new(),
            slots,
        }
    }

    pub fn set_desired(&mut self, desired: Vec<Subscription>) {
        self.desired = desired;
    }

    /// Reconciles desired state with available slots.
    /// Implements "Sticky Assignments" algorithm.
    pub fn reconcile(&mut self) -> Vec<VideoRequest> {
        tracing::debug!(
            "reconcile: slots={:?}, desired={:?}, active={:?}",
            self.slots,
            self.desired,
            self.active_assignments
        );
        let mut next_assignments = HashMap::new();
        let mut used_mids = HashSet::new();

        let mut still_desired = self.desired.clone();

        // Pass 1: Sticky Assignments (preserve existing mappings if track is still desired)
        for &mid in &self.slots {
            if let Some(active) = self.active_assignments.get(&mid)
                && let Some(pos) = still_desired
                    .iter()
                    .position(|d| d.track_id == active.track_id)
            {
                let sub = still_desired.remove(pos);
                next_assignments.insert(mid, sub);
                used_mids.insert(mid);
            }
        }

        // Pass 2: New Assignments (fill remaining desired tracks into free slots)
        for sub in still_desired {
            if let Some(&free_mid) = self.slots.iter().find(|m| !used_mids.contains(m)) {
                next_assignments.insert(free_mid, sub);
                used_mids.insert(free_mid);
            } else {
                break; // No more slots
            }
        }

        // Pass 3: Construct VideoRequests and update active state
        let mut requests = Vec::new();

        // We only care about slots that changed or were cleared
        for &mid in &self.slots {
            let next = next_assignments.get(&mid);
            let current = self.active_assignments.get(&mid);

            if next == current {
                continue;
            }

            let Some(sub) = next else { continue };
            requests.push(VideoRequest {
                mid: mid.to_string(),
                track_id: sub.track_id.clone(),
                height: sub.height,
            });
        }

        self.active_assignments = next_assignments;
        requests
    }

    pub fn get_track_for_mid(&self, mid: Mid) -> Option<String> {
        self.active_assignments
            .get(&mid)
            .map(|s| s.track_id.clone())
    }
}
