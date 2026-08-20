use std::collections::VecDeque;

use crate::{id::ShardId, shard::worker::ShardCommand};

#[derive(Debug)]
pub(crate) enum ControllerEvent {
    ShardCommandSent(ShardId, ShardCommand),
}

pub(crate) struct ControllerEventQueue {
    queue: VecDeque<ControllerEvent>,
    shard_count: usize,
}

impl ControllerEventQueue {
    pub(crate) fn new(shard_count: usize) -> Self {
        debug_assert!(shard_count > 0);
        Self {
            queue: VecDeque::with_capacity(64),
            shard_count,
        }
    }

    pub(crate) fn push(&mut self, event: ControllerEvent) {
        self.queue.push_back(event);
    }

    pub(crate) fn pop(&mut self) -> Option<ControllerEvent> {
        self.queue.pop_front()
    }

    pub(crate) fn send(&mut self, shard_id: ShardId, command: ShardCommand) {
        debug_assert!(shard_id.index() < self.shard_count);
        self.push(ControllerEvent::ShardCommandSent(shard_id, command));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_outbox_is_empty() {
        assert!(ControllerEventQueue::new(1).pop().is_none());
    }

    #[test]
    #[should_panic]
    fn an_outbox_requires_a_shard() {
        let _ = ControllerEventQueue::new(0);
    }
}
