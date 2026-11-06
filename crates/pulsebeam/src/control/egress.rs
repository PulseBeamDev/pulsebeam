use std::collections::VecDeque;

use super::router::ShardRouter;
use crate::{id::ShardId, shard::worker::ShardCommand};
use pulsebeam_queue as mailbox;

pub(super) struct CommandBacklog {
    queues: Vec<VecDeque<ShardCommand>>,
    pending: VecDeque<ShardId>,
    len: usize,
}

impl CommandBacklog {
    pub(super) fn new(shards: usize) -> Self {
        Self {
            queues: (0..shards).map(|_| VecDeque::new()).collect(),
            pending: VecDeque::new(),
            len: 0,
        }
    }

    pub(super) fn push_back(&mut self, (shard, command): (ShardId, ShardCommand)) {
        let Some(queue) = self.queues.get_mut(shard.index()) else {
            pulsebeam_runtime::fatal!("command must target a configured shard");
        };
        if queue.is_empty() {
            self.pending.push_back(shard);
        }
        queue.push_back(command);
        self.len = self.len.saturating_add(1);
    }

    pub(super) fn flush(&mut self, router: &mut ShardRouter, budget: usize) -> (usize, bool) {
        let mut attempts: usize = 0;
        let mut progressed = false;
        let mut blocked = Vec::new();
        for _ in 0..budget {
            let Some(shard) = self.pending.pop_front() else {
                break;
            };
            let Some(queue) = self.queues.get_mut(shard.index()) else {
                pulsebeam_runtime::fatal!("scheduled command must target a configured shard");
            };
            let Some(command) = queue.pop_front() else {
                pulsebeam_runtime::fatal!("scheduled command queue must not be empty");
            };
            attempts = attempts.saturating_add(1);
            match router.try_send(shard, command) {
                Ok(()) => progressed = true,
                Err(error) => match *error {
                    mailbox::TrySendError::Full(command) => {
                        queue.push_front(command);
                        blocked.push(shard);
                        continue;
                    }
                    mailbox::TrySendError::Closed(_) => {
                        tracing::warn!(%shard, "shard command mailbox closed");
                        progressed = true;
                    }
                },
            }
            self.len = self.len.saturating_sub(1);
            if !queue.is_empty() {
                self.pending.push_back(shard);
            }
        }
        self.pending.extend(blocked);
        (attempts, progressed)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    #[cfg(test)]
    pub(super) fn pop_front(&mut self) -> Option<(ShardId, ShardCommand)> {
        let shard = self.pending.pop_front()?;
        let queue = self.queues.get_mut(shard.index())?;
        let command = queue.pop_front()?;
        self.len = self.len.saturating_sub(1);
        if !queue.is_empty() {
            self.pending.push_back(shard);
        }
        Some((shard, command))
    }

    #[cfg(test)]
    pub(super) fn back(&self) -> Option<(ShardId, &ShardCommand)> {
        let shard = *self.pending.back()?;
        Some((shard, self.queues.get(shard.index())?.back()?))
    }

    #[cfg(test)]
    pub(super) fn iter(&self) -> impl Iterator<Item = (ShardId, &ShardCommand)> {
        self.queues.iter().enumerate().flat_map(|(index, queue)| {
            queue
                .iter()
                .map(move |command| (ShardId::new(index), command))
        })
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }
}
