//! Owned process execution and movable shard services.

mod config;
mod executor;
mod managed;
mod monitor;
pub use monitor::monitor;
mod process;

pub use config::{ExecutionConfig, Profile};
pub use executor::{
    DataExecutor, ExecutionSnapshot, Migration, MigrationHandle, ServiceSnapshot, WorkerSnapshot,
};
pub use managed::{ManagedIo, ManagedService, Service};
pub use process::{available_workers, block_on, spare_core, tune_control_thread, tune_data_thread};

pub use tokio::task::JoinSet;

pub async fn yield_now() {
    tokio::task::yield_now().await;
}

pub async fn sleep(duration: std::time::Duration) {
    tokio::time::sleep(duration).await;
}
