use pulsebeam_proto::signaling::VideoRequest;
use std::collections::{HashMap, HashSet};
use str0m::media::Mid;

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct VideoSubscription {
    pub track_id: String,
    pub height: u32,
    pub min_height: u32,
    /// Minimum frame rate to keep for a scalable stream (temporal floor). `0` = none.
    pub min_fps: u32,
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
            min_fps: 0,
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

    /// Temporal floor: keep at least this frame rate for a scalable stream.
    pub fn minimum_fps(mut self, fps: u32) -> Self {
        self.min_fps = fps;
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
    pub fn reconcile(&mut self) -> (bool, Vec<VideoRequest>) {
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

        let changed = next_assignments != self.active_assignments;
        let requests = self
            .slots
            .iter()
            .filter_map(|mid| {
                let sub = next_assignments.get(mid)?;
                Some(VideoRequest {
                    mid: mid.to_string(),
                    track_id: sub.track_id.clone(),
                    target_height: sub.height,
                    min_height: sub.min_height,
                    min_fps: sub.min_fps,
                    priority: sub.priority,
                })
            })
            .collect();

        self.active_assignments = next_assignments;
        (changed, requests)
    }

    /// Clears the cached active assignments so that the next `reconcile` will
    /// re-send all desired subscriptions.  Call this when a new signaling
    /// session starts (e.g. after a reconnect) because the server-side state
    /// has been reset and no longer knows about previous assignments.
    pub fn reset_active_assignments(&mut self) {
        self.active_assignments.clear();
    }

    pub fn remove_track(&mut self, track_id: &str) {
        debug_assert!(!track_id.is_empty());
        self.desired
            .retain(|subscription| subscription.track_id != track_id);
        self.active_assignments
            .retain(|_, subscription| subscription.track_id != track_id);
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )] // tests assert by panicking
    use super::*;

    #[test]
    fn changed_subscription_emits_the_complete_assignment() {
        let mids = vec![Mid::from("0"), Mid::from("1")];
        let mut manager = SubscriptionManager::new(mids);
        manager.set_desired(vec![
            VideoSubscription::new("camera"),
            VideoSubscription::new("screen"),
        ]);
        let (changed, requests) = manager.reconcile();
        assert!(changed);
        assert_eq!(requests.len(), 2);

        manager.set_desired(vec![
            VideoSubscription::new("camera").priority(200),
            VideoSubscription::new("screen").priority(10),
        ]);
        let (changed, requests) = manager.reconcile();

        assert!(changed);
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().any(|request| request.track_id == "camera"));
        assert!(requests.iter().any(|request| request.track_id == "screen"));
    }

    #[test]
    fn clearing_every_subscription_is_a_change() {
        let mut manager = SubscriptionManager::new(vec![Mid::from("0")]);
        manager.set_desired(vec![VideoSubscription::new("camera")]);
        let _ = manager.reconcile();

        manager.set_desired(Vec::new());
        let (changed, requests) = manager.reconcile();

        assert!(changed);
        assert!(requests.is_empty());
    }

    #[test]
    fn removed_track_is_not_reselected() {
        let mut manager = SubscriptionManager::new(vec![Mid::from("0")]);
        manager.set_desired(vec![
            VideoSubscription::new("camera").priority(200),
            VideoSubscription::new("screen").priority(10),
        ]);
        let _ = manager.reconcile();

        manager.remove_track("camera");
        let (changed, requests) = manager.reconcile();

        assert!(changed);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].track_id, "screen");
    }
}
