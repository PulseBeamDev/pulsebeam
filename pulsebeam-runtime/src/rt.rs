use std::time::Duration;

pub use tokio::runtime::Handle;
pub use tokio::task::JoinSet;

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
