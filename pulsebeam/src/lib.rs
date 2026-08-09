//! # Thread-per-core
//!
//! A shard owns its participants, routes and packet buffers outright and reaches
//! other shards only by message. Shared-state primitives — `Arc`, `Mutex`,
//! `RwLock`, bare atomics — are denied by `clippy.toml` and need an `#[allow]`
//! with a reason. Read `docs/thread-per-core.md` before adding one; the rules
//! there are not style preferences, they are what keeps the design able to span
//! more than one node.
#![deny(clippy::disallowed_types)]

pub mod audio_selector;
mod bitrate;
pub mod clock;
pub mod control;
pub mod entity;
pub mod id;
pub(crate) mod log;
pub mod message;
pub mod node;
pub mod participant;
pub mod route;
pub mod rtp;
pub mod shard;
#[cfg(feature = "sim")]
pub mod sim_metrics;
pub mod track;

#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
}
