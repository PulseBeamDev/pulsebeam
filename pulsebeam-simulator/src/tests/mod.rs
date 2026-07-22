pub mod common;
pub mod connection;
pub mod data_channel;
pub mod declarative;
pub mod dynamic;
pub mod network;
pub mod scenario;
pub mod simulcast;
pub mod slots;

#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
}
