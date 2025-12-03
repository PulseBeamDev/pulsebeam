use std::pin::Pin;

use futures::StreamExt;
use futures_buffered::FuturesUnorderedBounded;
use pulsebeam_runtime::{actor, sync::Arc};

pub type ShardTask = Pin<Box<dyn futures::Future<Output = ()> + Send>>;

pub enum ShardMessage {
    AddTask(ShardTask),
}

pub struct ShardActor {
    shard_id: usize,
    tasks: FuturesUnorderedBounded<ShardTask>,
}

pub struct ShardMessageSet;

impl actor::MessageSet for ShardMessageSet {
    type Meta = usize;
    type Msg = ShardMessage;
    type ObservableState = usize;
}

impl actor::Actor<ShardMessageSet> for ShardActor {
    fn kind() -> actor::ActorKind {
        "shard"
    }

    fn monitor() -> std::sync::Arc<tokio_metrics::TaskMonitor> {
        static MONITOR: once_cell::sync::Lazy<Arc<tokio_metrics::TaskMonitor>> =
            once_cell::sync::Lazy::new(|| Arc::new(tokio_metrics::TaskMonitor::new()));
        MONITOR.clone()
    }

    fn meta(&self) -> usize {
        self.shard_id
    }

    fn get_observable_state(&self) -> usize {
        self.tasks.len()
    }

    async fn run(
        &mut self,
        _ctx: &mut actor::ActorContext<ShardMessageSet>,
    ) -> Result<(), actor::ActorError> {
        pulsebeam_runtime::actor_loop!(self, _ctx,
            pre_select: {},
            select: {
                Some(_) = self.tasks.next() => {
                    // task completed, shard does nothing else
                }
            }
        );

        Ok(())
    }

    async fn on_msg(&mut self, _ctx: &mut actor::ActorContext<ShardMessageSet>, msg: ShardMessage) {
        let ShardMessage::AddTask(task) = msg;
        self.tasks.push(task);
        metrics::counter!("shard_task_count", "shard_id" => self.shard_id.to_string())
            .absolute(self.tasks.len() as u64);
    }
}

impl ShardActor {
    pub fn new(shard_id: usize) -> Self {
        Self {
            shard_id,
            tasks: FuturesUnorderedBounded::new(512),
        }
    }
}

pub type ShardHandle = actor::ActorHandle<ShardMessageSet>;
