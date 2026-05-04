use std::time::Duration;

pub use tokio::runtime::Handle;
pub use tokio::task::JoinSet;
mod detector;
pub use detector::LongRunningTaskDetector as Builder;

mod occupancy;
pub use occupancy::{OccupancySnapshot, ShardOccupancy};

mod spawn;
pub use spawn::{
    RuntimeSpawner, SpawnError, TaskFactory, TaskHandle, set_global_runtime_spawner, spawn,
    spawn_control, spawn_data,
};

pub type Runtime = tokio::runtime::Runtime;

pub fn current() -> Handle {
    Handle::current()
}

pub async fn yield_now() {
    tokio::task::yield_now().await;
}

pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await
}
