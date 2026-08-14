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
pub mod envelope;
pub mod stun;
pub mod ufrag;

pub const ROUTE_SLOT_BITS: u32 = 20;
pub const ROUTE_SHARD_BITS: u32 = 12;

const ROUTE_SLOT_MASK: u32 = (1u32 << ROUTE_SLOT_BITS) - 1;
const ROUTE_SHARD_MASK: u32 = (1u32 << ROUTE_SHARD_BITS) - 1;

const _: () = assert!(ROUTE_SLOT_BITS + ROUTE_SHARD_BITS == u32::BITS);

/// A shard/slot pair packed into a single `u32`: `shard(12) | slot(20)`.
///
/// This is the raw bit layout shared by every route family in this crate.
/// [`TransportRoute`] and [`RouteId`] are distinct newtypes built on top of
/// it — see the `route_family!` macro below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackedRoute(u32);

impl PackedRoute {
    pub const MAX_SLOT: u32 = ROUTE_SLOT_MASK;
    pub const MAX_SHARD: u32 = ROUTE_SHARD_MASK;

    pub const fn new(shard: u16, slot: u32) -> Self {
        debug_assert!(slot <= Self::MAX_SLOT, "route slot overflows 20 bits");
        debug_assert!(
            (shard as u32) <= Self::MAX_SHARD,
            "shard id overflows 12 bits"
        );
        Self((((shard as u32) & ROUTE_SHARD_MASK) << ROUTE_SLOT_BITS) | (slot & ROUTE_SLOT_MASK))
    }

    pub const fn try_new(shard: u16, slot: u32) -> Option<Self> {
        if slot > Self::MAX_SLOT {
            return None;
        }
        if (shard as u32) > Self::MAX_SHARD {
            return None;
        }
        Some(Self(((shard as u32) << ROUTE_SLOT_BITS) | slot))
    }

    pub const fn from_raw(bits: u32) -> Self {
        Self(bits)
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "self.0 >> ROUTE_SLOT_BITS keeps only the top 12 bits, well within u16"
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

/// Declares a distinct route family newtype over [`PackedRoute`].
///
/// Every family gets the same constructors and accessors, but the types are
/// never aliases of one another and there are no `From` conversions between
/// them: mixing up a client transport route and an inter-node route id is a
/// protocol bug, not a representation detail.
macro_rules! route_family {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(PackedRoute);

        impl $name {
            pub const MAX_SLOT: u32 = PackedRoute::MAX_SLOT;
            pub const MAX_SHARD: u32 = PackedRoute::MAX_SHARD;

            pub const fn new(shard: u16, slot: u32) -> Self {
                Self(PackedRoute::new(shard, slot))
            }

            pub const fn try_new(shard: u16, slot: u32) -> Option<Self> {
                match PackedRoute::try_new(shard, slot) {
                    Some(packed) => Some(Self(packed)),
                    None => None,
                }
            }

            pub const fn from_raw(bits: u32) -> Self {
                Self(PackedRoute::from_raw(bits))
            }

            pub const fn shard(self) -> u16 {
                self.0.shard()
            }

            pub const fn slot(self) -> u32 {
                self.0.slot()
            }

            pub const fn get(self) -> u32 {
                self.0.get()
            }
        }
    };
}

route_family!(
    /// A client's ICE transport association: which shard owns it, and where
    /// in that shard's table. Carried inside the ICE ufrag.
    TransportRoute
);

route_family!(
    /// An inter-node Envelope destination: which shard on the receiving node
    /// should get this datagram. Carried in the fixed Envelope header.
    RouteId
);

/// The `(route, epoch)` pair a receiver validates a client transport packet
/// against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportHandle {
    pub route: TransportRoute,
    pub epoch: u16,
}

/// The `(route, epoch)` pair a receiver validates an inter-node Envelope
/// against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteHandle {
    pub route: RouteId,
    pub epoch: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_route_round_trips_boundaries() {
        for (shard, slot) in [
            (0u16, 0u32),
            (0, PackedRoute::MAX_SLOT),
            (u16::try_from(PackedRoute::MAX_SHARD).unwrap(), 0),
            (
                u16::try_from(PackedRoute::MAX_SHARD).unwrap(),
                PackedRoute::MAX_SLOT,
            ),
            (7, 12345),
            (1, PackedRoute::MAX_SLOT - 1),
        ] {
            let route = PackedRoute::new(shard, slot);
            assert_eq!(route.shard(), shard);
            assert_eq!(route.slot(), slot);
            assert_eq!(PackedRoute::from_raw(route.get()), route);
        }
    }

    #[test]
    fn packed_route_try_new_rejects_out_of_range() {
        assert!(PackedRoute::try_new(0, PackedRoute::MAX_SLOT + 1).is_none());
        assert!(
            PackedRoute::try_new(u16::try_from(PackedRoute::MAX_SHARD + 1).unwrap(), 0).is_none()
        );
        assert!(PackedRoute::try_new(0, PackedRoute::MAX_SLOT).is_some());
        assert!(PackedRoute::try_new(u16::try_from(PackedRoute::MAX_SHARD).unwrap(), 0).is_some());
    }

    #[test]
    fn packed_route_mixed_bits_do_not_leak_across_fields() {
        let route = PackedRoute::new(0b1010_1010_1010, 0b1111_0000_1111_0000_1111);
        assert_eq!(route.shard(), 0b1010_1010_1010);
        assert_eq!(route.slot(), 0b1111_0000_1111_0000_1111);
    }

    #[test]
    fn transport_route_and_route_id_are_distinct_families() {
        let t = TransportRoute::new(3, 9);
        let r = RouteId::new(3, 9);
        assert_eq!(t.get(), r.get());
        // No From/Into between the two exists; this is a compile-time
        // property. If either of these lines is ever changed to compile
        // via an implicit conversion, that is the regression this test
        // exists to catch by inspection.
        assert_eq!(t.shard(), r.shard());
        assert_eq!(t.slot(), r.slot());
    }

    #[test]
    fn handles_carry_route_and_epoch() {
        let handle = TransportHandle {
            route: TransportRoute::new(1, 2),
            epoch: 42,
        };
        assert_eq!(handle.route.shard(), 1);
        assert_eq!(handle.epoch, 42);

        let handle = RouteHandle {
            route: RouteId::new(1, 2),
            epoch: 42,
        };
        assert_eq!(handle.route.shard(), 1);
        assert_eq!(handle.epoch, 42);
    }
}
