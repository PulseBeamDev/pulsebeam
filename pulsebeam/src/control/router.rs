use pulsebeam_runtime::mailbox::{self};
use pulsebeam_runtime::rand::RngCore;
use pulsebeam_runtime::rt::OccupancySnapshot;
use std::hash::{BuildHasher, Hash, Hasher};

use crate::{
    shard::ShardContext,
    shard::worker::{ClusterCommand, ShardCommand},
};

const MAX_LOAD: f64 = 0.7;

pub struct ShardRouter {
    hasher_config: ahash::RandomState,
    shard_contexts: Vec<ShardContext>,
    /// Current load of each shard (e.g., CPU % or Participant Count)
    shard_loads: Vec<f64>,
    shard_occupancy_snapshots: Vec<OccupancySnapshot>,
}

impl ShardRouter {
    pub fn new(shard_contexts: Vec<ShardContext>, rng: &mut impl RngCore) -> Self {
        let shard_count = shard_contexts.len();
        let shard_occupancy_snapshots = shard_contexts
            .iter()
            .map(|ctx| ctx.occupancy.snapshot())
            .collect();

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
        for shard_idx in 0..shard_count {
            let snapshot = self.shard_contexts[shard_idx].occupancy.snapshot();
            let load = snapshot.delta_load(&self.shard_occupancy_snapshots[shard_idx]);
            self.shard_occupancy_snapshots[shard_idx] = snapshot;
            self.update_load(shard_idx, load);
        }
    }

    /// Update the load for a specific shard.
    /// `load` could be CPU usage (0.0 to 1.0) or active participant count.
    pub fn update_load(&mut self, shard_idx: usize, load: f64) {
        debug_assert!(load >= 0.0);
        debug_assert!(load <= 1.0);
        let load = load.min(1.0).max(0.0);
        if let Some(slot) = self.shard_loads.get_mut(shard_idx) {
            *slot = load;
        }
    }

    pub fn try_route<K: Hash>(&self, key: &K) -> Option<usize> {
        let mut best_index = None;
        let mut max_score = -1.0;

        for i in 0..self.shard_loads.len() {
            let load = self.shard_loads[i];

            // If the shard is too hot, it's not even a candidate.
            if load >= MAX_LOAD {
                continue;
            }

            let mut hasher = self.hasher_config.build_hasher();
            key.hash(&mut hasher);
            i.hash(&mut hasher);

            // normalize hash value to 0.0 and 1.0
            let h_val = (hasher.finish() as f64) / (u64::MAX as f64);

            let capacity_factor = 1.0 - load;
            let score = h_val * capacity_factor;

            if score > max_score {
                max_score = score;
                best_index = Some(i);
            }
        }

        // If all shards were > MAX_LOAD, this returns None.
        // The Manager should then send a "Server Busy" to the client.
        best_index
    }

    pub async fn send(&mut self, shard_id: usize, cmd: ShardCommand) {
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

    fn get_mut(&mut self, shard_id: usize) -> &mut mailbox::Sender<ShardCommand> {
        &mut self.shard_contexts[shard_id].command_tx
    }
}
