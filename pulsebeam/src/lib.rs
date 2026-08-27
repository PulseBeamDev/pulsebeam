#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(
    test,
    allow(
        clippy::unreachable,
        clippy::string_slice,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::float_cmp,
        clippy::arithmetic_side_effects,
    )
)]

//! # Thread-per-core
//!
//! A shard owns its participants, routes and packet buffers outright and reaches
//! other shards only by message.
//!
//! Denied here, each with a reason in `clippy.toml` and a level in
//! `[workspace.lints]`: shared-state primitives (`Arc`, `Mutex`, `RwLock`, bare
//! atomics), the ambient clock, unseeded randomness, and blocking calls. Taking
//! one needs an `#[allow]` saying which exception applies.
//!
//! Read `docs/thread-per-core.md` before adding one. Those rules are not style
//! preferences; they are what keeps the design able to span more than one node.

#[cfg(not(target_os = "linux"))]
compile_error!(
    "pulsebeam server requires Linux: its UDP steering path is Aya/eBPF \
     (BPF_PROG_TYPE_SK_REUSEPORT). Portable crates (protocol, core, \
     simulator) build elsewhere; the server binary does not."
);

mod bitrate;
pub mod clock;
pub mod control;
pub mod entity;
pub mod id;
pub(crate) mod keys;
pub(crate) mod log;
pub mod node;
pub mod participant;
pub mod route;
pub mod rtp;
pub mod shard;
pub(crate) mod shard_update;
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
