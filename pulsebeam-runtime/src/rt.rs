pub use tokio::runtime::Handle;
pub use tokio::task::JoinSet;

pub fn current() -> Handle {
    Handle::current()
}

pub async fn yield_now() {
    tokio::task::yield_now().await;
}
