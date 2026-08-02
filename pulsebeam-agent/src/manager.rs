use pulsebeam_proto::signaling::VideoRequest;
use std::collections::{HashMap, HashSet};
use str0m::media::Mid;

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct VideoSubscription {
    pub track_id: String,
    pub height: u32,
    pub min_height: u32,
    pub priority: u32,
}

impl VideoSubscription {
    pub fn new(track_id: impl Into<String>) -> Self {
        let track_id = track_id.into();
        debug_assert!(!track_id.is_empty());
        Self {
            track_id,
            height: 720,
            min_height: 0,
            priority: 0,
        }
    }

    pub fn target_height(mut self, height: u32) -> Self {
        self.height = height;
        self
    }

    pub fn minimum_height(mut self, height: u32) -> Self {
        self.min_height = height;
        self
    }

    pub fn priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }
}

pub struct SubscriptionManager {
    desired: Vec<VideoSubscription>,
    active_assignments: HashMap<Mid, VideoSubscription>,
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

    pub fn set_desired(&mut self, desired: Vec<VideoSubscription>) {
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

        // Pass 3: Construct VideoRequests and update active state.
        //
        // Every assignment is sent, not just the ones that changed. The SFU treats a
        // `ClientIntent` as a declarative statement of desired state: `VideoAllocator::configure`
        // walks all of its slots and stops any whose mid the intent does not mention. Sending a
        // delta therefore unbinds the slots left out of it - subscribing to a second track would
        // silently drop the first, which is the "allocator doesn't think there's enough for the
        // camera" report. The slot is not starved of bandwidth, it is unbound.
        let mut requests = Vec::new();

        for &mid in &self.slots {
            let Some(sub) = next_assignments.get(&mid) else {
                continue;
            };
            requests.push(VideoRequest {
                mid: mid.to_string(),
                track_id: sub.track_id.clone(),
                target_height: sub.height,
                min_height: sub.min_height,
                priority: sub.priority,
            });
        }

        self.active_assignments = next_assignments;
        requests
    }

    /// Clears the cached active assignments so that the next `reconcile` will
    /// re-send all desired subscriptions.  Call this when a new signaling
    /// session starts (e.g. after a reconnect) because the server-side state
    /// has been reset and no longer knows about previous assignments.
    pub fn reset_active_assignments(&mut self) {
        self.active_assignments.clear();
    }
}
