use crate::shard::metrics::MetricsSnapshot;
use pulsebeam_runtime::mailbox::{self};
use pulsebeam_runtime::rand::RngCore;
use std::hash::{BuildHasher, Hash, Hasher};

use crate::{id::ShardId, shard::ShardContext, shard::worker::ShardCommand};

#[cfg(test)]
const MAX_LOAD: f64 = 0.8;

pub struct ShardRouter {
    hasher_config: ahash::RandomState,
    shard_contexts: Vec<ShardContext>,
    /// Current load of each shard (e.g., CPU % or Participant Count)
    shard_loads: Vec<f64>,
    shard_occupancy_snapshots: Vec<MetricsSnapshot>,
}

impl ShardRouter {
    pub fn new(shard_contexts: Vec<ShardContext>) -> Self {
        let mut rng = pulsebeam_runtime::rand::os_rng();
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
            let (Some(ctx), Some(previous)) = (
                self.shard_contexts.get(shard_idx),
                self.shard_occupancy_snapshots.get_mut(shard_idx),
            ) else {
                debug_assert!(false, "shard {shard_idx} has no context or snapshot");
                continue;
            };
            let snapshot = ctx.metrics.snapshot();
            let load = snapshot.delta_load(previous);
            *previous = snapshot;
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

    pub fn shard_count(&self) -> usize {
        self.shard_loads.len()
    }

    #[cfg(test)]
    pub fn try_route<K: Hash>(&self, key: &K) -> Option<ShardId> {
        let mut best_index = None;
        let mut max_hash = -1.0;

        for (i, &load) in self.shard_loads.iter().enumerate() {
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

    pub fn stable_route<K: Hash>(&self, key: &K) -> Option<ShardId> {
        let mut best_index = None;
        let mut best_hash = 0;
        for index in 0..self.shard_loads.len() {
            let hash = self.hash_for(key, index);
            if best_index.is_none() || hash > best_hash {
                best_hash = hash;
                best_index = Some(ShardId::new(index));
            }
        }
        best_index
    }

    fn hash_for<K: Hash>(&self, key: &K, index: usize) -> u64 {
        let mut hasher = self.hasher_config.build_hasher();
        key.hash(&mut hasher);
        index.hash(&mut hasher);
        hasher.finish()
    }

    pub fn try_send(
        &mut self,
        shard_id: ShardId,
        cmd: ShardCommand,
    ) -> Result<(), Box<mailbox::TrySendError<ShardCommand>>> {
        // A shard that has gone away cannot be told anything; the controller
        // keeps serving the others rather than following it down.
        self.get_mut(shard_id).try_send(cmd).map_err(Box::new)
    }

    fn get_mut(&mut self, shard_id: ShardId) -> &mut mailbox::Sender<ShardCommand> {
        let Some(ctx) = self.shard_contexts.get_mut(shard_id.index()) else {
            pulsebeam_runtime::fatal!(
                "shard {} is not in this node's shard table",
                shard_id.index()
            )
        };
        &mut ctx.command_tx
    }
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;

    // Helper to generate a minimal testing router with artificial capacity
    fn setup_test_router(shard_count: usize) -> ShardRouter {
        let _rng = pulsebeam_runtime::rand::seeded_rng(42);
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

    #[test]
    fn stable_route_ignores_load() {
        let mut router = setup_test_router(4);
        let room_key = "room-stable";
        let expected = router.stable_route(&room_key).unwrap();
        router.shard_loads[expected.index()] = 1.0;
        assert_eq!(router.stable_route(&room_key), Some(expected));
    }
}
