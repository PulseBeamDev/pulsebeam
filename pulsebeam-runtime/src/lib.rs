#![cfg_attr(not(test), forbid(unsafe_code))]
//! Runtime primitives underneath the shard model.
//!
//! Shared-state exception, crate-wide: this is the layer that *implements*
//! thread-per-core rather than one that lives inside it. A socket shared
//! between its reader and writer halves, a spawner reachable from any thread,
//! a mailbox endpoint held at both ends — these are the seams shards are built
//! from, and they are shared by construction.
//!
//! The restriction applies above this line. See `docs/thread-per-core.md`;
//! nothing here licenses an `Arc` inside a shard.
#![allow(clippy::disallowed_types)]
#![cfg_attr(
    test,
    allow(
        clippy::unreachable,
        clippy::string_slice,
        clippy::disallowed_methods,
        clippy::float_cmp,
        clippy::arithmetic_side_effects,
    )
)]

pub mod buggify;
pub mod collections;
pub mod fatal;
pub mod mailbox;
pub mod net;
pub mod prelude;
pub mod rand;
pub mod rt;
pub mod sync;
pub mod system;
pub mod testing;

pub const SHARD_TIMER_QUANTUM: std::time::Duration = std::time::Duration::from_micros(100);
