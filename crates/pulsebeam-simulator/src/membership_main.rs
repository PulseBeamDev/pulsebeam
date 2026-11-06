//! Membership scenarios share the simulator's process-wide clock and RNG hooks.
//! This harness is control-side, not a shard.
#![allow(clippy::disallowed_types)]
#![cfg_attr(
    test,
    allow(clippy::disallowed_methods, clippy::arithmetic_side_effects)
)]

#[cfg(test)]
#[path = "tests/membership.rs"]
mod membership;
#[cfg(test)]
mod sim_clock;
#[cfg(test)]
mod sim_rand;

fn main() {}
