use crate::shard::metrics::MetricsSnapshot;
use pulsebeam_runtime::mailbox::{self};
use pulsebeam_runtime::rand::RngCore;
use std::hash::{BuildHasher, Hash, Hasher};

use crate::{
    id::ShardId,
    shard::ShardContext,
    shard::worker::{ClusterCommand, ShardCommand},
};

const MAX_LOAD: f64 = 0.8;

pub struct ShardRouter {
    hasher_config: ahash::RandomState,
    shard_contexts: Vec<ShardContext>,
    /// Current load of each shard (e.g., CPU % or Participant Count)
    shard_loads: Vec<f64>,
    shard_occupancy_snapshots: Vec<MetricsSnapshot>,
}

impl ShardRouter {
    pub fn new(shard_contexts: Vec<ShardContext>, rng: &mut impl RngCore) -> Self {
        let shard_count = shard_contexts.len();
        let shard_occupancy_snapshots = shard_contexts
            .iter()
            .map(|ctx| ctx.metrics.snapshot())
            .collect();
        assert!(!shard_contexts.is_empty(), "missing shard_contexts");

        Self {
            hasher_config: ahash::RandomState::with_seeds(
                rng.next_u64(),
                rng.next_u64(),
                rng.next_u64(),
                rng.next_u64(),
            ),
            shard_contexts,
            shard_loads: vec![0.0; shard_count],
            shard_occupancy_snapshots,
        }
    }

    pub fn poll_loads(&mut self) {
        let shard_count = self.shard_contexts.len();
        let mut peak_load = 0f64;
        let mut total_load = 0f64;

        for shard_idx in 0..shard_count {
            let snapshot = self.shard_contexts[shard_idx].metrics.snapshot();
            let load = snapshot.delta_load(&self.shard_occupancy_snapshots[shard_idx]);
            self.shard_occupancy_snapshots[shard_idx] = snapshot;
            let load = self.update_load(shard_idx, load);
            peak_load = peak_load.max(load);
            total_load += load;
        }

        let mean_load = total_load / shard_count as f64;
        let peak_to_mean = if mean_load > 0.05 {
            peak_load / mean_load
        } else {
            // Not enough load yet
            0.0
        };
        metrics::gauge!("shard_load_peak").set(peak_load);
        metrics::gauge!("shard_load_mean").set(mean_load);
        metrics::gauge!("shard_load_peak_to_mean").set(peak_to_mean);
    }

    pub fn update_load(&mut self, shard_id: impl Into<ShardId>, load: f64) -> f64 {
        let shard_id = shard_id.into();
        debug_assert!(load >= 0.0);
        debug_assert!(load <= 1.0);

        let new_sample = load.clamp(0.0, 1.0);
        if let Some(current_load) = self.shard_loads.get_mut(shard_id.index()) {
            let old_load = *current_load;

            let alpha = if new_sample > old_load { 0.8 } else { 0.1 };
            let smoothed_load = (new_sample * alpha) + (old_load * (1.0 - alpha));

            *current_load = smoothed_load;
            smoothed_load
        } else {
            0.0
        }
    }

    pub fn try_route<K: Hash>(&self, key: &K) -> Option<ShardId> {
        let mut best_index = None;
        let mut max_hash = -1.0;

        for i in 0..self.shard_loads.len() {
            let load = self.shard_loads[i];

            // Protect core real-time execution deadlines
            if load >= MAX_LOAD {
                continue;
            }

            let mut hasher = self.hasher_config.build_hasher();
            key.hash(&mut hasher);
            i.hash(&mut hasher);

            let h_val = (hasher.finish() as f64) / (u64::MAX as f64);

            // Enforce absolute room locality by preferring the highest raw hash mapping
            if h_val > max_hash {
                max_hash = h_val;
                best_index = Some(ShardId::new(i));
            }
        }

        best_index
    }

    pub async fn send(&mut self, shard_id: ShardId, cmd: ShardCommand) {
        self.get_mut(shard_id)
            .send(cmd)
            .await
            .expect("shard to be running");
    }

    pub async fn broadcast(&mut self, cmd: ClusterCommand) {
        for ctx in &self.shard_contexts {
            let cmd = ShardCommand::Cluster(cmd.clone());
            ctx.command_tx.send(cmd).await.expect("shard to be running");
        }
    }

    fn get_mut(&mut self, shard_id: ShardId) -> &mut mailbox::Sender<ShardCommand> {
        &mut self.shard_contexts[shard_id.index()].command_tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to generate a minimal testing router with artificial capacity
    fn setup_test_router(shard_count: usize) -> ShardRouter {
        let rng = pulsebeam_runtime::rand::seeded_rng(42);
        ShardRouter {
            hasher_config: ahash::RandomState::with_seeds(1, 2, 3, 4),
            shard_contexts: vec![], // Omitted to keep tests pure-functional on routing math
            shard_loads: vec![0.0; shard_count],
            shard_occupancy_snapshots: vec![],
        }
    }

    #[test]
    fn test_room_locality_preserved_under_moderate_load() {
        let mut router = setup_test_router(4);
        let room_key = "room-mega-0";

        // Find the natural primary target shard for this room key when idle
        let primary_shard = router.try_route(&room_key).expect("should route");

        // Simulate moderate load on the primary shard (e.g., 50% CPU utilization)
        router.shard_loads[primary_shard.index()] = 0.50;

        // Ensure subsequent joins for the same room key still strictly match the primary shard
        let next_route = router.try_route(&room_key).expect("should route");
        assert_eq!(
            primary_shard, next_route,
            "Room locality was broken before reaching MAX_LOAD!"
        );
    }

    #[test]
    fn test_room_overflows_only_when_max_load_breached() {
        let mut router = setup_test_router(4);
        let room_key = "room-mega-0";

        let primary_shard = router.try_route(&room_key).expect("should route");

        // Push primary shard right up to the line, locality must hold
        router.shard_loads[primary_shard.index()] = 0.79;
        assert_eq!(router.try_route(&room_key).unwrap(), primary_shard);

        // Breach the threshold limit
        router.shard_loads[primary_shard.index()] = 0.80;

        // Ensure routing safely cascades away from the hot shard to a healthy neighbor
        let backup_shard = router.try_route(&room_key).expect("should route to backup");
        assert_ne!(
            primary_shard, backup_shard,
            "Router failed to shed load away from an overloaded core!"
        );
    }

    #[test]
    fn test_returns_none_when_all_shards_overloaded() {
        let mut router = setup_test_router(2);
        let room_key = "room-failed-0";

        router.shard_loads[0] = 0.85;
        router.shard_loads[1] = 0.90;

        assert!(
            router.try_route(&room_key).is_none(),
            "Router should signal busy state when no healthy cores remain"
        );
    }
}
