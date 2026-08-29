use std::collections::VecDeque;

use pulsebeam_runtime::mailbox;

use crate::keys::ParticipantKey;
use crate::participant::ParticipantEffect;
use crate::shard_update::{ShardUpdate, ShardUpdateOp};

use super::core::ShardExecution;
use super::worker::SHARD_PLAN_OPERATION_BUDGET;

pub(crate) struct ShardUpdateApplication {
    shard_id: crate::id::ShardId,
    generation: u64,
    update_rx: mailbox::Receiver<Box<ShardUpdate>>,
    pending_update: Option<Box<ShardUpdate>>,
    pending_update_lifecycle: bool,
    pending_plan_index: usize,
    pending_participant_effects: VecDeque<(ParticipantKey, ParticipantEffect)>,
}

impl ShardUpdateApplication {
    pub(crate) fn new(
        shard_id: crate::id::ShardId,
        update_rx: mailbox::Receiver<Box<ShardUpdate>>,
    ) -> Self {
        Self {
            shard_id,
            generation: 0,
            update_rx,
            pending_update: None,
            pending_update_lifecycle: false,
            pending_plan_index: 0,
            pending_participant_effects: VecDeque::new(),
        }
    }

    pub(crate) fn apply(&mut self, execution: &mut ShardExecution, budget: usize) -> usize {
        debug_assert!(budget > 0);
        let mut applied = 0;
        while applied < budget {
            if self.pending_update.is_none() {
                let Ok(delta) = self.update_rx.try_recv() else {
                    break;
                };
                self.pending_update = Some(delta);
            }
            let Some(delta) = self.pending_update.take() else {
                debug_assert!(false, "a readable view delta must be retained");
                break;
            };
            if !delta.validate_for(self.shard_id, self.generation) {
                debug_assert_eq!(delta.shard, self.shard_id);
                debug_assert!(
                    delta.generation > self.generation,
                    "update generations arrive strictly newer"
                );
                self.reset_pending();
                continue;
            }
            if !self.pending_update_lifecycle {
                for op in delta
                    .lifecycle
                    .iter()
                    .filter(|op| matches!(op, ShardUpdateOp::InsertParticipant))
                {
                    self.apply_lifecycle_op(execution, op);
                }
                for op in delta
                    .lifecycle
                    .iter()
                    .filter(|op| !is_retire(op) && !matches!(op, ShardUpdateOp::InsertParticipant))
                {
                    self.apply_lifecycle_op(execution, op);
                }
                self.pending_update_lifecycle = true;
            }
            let end = self
                .pending_plan_index
                .saturating_add(SHARD_PLAN_OPERATION_BUDGET)
                .min(delta.plans.len());
            let operations = delta
                .plans
                .get(self.pending_plan_index..end)
                .unwrap_or_default();
            let touched = operations.iter().fold(0usize, |touched, operation| {
                touched.saturating_add(execution.apply_plan(operation))
            });
            self.pending_plan_index = end;
            #[cfg(not(feature = "sim"))]
            let _ = touched;
            #[cfg(feature = "sim")]
            crate::sim_metrics::record_routing_work("plan_entries_touched", touched);
            if self.pending_plan_index < delta.plans.len() {
                self.pending_update = Some(delta);
                break;
            }
            self.apply_pending_participant_effects(execution);
            for (participant, effect) in &delta.participant_effects {
                if !execution.apply_participant_effect(*participant, effect.clone()) {
                    self.pending_participant_effects
                        .push_back((*participant, effect.clone()));
                }
            }
            for op in delta.lifecycle.iter().filter(|op| is_retire(op)) {
                self.apply_lifecycle_op(execution, op);
            }
            self.generation = delta.generation;
            self.apply_pending_participant_effects(execution);
            self.reset_pending();
            applied = applied.saturating_add(1);
        }
        applied
    }

    pub(crate) async fn readable(&mut self) -> Option<()> {
        self.update_rx.readable().await
    }

    pub(crate) fn apply_pending_participant_effects(&mut self, execution: &mut ShardExecution) {
        let pending = std::mem::take(&mut self.pending_participant_effects);
        for (participant, effect) in pending {
            if !execution.apply_participant_effect(participant, effect.clone()) {
                self.pending_participant_effects
                    .push_back((participant, effect));
            }
        }
    }

    fn apply_lifecycle_op(&mut self, execution: &mut ShardExecution, op: &ShardUpdateOp) {
        execution.apply_lifecycle_op(op);
        if let ShardUpdateOp::RemoveParticipant { key } = op {
            self.pending_participant_effects
                .retain(|(participant, _)| *participant != *key);
        }
    }

    fn reset_pending(&mut self) {
        self.pending_update = None;
        self.pending_update_lifecycle = false;
        self.pending_plan_index = 0;
    }
}

fn is_retire(op: &ShardUpdateOp) -> bool {
    matches!(
        op,
        ShardUpdateOp::RetireRoute { .. }
            | ShardUpdateOp::RetireTransport { .. }
            | ShardUpdateOp::RemoveParticipant { .. }
            | ShardUpdateOp::RemoveTrackRuntime { .. }
    )
}
