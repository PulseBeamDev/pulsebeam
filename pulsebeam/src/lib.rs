pub mod audio_selector;
pub mod control;
pub mod entity;
pub mod id;
pub(crate) mod log;
pub mod message;
pub mod node;
pub mod participant;
pub mod rtp;
pub mod shard;
pub mod track;

#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
}
