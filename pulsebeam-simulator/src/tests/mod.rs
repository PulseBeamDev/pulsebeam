pub mod common;
pub mod connectivity;
pub mod data_channel;
pub mod subscriptions;
pub mod video;

#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
}
