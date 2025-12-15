use std::pin::Pin;

use futures_concurrency::future::FutureGroup;
use futures_lite::StreamExt;
use pulsebeam_runtime::{actor, sync::Arc};
use std::future::Future;

pub type ShardTask = Pin<Box<dyn Future<Output = ()> + Send>>;

pub enum ShardMessage {
    AddTask(ShardTask),
}

pub struct ShardActor {
    shard_id: usize,
    tasks: FutureGroup<ShardTask>,
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
                    metrics::histogram!("shard_task_count").record(self.tasks.len() as f64);
                }
            }
        );

        Ok(())
    }

    async fn on_msg(&mut self, _ctx: &mut actor::ActorContext<ShardMessageSet>, msg: ShardMessage) {
        let ShardMessage::AddTask(task) = msg;
        self.tasks.insert(task);
        metrics::histogram!("shard_task_count").record(self.tasks.len() as f64);
    }
}

impl ShardActor {
    pub fn new(shard_id: usize) -> Self {
        Self {
            shard_id,
            tasks: FutureGroup::with_capacity(64),
        }
    }
}

pub type ShardHandle = actor::ActorHandle<ShardMessageSet>;
