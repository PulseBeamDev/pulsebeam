//! Shared-state exception, crate-wide: The simulator harness. Not a shard.
//! The thread-per-core restriction in `docs/thread-per-core.md` applies to the
//! `pulsebeam` SFU crate.

// Determinism shims. These override libc symbols for the whole process, so they must be linked
// into the test binary rather than living in a helper crate.
#[cfg(test)]
mod sim_clock;
#[cfg(test)]
mod sim_rand;
#[cfg(test)]
mod tests;

fn main() {}
