use crate::shard::metrics::MetricsSnapshot;
use crate::{id::ShardId, shard::ShardContext, shard::worker::ShardCommand};
use pulsebeam_queue as mailbox;

pub struct ShardRouter {
    shard_contexts: Vec<ShardContext>,
    /// Current load of each shard (e.g., CPU % or Participant Count)
    shard_loads: Vec<f64>,
    shard_occupancy_snapshots: Vec<MetricsSnapshot>,
}

impl ShardRouter {
    pub fn new(shard_contexts: Vec<ShardContext>) -> Self {
        let shard_count = shard_contexts.len();
        let shard_occupancy_snapshots = shard_contexts
            .iter()
            .map(|ctx| ctx.metrics.snapshot())
            .collect();
        assert!(!shard_contexts.is_empty(), "missing shard_contexts");

        Self {
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
    use super::*;

    #[test]
    fn load_observation_tracks_rises_and_decays_without_choosing_routes() {
        let mut router = ShardRouter {
            shard_contexts: vec![],
            shard_loads: vec![0.0],
            shard_occupancy_snapshots: vec![],
        };
        assert_eq!(router.update_load(ShardId::new(0), 1.0), 0.8);
        let decayed = router.update_load(ShardId::new(0), 0.0);
        assert!((decayed - 0.72).abs() < f64::EPSILON);
        assert_eq!(router.update_load(ShardId::new(1), 0.5), 0.0);
    }
}
