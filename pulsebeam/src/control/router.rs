use pulsebeam_runtime::mailbox::{self, SendError};
use std::hash::{BuildHasher, Hash, Hasher};

use crate::shard::worker::ShardCommand;

const MAX_LOAD: f64 = 0.7;

pub struct ShardRouter {
    hasher_config: ahash::RandomState,
    shard_command_txs: Vec<mailbox::Sender<ShardCommand>>,
    /// Current load of each shard (e.g., CPU % or Participant Count)
    shard_loads: Vec<f64>,
}

impl ShardRouter {
    pub fn new(shards: Vec<mailbox::Sender<ShardCommand>>) -> Self {
        let shard_count = shards.len();
        Self {
            hasher_config: ahash::RandomState::new(),
            shard_command_txs: shards,
            shard_loads: vec![0.0; shard_count],
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

    pub fn try_route<K: Hash>(&self, key: K) -> Option<usize> {
        let mut best_index = 0;
        let mut max_score = -1.0;

        for i in 0..self.shard_loads.len() {
            let mut hasher = self.hasher_config.build_hasher();
            key.hash(&mut hasher);
            i.hash(&mut hasher);

            // normalize hash value to 0.0 and 1.0
            let h_val = (hasher.finish() as f64) / (u64::MAX as f64);

            // Use a small epsilon (1e-6) so that even at 100% load (1.0),
            // the hash still has a tiny bit of influence (the "tie-breaker").
            let capacity_factor = (1.0 - self.shard_loads[i]).max(0.000001);
            let score = h_val * capacity_factor;

            if score > max_score {
                max_score = score;
                best_index = i;
            }
        }

        if self.shard_loads[best_index] > MAX_LOAD {
            return None;
        }

        Some(best_index)
    }

    pub async fn send(
        &mut self,
        shard_id: usize,
        cmd: ShardCommand,
    ) -> Result<(), SendError<ShardCommand>> {
        self.get_mut(shard_id).send(cmd).await
    }

    fn get_mut(&mut self, shard_id: usize) -> &mut mailbox::Sender<ShardCommand> {
        &mut self.shard_command_txs[shard_id]
    }
}
