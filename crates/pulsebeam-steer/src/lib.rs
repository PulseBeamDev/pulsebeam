#![no_std]
#![doc = include_str!("../README.md")]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
    )
)]

#[cfg(test)]
extern crate std;

pub mod classify;
pub mod steer;
pub mod stun;
pub mod ufrag;

pub use classify::{Bootstrap, DropReason, NodeScope, classify_bootstrap};
pub use steer::{FlowKey, FlowLookup, Verdict, steer_flow};
pub use ufrag::{IceUfrag, UfragError};

pub const ROUTE_SLOT_BITS: u32 = 20;
pub const ROUTE_SHARD_BITS: u32 = 12;

const ROUTE_SLOT_MASK: u32 = (1u32 << ROUTE_SLOT_BITS) - 1;
const ROUTE_SHARD_MASK: u32 = (1u32 << ROUTE_SHARD_BITS) - 1;

const _: () = assert!(ROUTE_SLOT_BITS + ROUTE_SHARD_BITS == u32::BITS);

/// A client transport candidate packed as `shard(12) | slot(20)`.
/// Decoding does not establish live ownership, epoch validity or ICE authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransportRoute(u32);

impl TransportRoute {
    pub const MAX_SLOT: u32 = ROUTE_SLOT_MASK;
    pub const MAX_SHARD: u32 = ROUTE_SHARD_MASK;

    pub const fn try_new(shard: u16, slot: u32) -> Option<Self> {
        if slot > Self::MAX_SLOT || (shard as u32) > Self::MAX_SHARD {
            return None;
        }
        Some(Self(((shard as u32) << ROUTE_SLOT_BITS) | slot))
    }

    /// Every packed bit pattern is syntactically valid, but not necessarily live.
    pub const fn from_raw(bits: u32) -> Self {
        Self(bits)
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "shifting out the slot leaves only 12 bits"
    )]
    pub const fn shard(self) -> u16 {
        (self.0 >> ROUTE_SLOT_BITS) as u16
    }

    pub const fn slot(self) -> u32 {
        self.0 & ROUTE_SLOT_MASK
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_bits_and_checked_boundaries_are_stable() {
        for (shard, slot, bits) in [
            (0, 0, 0),
            (0, 0xFFFFF, 0x000F_FFFF),
            (0xFFF, 0, 0xFFF0_0000),
            (0xFFF, 0xFFFFF, u32::MAX),
            (0xAAA, 0xF0F0F, 0xAAAF_0F0F),
        ] {
            let route = TransportRoute::try_new(shard, slot).unwrap();
            assert_eq!(route.get(), bits);
            assert_eq!(route.shard(), shard);
            assert_eq!(route.slot(), slot);
            assert_eq!(TransportRoute::from_raw(bits), route);
        }
        assert!(TransportRoute::try_new(0x1000, 0).is_none());
        assert!(TransportRoute::try_new(0, 0x100000).is_none());
    }
}
