use futures::future::BoxFuture;
use once_cell::sync::OnceCell;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// A closure executed in the target local runtime.
pub type TaskFactory = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send + 'static>;

/// Handle to observe the execution completion of a spawned task.
pub struct TaskHandle {
    rx: oneshot::Receiver<()>,
}

impl TaskHandle {
    pub async fn wait(self) {
        let _ = self.rx.await;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("runtime not initialized")]
    NotInitialized,
    #[error("target thread unreachable")]
    TargetUnavailable,
}

pub struct SpawnRequest {
    pub(crate) factory: TaskFactory,
    pub(crate) completion: oneshot::Sender<()>,
}

pub struct RuntimeSpawner {
    control_tx: mpsc::UnboundedSender<SpawnRequest>,
    data_txs: Vec<mpsc::UnboundedSender<SpawnRequest>>,
}

impl RuntimeSpawner {
    pub fn new(
        control_tx: mpsc::UnboundedSender<SpawnRequest>,
        data_txs: Vec<mpsc::UnboundedSender<SpawnRequest>>,
    ) -> Self {
        Self { control_tx, data_txs }
    }

    pub fn data_workers(&self) -> usize {
        self.data_txs.len().max(1)
    }

    pub async fn control_dispatcher(mut rx: mpsc::UnboundedReceiver<SpawnRequest>) {
        while let Some(req) = rx.recv().await {
            let SpawnRequest { factory, completion } = req;
            tokio::task::spawn_local(async move {
                (factory)().await;
                let _ = completion.send(());
            });
        }
    }

    pub async fn worker_dispatcher(mut rx: mpsc::UnboundedReceiver<SpawnRequest>) {
        while let Some(req) = rx.recv().await {
            let SpawnRequest { factory, completion } = req;
            tokio::task::spawn_local(async move {
                (factory)().await;
                let _ = completion.send(());
            });
        }
    }

    pub fn spawn_control(&self, factory: TaskFactory) -> Result<TaskHandle, SpawnError> {
        let (tx, rx) = oneshot::channel();
        let req = SpawnRequest { factory, completion: tx };
        self.control_tx
            .send(req)
            .map_err(|_| SpawnError::TargetUnavailable)?;
        Ok(TaskHandle { rx })
    }

    pub fn spawn_data(&self, worker: usize, factory: TaskFactory) -> Result<TaskHandle, SpawnError> {
        let (tx, rx) = oneshot::channel();
        let req = SpawnRequest { factory, completion: tx };

        if self.data_txs.is_empty() {
            // Single-thread mode: data tasks run on control runtime.
            self.control_tx
                .send(req)
                .map_err(|_| SpawnError::TargetUnavailable)?;
            return Ok(TaskHandle { rx });
        }

        let target = self
            .data_txs
            .get(worker % self.data_txs.len())
            .ok_or(SpawnError::TargetUnavailable)?;
        target.send(req).map_err(|_| SpawnError::TargetUnavailable)?;
        Ok(TaskHandle { rx })
    }

    pub fn spawn(&self, thread: usize, factory: TaskFactory) -> Result<TaskHandle, SpawnError> {
        if thread == 0 || self.data_txs.is_empty() {
            self.spawn_control(factory)
        } else {
            self.spawn_data((thread - 1) % self.data_txs.len(), factory)
        }
    }
}


static GLOBAL_SPAWNER: OnceCell<Arc<RuntimeSpawner>> = OnceCell::new();

pub fn set_global_runtime_spawner(spawner: RuntimeSpawner) -> Result<(), SpawnError> {
    GLOBAL_SPAWNER
        .set(Arc::new(spawner))
        .map_err(|_| SpawnError::TargetUnavailable)
}

fn global_spawner() -> Result<&'static Arc<RuntimeSpawner>, SpawnError> {
    GLOBAL_SPAWNER.get().ok_or(SpawnError::NotInitialized)
}

pub fn spawn(thread: usize, factory: TaskFactory) -> Result<TaskHandle, SpawnError> {
    global_spawner()?.spawn(thread, factory)
}

pub fn spawn_control(factory: TaskFactory) -> Result<TaskHandle, SpawnError> {
    global_spawner()?.spawn_control(factory)
}

pub fn spawn_data(worker: usize, factory: TaskFactory) -> Result<TaskHandle, SpawnError> {
    global_spawner()?.spawn_data(worker, factory)
}

