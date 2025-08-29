pub use tokio::runtime::Handle;
pub use tokio::task::JoinSet;

pub fn current() -> Handle {
    Handle::current()
}
