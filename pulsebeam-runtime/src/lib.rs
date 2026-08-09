//! Runtime primitives underneath the shard model.
//!
//! Shared-state exception, crate-wide: this is the layer that *implements*
//! thread-per-core rather than one that lives inside it. A socket shared
//! between its reader and writer halves, a spawner reachable from any thread,
//! a mailbox endpoint held at both ends — these are the seams shards are built
//! from, and they are shared by construction.
//!
//! The same goes for the clock: `rt::detector` measures whether a task blocked
//! the runtime, which is a fact about the *host* and cannot be observed through
//! a simulated clock.
//!
//! The restriction applies above this line. See `docs/thread-per-core.md`;
//! nothing here licenses an `Arc` or a wall-clock read inside a shard.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

pub mod collections;
pub mod mailbox;
pub mod net;
pub mod prelude;
pub mod rand;
pub mod rt;
pub mod sync;
pub mod system;
pub mod testing;
