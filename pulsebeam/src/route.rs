//! Compiled routes and the wire envelope that addresses them.
//!
//! A route id's slot is allocated by the *destination*, because it indexes
//! that destination's table — but the id also carries which shard that
//! table belongs to (see [`RouteId`]), so it is a node-scoped address, not
//! purely a destination-local one. Semantic ids (participant, track, room,
//! topic) never appear on the wire; they survive only in [`RouteNames`] for
//! logs.
//!
//! Overflow is explicit in this module: `#![deny(clippy::arithmetic_side_effects)]`.
//!
//! `overflow-checks` is off in release, so a bare `+` or `-` that goes out of
//! range does not stop — it yields a plausible-looking number that the pacer,
//! the allocator or the jitter estimator then treats as a measurement. This is
//! timestamp and sequence arithmetic, where that number is the whole output, so
//! every operation says which behaviour it wants: `saturating_` to clamp,
//! `checked_` to fall back, `wrapping_` where an era boundary makes wrapping
//! the correct answer.
#![deny(clippy::arithmetic_side_effects)]

use std::collections::VecDeque;
use tokio::time::{Duration, Instant};

use crate::clock::{NtpExpander, NtpTime};
use crate::entity::{ParticipantId, RoomId, TrackId};
use crate::id::ShardId;
use crate::shard::participants::ParticipantKey;
use crate::shard::router::{DataStreamKey, LocalTrackKey, ReliableStreamKey, RoomKey};
use crate::track::Topic;

/// How long a retired slot waits before it can be handed out again.
///
/// `epoch` is the primary guard against a delayed datagram landing on a
/// recycled slot; this is the second line of defence, and what makes the
/// "a slot cannot complete 65,536 generations within one stale-datagram
/// lifetime" invariant trivially true.
///
/// 2 seconds, not the 60 a first-cut number lands on: at 60s, slot
/// consumption under a reconnect storm is `concurrent + installs/sec × 60`,
/// so the quarantine — not the traffic — is what would exhaust a
/// preallocated table. 2s still gives an epoch 1,092× the margin a 2-minute
/// MSL needs (below), which is three orders of magnitude nobody asked for
/// while costing far less of the working set.
pub const ROUTE_QUARANTINE: Duration = Duration::from_secs(2);

/// A delayed duplicate cannot outlive a TCP-style maximum segment lifetime;
/// this is that bound, and what the const assertion below checks the
/// quarantine against.
const ASSUMED_MAX_SEGMENT_LIFETIME_SECS: u64 = 120;

/// Ties [`ROUTE_QUARANTINE`] to the epoch's 16-bit width so tuning one can't
/// silently break the safety margin the other depends on: a slot must
/// complete `u16::MAX` recycles before quarantine could let a stale
/// datagram land on the incarnation that replaced it, and this asserts that
/// takes orders of magnitude longer than any datagram could stay in flight.
const _: () = assert!(
    ROUTE_QUARANTINE.as_secs() * (u16::MAX as u64)
        >= ASSUMED_MAX_SEGMENT_LIFETIME_SECS.saturating_mul(500)
);

/// `shard(12) | slot(20)`, so a packet landing on the wrong shard — which
/// happens on every node, because `SO_REUSEPORT` picks the shard by 5-tuple
/// hash and knows nothing about routes — is steered by reading 12 bits
/// instead of hashing a name back to a shard. 4096 shards, 1M slots per
/// shard: core counts grow predictably, per-shard route counts don't.
///
/// This is the one place route bits are packed and unpacked. The two route
/// families ([`TransportRoute`] and [`RouteId`]) wrap it rather than
/// re-deriving the layout, so a change to the widths cannot leave one family
/// disagreeing with the other, with the ufrag, or with the eBPF classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PackedRoute(u32);

pub const ROUTE_SLOT_BITS: u32 = 20;
pub const ROUTE_SHARD_BITS: u32 = 12;
const ROUTE_SLOT_MASK: u32 = (1 << ROUTE_SLOT_BITS) - 1;
const ROUTE_SHARD_MASK: u32 = (1 << ROUTE_SHARD_BITS) - 1;

const _: () = assert!(ROUTE_SLOT_BITS + ROUTE_SHARD_BITS == u32::BITS);

impl PackedRoute {
    pub const MAX_SLOT: u32 = ROUTE_SLOT_MASK;
    pub const MAX_SHARD: u32 = ROUTE_SHARD_MASK;

    /// Packs a shard and its local slot. `slot` must fit in 20 bits and
    /// `shard` in 12 — both are asserted, not silently truncated, because a
    /// truncated shard id would steer a packet to the wrong worker without
    /// ever failing loudly.
    pub const fn new(shard: ShardId, slot: u32) -> Self {
        debug_assert!(slot <= Self::MAX_SLOT, "route slot overflows 20 bits");
        // Asserted below to fit in 12 bits, so the truncation clippy warns
        // about cannot happen for any shard this format actually supports.
        #[allow(clippy::cast_possible_truncation)]
        let shard_bits = shard.index() as u32;
        debug_assert!(shard_bits <= Self::MAX_SHARD, "shard id overflows 12 bits");
        Self(((shard_bits & ROUTE_SHARD_MASK) << ROUTE_SLOT_BITS) | (slot & ROUTE_SLOT_MASK))
    }

    /// The checked constructor, for anywhere the shard or slot is derived
    /// from configuration or arithmetic rather than from an allocator that
    /// already bounds them.
    pub const fn try_new(shard: ShardId, slot: u32) -> Option<Self> {
        if slot > Self::MAX_SLOT {
            return None;
        }
        #[allow(clippy::cast_possible_truncation)]
        let shard_bits = shard.index() as u32;
        if shard_bits > Self::MAX_SHARD {
            return None;
        }
        Some(Self((shard_bits << ROUTE_SLOT_BITS) | slot))
    }

    /// Reconstructs from a wire representation. Unlike `new`, this trusts the
    /// bits as given — the network handed them to us already packed, and
    /// re-deriving shard/slot from them is exactly `shard()`/`slot()`. Every
    /// bit pattern is a syntactically valid packed route; whether that shard
    /// exists and that slot is live is the receiver's check, not this one's.
    pub const fn from_raw(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn shard(self) -> ShardId {
        ShardId::new((self.0 >> ROUTE_SLOT_BITS) as usize)
    }

    pub const fn slot(self) -> u32 {
        self.0 & ROUTE_SLOT_MASK
    }

    pub const fn index(self) -> usize {
        self.slot() as usize
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Declares one route family over [`PackedRoute`].
///
/// [`TransportRoute`] and [`RouteId`] are the same 32 bits and deliberately
/// not the same type: they address different things (a client's ICE
/// association versus a distributed endpoint), they are allocated from
/// separate slot namespaces, and they appear at different places on the wire.
/// A `From` impl between them would make that boundary a typo away from
/// disappearing, so there is none — crossing it costs an explicit
/// `from_packed`/`packed` pair that greps.
macro_rules! route_family {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(PackedRoute);

        impl $name {
            pub const fn new(shard: ShardId, slot: u32) -> Self {
                Self(PackedRoute::new(shard, slot))
            }

            pub const fn try_new(shard: ShardId, slot: u32) -> Option<Self> {
                match PackedRoute::try_new(shard, slot) {
                    Some(packed) => Some(Self(packed)),
                    None => None,
                }
            }

            pub const fn from_raw(bits: u32) -> Self {
                Self(PackedRoute::from_raw(bits))
            }

            pub const fn from_packed(packed: PackedRoute) -> Self {
                Self(packed)
            }

            pub const fn packed(self) -> PackedRoute {
                self.0
            }

            pub const fn shard(self) -> ShardId {
                self.0.shard()
            }

            pub const fn slot(self) -> u32 {
                self.0.slot()
            }

            pub const fn index(self) -> usize {
                self.0.index()
            }

            pub const fn get(self) -> u32 {
                self.0.get()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0.get())
            }
        }
    };
}

route_family!(
    /// A client's WebRTC transport association, addressing the shard that owns
    /// its `str0m::Rtc`. Travels on the wire inside the ICE ufrag, which is
    /// what lets the kernel steer a client's very first STUN packet to the
    /// owning shard before any userspace lookup exists.
    TransportRoute,
    "tr"
);

route_family!(
    /// A distributed endpoint — an imported track, an audio stream, a data or
    /// reliable stream, or a reverse path. Travels on the wire at a fixed
    /// offset in the inter-node envelope.
    RouteId,
    "rt"
);

/// The only safe way to name a live [`RouteId`].
///
/// A slot is dense and reused, so the u32 alone identifies a *slot*, not an
/// incarnation. Pairing it with the epoch is what makes a delayed datagram
/// addressed to the previous tenant fail closed instead of being delivered to
/// the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RouteHandle {
    pub route: RouteId,
    pub epoch: u16,
}

impl RouteHandle {
    pub const fn new(route: RouteId, epoch: u16) -> Self {
        Self { route, epoch }
    }

    pub const fn shard(self) -> ShardId {
        self.route.shard()
    }
}

impl std::fmt::Display for RouteHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}e{}", self.route, self.epoch)
    }
}

/// [`RouteHandle`]'s transport-side twin — see [`TransportRoute`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransportHandle {
    pub route: TransportRoute,
    pub epoch: u16,
}

impl TransportHandle {
    pub const fn new(route: TransportRoute, epoch: u16) -> Self {
        Self { route, epoch }
    }

    pub const fn shard(self) -> ShardId {
        self.route.shard()
    }
}

impl std::fmt::Display for TransportHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}e{}", self.route, self.epoch)
    }
}

pub use pulsebeam_routing::envelope::{
    ENVELOPE_LEN, ENVELOPE_VERSION, EnvelopeError, EnvelopeType, ROUTE_OFFSET,
};

/// Read the destination shard out of an encoded frame without parsing the
/// payload — the same fixed-offset read the eBPF steering program does, so a
/// userspace fallback cannot disagree with the kernel about where a datagram
/// belongs.
pub fn peek_shard(buf: &[u8]) -> Option<ShardId> {
    pulsebeam_routing::envelope::peek_shard(buf).map(|shard| ShardId::new(usize::from(shard)))
}

/// Read the type tag of an encoded frame, which is what says how to interpret
/// its `extension` and its payload.
pub fn peek_type(buf: &[u8]) -> Result<EnvelopeType, EnvelopeError> {
    decode_wire(buf).map(|env| env.ty)
}

fn decode_wire(buf: &[u8]) -> Result<pulsebeam_routing::envelope::Envelope, EnvelopeError> {
    pulsebeam_routing::envelope::Envelope::decode(buf)
}

fn to_wire_route(route: RouteId) -> pulsebeam_routing::RouteId {
    pulsebeam_routing::RouteId::from_raw(route.get())
}

fn from_wire_route(route: pulsebeam_routing::RouteId) -> RouteId {
    RouteId::from_raw(route.get())
}

/// The typed body of a media frame, above the shared wire header.
///
/// `link_seq` and `playout_ntp32` are the Media type's interpretation of the
/// envelope's `extension` word — the header itself has no idea they exist,
/// which is what lets a new payload family be added without touching the
/// framing or the steering program.
///
/// ```text
/// extension: | link_seq (u32) | playout_ntp32 (u32) |
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaEnvelope {
    pub epoch: u16,
    pub route: RouteId,
    /// Scoped to `(route, epoch)` — a route incarnation. Wrapping `u32`, one
    /// step per transmitted media frame. Hop-local only: it does not survive a
    /// reinstall and a relay does not forward it.
    pub link_seq: u32,
    /// Middle 32 bits of the playout [`NtpTime`].
    pub playout_ntp32: u32,
}

impl MediaEnvelope {
    fn pack_extension(link_seq: u32, playout_ntp32: u32) -> u64 {
        (u64::from(link_seq) << 32) | u64::from(playout_ntp32)
    }

    fn unpack_extension(extension: u64) -> (u32, u32) {
        // Both halves are exactly 32 bits of a 64-bit word, so neither
        // conversion can lose anything.
        #[allow(clippy::cast_possible_truncation)]
        let link_seq = (extension >> 32) as u32;
        #[allow(clippy::cast_possible_truncation)]
        let playout_ntp32 = extension as u32;
        (link_seq, playout_ntp32)
    }

    pub fn encode(&self) -> [u8; ENVELOPE_LEN] {
        pulsebeam_routing::envelope::Envelope {
            ty: EnvelopeType::Media,
            epoch: self.epoch,
            route: to_wire_route(self.route),
            extension: Self::pack_extension(self.link_seq, self.playout_ntp32),
        }
        .encode()
    }

    pub fn decode(buf: &[u8]) -> Result<Self, EnvelopeError> {
        let wire = decode_wire(buf)?;
        if wire.ty != EnvelopeType::Media {
            return Err(EnvelopeError::UnknownType {
                ty: wire.ty.as_u8(),
            });
        }
        let (link_seq, playout_ntp32) = Self::unpack_extension(wire.extension);
        Ok(Self {
            epoch: wire.epoch,
            route: from_wire_route(wire.route),
            link_seq,
            playout_ntp32,
        })
    }
}

/// A frame that carries no timeline: an upstream request travelling back to a
/// publisher, or forward telemetry travelling out to a destination.
///
/// The two are different `EnvelopeType`s over the same header rather than a
/// second header — a reverse message that needs a compact body uses `type` and
/// `extension`, which is why there is only one route offset on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteEnvelope {
    pub ty: EnvelopeType,
    pub epoch: u16,
    pub route: RouteId,
}

impl RouteEnvelope {
    /// An upstream request addressed to a publisher's reverse route.
    pub fn feedback(handle: RouteHandle) -> Self {
        Self {
            ty: EnvelopeType::Feedback,
            epoch: handle.epoch,
            route: handle.route,
        }
    }

    /// Forward telemetry addressed to a destination.
    pub fn telemetry(handle: RouteHandle) -> Self {
        Self {
            ty: EnvelopeType::Telemetry,
            epoch: handle.epoch,
            route: handle.route,
        }
    }

    pub fn encode(&self) -> [u8; ENVELOPE_LEN] {
        debug_assert!(
            matches!(self.ty, EnvelopeType::Feedback | EnvelopeType::Telemetry),
            "a timeline-free frame is feedback or telemetry, not {:?}",
            self.ty
        );
        pulsebeam_routing::envelope::Envelope {
            ty: self.ty,
            epoch: self.epoch,
            route: to_wire_route(self.route),
            extension: 0,
        }
        .encode()
    }

    pub fn decode(buf: &[u8]) -> Result<Self, EnvelopeError> {
        let wire = decode_wire(buf)?;
        match wire.ty {
            EnvelopeType::Feedback | EnvelopeType::Telemetry => Ok(Self {
                ty: wire.ty,
                epoch: wire.epoch,
                route: from_wire_route(wire.route),
            }),
            other => Err(EnvelopeError::UnknownType { ty: other.as_u8() }),
        }
    }
}

/// A sender-side handle to a route installed at a destination.
///
/// Holding one is the *only* way to address a destination, so "media must not
/// be emitted before the receiver route is installed" is structural: the
/// handle does not exist until the destination has installed and acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteRoute {
    pub shard_id: crate::id::ShardId,
    pub route: RouteId,
    pub epoch: u16,
    link_seq: u32,
}

impl RemoteRoute {
    pub fn new(shard_id: crate::id::ShardId, route: RouteId, epoch: u16) -> Self {
        Self {
            shard_id,
            route,
            epoch,
            link_seq: 0,
        }
    }

    /// Build the envelope for the next frame on this route, advancing
    /// `link_seq`. Wrapping, because `link_seq` is modulo 2^32.
    pub fn next_envelope(&mut self, playout: NtpTime) -> MediaEnvelope {
        let env = MediaEnvelope {
            epoch: self.epoch,
            route: self.route,
            link_seq: self.link_seq,
            playout_ntp32: playout.middle32(),
        };
        self.link_seq = self.link_seq.wrapping_add(1);
        env
    }
}

/// Semantic identity, for logs and assertions only. Never read on the hot path.
///
/// A route on the wire names nothing, which is the point and also the reason a
/// route-level fault is otherwise unreadable: `rt41 epoch 3` says nothing about
/// whose stream stopped. This is the only place that mapping survives, so the
/// paths that report a fault holding a live entry render it.
#[derive(Debug, Clone)]
pub(crate) struct RouteNames {
    pub room_id: Option<RoomId>,
    pub origin: ParticipantId,
    pub track_id: Option<TrackId>,
    pub topic: Option<Topic>,
}

impl std::fmt::Display for RouteNames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.origin)?;
        if let Some(room_id) = &self.room_id {
            write!(f, " in {room_id}")?;
        }
        if let Some(track_id) = &self.track_id {
            write!(f, " track {track_id}")?;
        }
        if let Some(topic) = &self.topic {
            write!(f, " topic {topic}")?;
        }
        Ok(())
    }
}

/// What the destination does with a frame that arrives on a route.
///
/// Every variant holds keys, never names — `Copy`, and nothing here is ever
/// hashed to resolve it further. A dispatch function that takes a
/// `RouteAction` apart has nothing left to look up; whatever it needs next
/// comes off the object the key already resolved to.
///
/// Video and data point at a *shard-local* object rather than embedding
/// subscriber membership: local subscribe/unsubscribe is frequent and purely
/// local, while a cluster route is expensive to install. Churn mutates the
/// local object and leaves the route untouched.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RouteAction {
    Video {
        /// The destination's own fanout handle — a dense index, not a name.
        /// Resolving a route hands dispatch something it can use directly,
        /// rather than a `TrackId` it would have to hash back into a map.
        local_track: LocalTrackKey,
    },
    /// One route per (audio stream, destination). Audio is broadcast to a room
    /// rather than explicitly subscribed, so the destination installs this as
    /// soon as it learns the track exists and it has members to deliver to.
    ///
    /// `track` points at a `TrackRoute` the same way `Video::local_track`
    /// does — an audio import gets the same dense fanout entry a video
    /// subscription gets (it never populates `subscribers`/`remote_routes`;
    /// audio's own liveness is the import table, not this entry's
    /// emptiness). Origin and track_id are read off it, never carried here.
    Audio { room: RoomKey, track: LocalTrackKey },
    /// One route per (publisher, topic, destination) on the realtime lane.
    /// The destination installs it whether the local subscription named a
    /// publisher or was a wildcard — wildcards resolve to concrete streams as
    /// publishers are announced.
    Data { stream: DataStreamKey },
    /// The reliable-lane counterpart of `Data`. A separate variant rather
    /// than a `lane: DataLane` field on `Data`, because the two lanes now
    /// resolve through different arenas — the variant *is* the lane.
    Reliable { stream: ReliableStreamKey },
    /// The reverse path for one published stream, resolving at the shard that
    /// owns the publisher.
    ///
    /// Exactly one of these exists per published stream, shared by every
    /// subscribing shard rather than allocated per sender the way media routes
    /// are. Everything on the reverse lane is an idempotent request the sender
    /// repeats if it still needs it, so there is no per-link bookkeeping a
    /// per-sender route would protect — and with a 32-bit id space, paying
    /// `streams x shards` here would make it the largest consumer in the table.
    ///
    /// No `origin` field: `ReverseTarget::Track` and `::Topic` both resolve to
    /// an arena entry that already knows its own publisher.
    Reverse { target: ReverseTarget },
}

/// What a reverse route points at, holding everything the destination needs to
/// act on a frame that names nothing but the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReverseTarget {
    Track {
        /// The publisher's own fanout handle. Encodings live on the
        /// `TrackRoute` this resolves to, in declared order — a frame names
        /// one by index, so the rid itself never travels.
        track: LocalTrackKey,
    },
    Topic {
        stream: ReliableStreamKey,
    },
}

#[derive(Debug)]
pub(crate) struct RouteEntry {
    pub epoch: u16,
    pub action: RouteAction,
    pub names: RouteNames,
    /// Expands the envelope's middle-32 against this route's own reference.
    pub expander: NtpExpander,
    /// Last `link_seq` seen, for hop-local loss/reorder/duplicate accounting.
    pub last_link_seq: Option<u32>,
    pub stats: RouteStats,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RouteStats {
    pub received: u64,
    pub lost: u64,
    pub reordered: u64,
    pub duplicated: u64,
}

impl RouteEntry {
    /// Fold a frame's `link_seq` into hop-local counters.
    ///
    /// Comparison is wrapping: `link_seq` is modulo 2^32, so "newer" means a
    /// positive signed delta, not a larger integer.
    pub fn observe(&mut self, link_seq: u32) {
        self.stats.received = self.stats.received.saturating_add(1);
        let Some(last) = self.last_link_seq else {
            self.last_link_seq = Some(link_seq);
            return;
        };
        // The link sequence is modular; the wrap is how reordering reads as negative.
        let delta = link_seq.wrapping_sub(last).cast_signed();
        match delta {
            0 => self.stats.duplicated = self.stats.duplicated.saturating_add(1),
            d if d > 0 => {
                self.stats.lost = self
                    .stats
                    .lost
                    .saturating_add(u64::from(d.unsigned_abs().saturating_sub(1)));
                self.last_link_seq = Some(link_seq);
            }
            _ => {
                self.stats.reordered = self.stats.reordered.saturating_add(1);
                // A late frame does not move the high-water mark, and its gap
                // was already counted when the newer frame advanced past it.
                self.stats.lost = self.stats.lost.saturating_sub(1);
            }
        }
    }
}

#[derive(Debug)]
enum Slot {
    Free,
    Live(Box<RouteEntry>),
}

// `Free` carries nothing and `Live` is a `Box`, whose pointer is never null —
// so the niche optimization makes this enum exactly the size of the pointer,
// not the pointer plus a discriminant. That is what makes a working set of
// slots cheap enough to preallocate; if this ever regresses (a new variant, a
// wrapper type without the niche), preallocation silently gets `1 << 14`
// times more expensive, so it is checked here rather than trusted.
const _: () = assert!(size_of::<Slot>() == size_of::<usize>());

/// Destination-owned table of installed routes.
///
/// # Id budget
///
/// [`RouteId`] is 32 bits and slots grow monotonically until a retired one
/// clears [`ROUTE_QUARANTINE`], so the table's size is peak concurrent routes
/// plus whatever churned in the last quarantine window. What matters is that
/// every route family stays proportional to something bounded:
///
/// | family    | count per shard        |
/// |-----------|------------------------|
/// | video     | imported tracks        |
/// | audio     | imported audio streams |
/// | data      | imported (publisher, topic, lane) |
/// | feedback  | *published* tracks     |
///
/// Feedback is the one that could easily have been `tracks x shards`: a route
/// per subscribing shard is the obvious symmetry with the forward direction.
/// It is deliberately not, because feedback is latest-wins and keeps no
/// per-link accounting, so a per-sender route would buy nothing and would cost
/// 32x on a 32-shard node.
///
/// There is no policy cap here — the families above are all bounded, so
/// exhaustion means the address space itself ran out: [`RouteId`]'s slot
/// field is 20 bits, `1 << ROUTE_SLOT_BITS` routes, full stop. Nothing to
/// tune, and failing the install at that limit says what happened while
/// there is still a process to say it in.
#[derive(Debug)]
pub(crate) struct RouteTable {
    alloc: SlotAllocator,
    slots: Vec<Slot>,
}

/// Working set preallocated up front so steady-state operation never
/// allocates. A guess, not a bound — [`SlotAllocator::allocate`] grows past it
/// with a `tracing::warn!` naming the shard and the new size, and only fails
/// at the address space's own limit (`1 << ROUTE_SLOT_BITS`).
const ROUTE_TABLE_PREALLOCATED_SLOTS: usize = 1 << 14;

/// Hands out `(slot, epoch)` pairs within one shard's slot namespace.
///
/// Split out from the tables it serves because the two route families need
/// the identical allocation discipline over *separate* namespaces — a
/// [`TransportRoute`] slot and a [`RouteId`] slot may collide numerically and
/// must not share a quarantine queue or an epoch. It is deliberately storage
/// agnostic: it owns the epochs and the quarantine, the table owns the
/// entries, and `allocate` returns a slot the table is responsible for making
/// room for.
#[derive(Debug)]
pub(crate) struct SlotAllocator {
    shard_id: ShardId,
    /// One entry per slot ever handed out; its length *is* the slot high-water
    /// mark, which is what makes a fresh allocation a push.
    epochs: Vec<u16>,
    /// Retired slots, oldest first, with the instant they were retired.
    quarantine: VecDeque<(u32, Instant)>,
    max_slots: u32,
}

impl SlotAllocator {
    pub(crate) fn with_max_slots(shard_id: ShardId, max_slots: u32) -> Self {
        let prealloc = usize::try_from(max_slots)
            .unwrap_or(usize::MAX)
            .min(ROUTE_TABLE_PREALLOCATED_SLOTS);
        Self {
            shard_id,
            epochs: Vec::with_capacity(prealloc),
            quarantine: VecDeque::new(),
            max_slots,
        }
    }

    pub(crate) fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    /// The slot high-water mark — a fresh allocation returns exactly this, so
    /// the caller knows to grow its storage by one rather than overwrite.
    pub(crate) fn high_water(&self) -> usize {
        self.epochs.len()
    }

    /// `Ok((slot, epoch))`. A `slot` equal to the pre-call [`Self::high_water`]
    /// is fresh and the caller must push one entry; anything lower is a
    /// quarantined slot coming back, already bumped to a new epoch.
    pub(crate) fn allocate(&mut self, now: Instant) -> Result<(u32, u16), RouteError> {
        // FIFO from the oldest retirement, so a slot is only reused once no
        // datagram addressed to its previous incarnation could still arrive.
        if let Some(&(slot, retired_at)) = self.quarantine.front()
            && now.saturating_duration_since(retired_at) >= ROUTE_QUARANTINE
        {
            self.quarantine.pop_front();
            let Some(epoch) = self.epochs.get_mut(slot as usize) else {
                debug_assert!(false, "a quarantined slot must be within the namespace");
                return Err(RouteError::Exhausted {
                    max_slots: self.max_slots,
                });
            };
            if *epoch == u16::MAX {
                tracing::warn!(slot, "route epoch wrapped");
            }
            *epoch = epoch.wrapping_add(1);
            return Ok((slot, *epoch));
        }

        let slot = u32::try_from(self.epochs.len()).unwrap_or(u32::MAX);
        if slot >= self.max_slots || slot > PackedRoute::MAX_SLOT {
            return Err(RouteError::Exhausted {
                max_slots: self.max_slots,
            });
        }
        if self.epochs.len() == self.epochs.capacity() {
            let new_capacity = self.epochs.capacity().saturating_mul(2).max(1);
            tracing::warn!(
                shard_id = %self.shard_id,
                new_capacity,
                "route table working set exceeded, growing"
            );
        }
        self.epochs.push(0);
        Ok((slot, 0))
    }

    /// Quarantines a slot. Takes the *slot*, never the packed route: the
    /// quarantine queue indexes this shard's namespace, and a packed route
    /// carries shard bits that would land it far outside it on any shard but 0.
    pub(crate) fn retire(&mut self, slot: u32, now: Instant) {
        debug_assert!(
            (slot as usize) < self.epochs.len(),
            "retired a slot that was never allocated"
        );
        self.quarantine.push_back((slot, now));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteError {
    /// No slot is out of quarantine and the table is at its cap.
    Exhausted { max_slots: u32 },
    /// The envelope named a slot that is free, or an incarnation that is gone.
    Stale { route: RouteId, epoch: u16 },
    /// The envelope named a slot past the end of the table.
    OutOfRange { route: RouteId },
}

impl RouteTable {
    pub fn new(shard_id: ShardId) -> Self {
        Self::with_max_slots(shard_id, PackedRoute::MAX_SLOT.saturating_add(1))
    }

    pub fn with_max_slots(shard_id: ShardId, max_slots: u32) -> Self {
        let alloc = SlotAllocator::with_max_slots(shard_id, max_slots);
        let prealloc = alloc.epochs.capacity();
        Self {
            alloc,
            slots: Vec::with_capacity(prealloc),
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(s, Slot::Live(_)))
            .count()
    }

    #[cfg(test)]
    pub fn get(&self, id: RouteId) -> Option<&RouteEntry> {
        match self.slots.get(id.index()) {
            Some(Slot::Live(entry)) => Some(entry),
            _ => None,
        }
    }

    /// Allocate and install in one step. The caller hands the resulting
    /// `(RouteId, epoch)` to the sender, which may only then emit media.
    pub fn install(
        &mut self,
        action: RouteAction,
        names: RouteNames,
        ntp_ref: NtpTime,
        now: Instant,
    ) -> Result<(RouteId, u16), RouteError> {
        // Exhaustion is the one failure every caller here has written a rollback for and none has
        // ever taken: the table only fills under a participant count no plan reaches. Injecting it
        // is what puts those four recovery paths under test at all.
        if pulsebeam_runtime::buggify!("route table exhausted") {
            return Err(RouteError::Exhausted {
                max_slots: self.alloc.max_slots,
            });
        }
        let fresh = self.alloc.high_water();
        let (slot_idx, epoch) = self.alloc.allocate(now)?;
        let entry = Slot::Live(Box::new(RouteEntry {
            epoch,
            action,
            names,
            expander: NtpExpander::new(ntp_ref),
            last_link_seq: None,
            stats: RouteStats::default(),
        }));
        if slot_idx as usize == fresh {
            self.slots.push(entry);
        } else {
            let Some(slot) = self.slots.get_mut(slot_idx as usize) else {
                debug_assert!(false, "allocate() returned a slot outside the table");
                return Err(RouteError::Exhausted {
                    max_slots: u32::try_from(self.slots.len()).unwrap_or(u32::MAX),
                });
            };
            debug_assert!(
                matches!(slot, Slot::Free),
                "a quarantined slot must still be free"
            );
            *slot = entry;
        }
        Ok((RouteId::new(self.alloc.shard_id(), slot_idx), epoch))
    }

    /// Idempotent: retiring an already-free slot is a no-op, so a redelivered
    /// teardown cannot desync the table.
    ///
    /// `epoch` must match the live incarnation. Every other operation on this
    /// table checks it; retire was the one exception, and it is the one
    /// operation here that destroys state — a teardown for a superseded
    /// incarnation must not retire the one that replaced it.
    pub fn retire(&mut self, id: RouteId, epoch: u16, now: Instant) -> bool {
        debug_assert_eq!(
            id.shard(),
            self.alloc.shard_id(),
            "a route is only retirable at the shard that owns it"
        );
        let Some(slot) = self.slots.get_mut(id.index()) else {
            return false;
        };
        match slot {
            Slot::Live(entry) if entry.epoch == epoch => {}
            _ => return false,
        }
        *slot = Slot::Free;
        self.alloc.retire(id.slot(), now);
        true
    }

    pub fn resolve(&mut self, env: &MediaEnvelope) -> Result<&mut RouteEntry, RouteError> {
        let idx = env.route.index();
        let Some(slot) = self.slots.get_mut(idx) else {
            return Err(RouteError::OutOfRange { route: env.route });
        };
        match slot {
            Slot::Live(entry) if entry.epoch == env.epoch => Ok(entry),
            _ => Err(RouteError::Stale {
                route: env.route,
                epoch: env.epoch,
            }),
        }
    }

    /// The compiled action behind a route, if that incarnation is still live.
    ///
    /// For frames that carry no [`MediaEnvelope`] — feedback has neither a timeline
    /// nor per-link accounting to keep, so it addresses with `(route, epoch)`
    /// alone.
    pub fn resolve_action(&self, route: RouteId, epoch: u16) -> Option<&RouteAction> {
        match self.slots.get(route.index()) {
            Some(Slot::Live(entry)) if entry.epoch == epoch => Some(&entry.action),
            _ => None,
        }
    }

}

/// The shard's table of client ICE associations, addressed by
/// [`TransportRoute`].
///
/// Separate from [`RouteTable`] rather than a variant inside it because the
/// two are different address families with different lifetimes: a transport
/// route is minted once per connection before ICE credentials exist and lives
/// as long as the `Rtc` does, while an endpoint route is installed and retired
/// as subscriptions churn underneath it. Keeping the namespaces apart is what
/// makes the two `Route*` types distinct rather than decorative — a slot
/// number means nothing without knowing which table it indexes.
///
/// Entries hold only a [`ParticipantKey`]: the route and the key are minted
/// together at connection setup and destroyed together at teardown, so a key
/// handed to a live transport route always resolves.
#[derive(Debug)]
pub(crate) struct TransportTable {
    alloc: SlotAllocator,
    slots: Vec<Option<TransportEntry>>,
}

#[derive(Debug, Clone, Copy)]
struct TransportEntry {
    epoch: u16,
    participant: ParticipantKey,
}

impl TransportTable {
    pub fn new(shard_id: ShardId) -> Self {
        Self::with_max_slots(shard_id, PackedRoute::MAX_SLOT.saturating_add(1))
    }

    pub fn with_max_slots(shard_id: ShardId, max_slots: u32) -> Self {
        let alloc = SlotAllocator::with_max_slots(shard_id, max_slots);
        let prealloc = alloc.epochs.capacity();
        Self {
            alloc,
            slots: Vec::with_capacity(prealloc),
        }
    }

    /// Mint a transport route for a participant key that is already reserved.
    pub fn install(
        &mut self,
        participant: ParticipantKey,
        now: Instant,
    ) -> Result<TransportHandle, RouteError> {
        if pulsebeam_runtime::buggify!("transport table exhausted") {
            return Err(RouteError::Exhausted {
                max_slots: self.alloc.max_slots,
            });
        }
        let fresh = self.alloc.high_water();
        let (slot_idx, epoch) = self.alloc.allocate(now)?;
        let entry = Some(TransportEntry { epoch, participant });
        if slot_idx as usize == fresh {
            self.slots.push(entry);
        } else {
            let Some(slot) = self.slots.get_mut(slot_idx as usize) else {
                debug_assert!(false, "allocate() returned a slot outside the table");
                return Err(RouteError::Exhausted {
                    max_slots: u32::try_from(self.slots.len()).unwrap_or(u32::MAX),
                });
            };
            debug_assert!(slot.is_none(), "a quarantined slot must still be free");
            *slot = entry;
        }
        Ok(TransportHandle::new(
            TransportRoute::new(self.alloc.shard_id(), slot_idx),
            epoch,
        ))
    }

    /// Idempotent, and epoch-checked for the same reason [`RouteTable::retire`]
    /// is: a redelivered teardown must not retire the incarnation that
    /// replaced the one it names.
    pub fn retire(&mut self, handle: TransportHandle, now: Instant) -> bool {
        debug_assert_eq!(
            handle.shard(),
            self.alloc.shard_id(),
            "a transport route is only retirable at the shard that owns it"
        );
        let Some(slot) = self.slots.get_mut(handle.route.index()) else {
            return false;
        };
        match slot {
            Some(entry) if entry.epoch == handle.epoch => {}
            _ => return false,
        }
        *slot = None;
        self.alloc.retire(handle.route.slot(), now);
        true
    }

    /// Resolve an arriving client packet to the participant it addresses.
    ///
    /// Ownership is the caller's check, not this one's — a route for another
    /// shard would index this table's slots meaninglessly, so it is asserted
    /// rather than silently missed.
    pub fn resolve(&self, handle: TransportHandle) -> Option<ParticipantKey> {
        debug_assert_eq!(
            handle.shard(),
            self.alloc.shard_id(),
            "a transport route only resolves at the shard that owns it"
        );
        match self.slots.get(handle.route.index()) {
            Some(Some(entry)) if entry.epoch == handle.epoch => Some(entry.participant),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}

/// Lifecycle of one imported stream on a destination shard.
///
/// A cluster route is installed when the *first* local subscriber appears and
/// retired when the last one leaves. Everything in between mutates the local
/// fanout object and leaves the route alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportState {
    Installing,
    Active { route: RouteId, epoch: u16 },
    Retiring { route: RouteId, epoch: u16 },
}

/// Work the caller must perform after a lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportEffect {
    None,
    /// Allocate and install a route at the destination, then acknowledge the
    /// sender. The local fanout object already exists.
    Install,
    Retire {
        route: RouteId,
        epoch: u16,
    },
}

#[derive(Debug)]
struct Import {
    state: ImportState,
    subscribers: usize,
}

/// Tracks [`ImportState`] per imported stream.
///
/// Absent is represented by the absence of an entry, so the state machine
/// cannot leak empty rows.
#[derive(Debug)]
pub struct ImportTable<K> {
    entries: ahash::HashMap<K, Import>,
}

impl<K> Default for ImportTable<K> {
    fn default() -> Self {
        Self {
            entries: ahash::HashMap::default(),
        }
    }
}

impl<K: std::hash::Hash + Eq> ImportTable<K> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self, key: &K) -> Option<ImportState> {
        self.entries.get(key).map(|i| i.state)
    }

    pub fn subscribers(&self, key: &K) -> usize {
        self.entries.get(key).map_or(0, |i| i.subscribers)
    }

    /// Returns [`ImportEffect::Install`] only for the first subscriber. Later
    /// subscribers attach to the pending or active import and produce no
    /// cluster traffic.
    pub fn subscribe(&mut self, key: K) -> ImportEffect {
        match self.entries.get_mut(&key) {
            Some(import) => {
                import.subscribers = import.subscribers.saturating_add(1);
                ImportEffect::None
            }
            None => {
                self.entries.insert(
                    key,
                    Import {
                        state: ImportState::Installing,
                        subscribers: 1,
                    },
                );
                ImportEffect::Install
            }
        }
    }

    /// The destination acknowledged the install.
    ///
    /// If every subscriber left while the install was in flight, the route is
    /// retired immediately rather than cancelled — a cancel would have to race
    /// the acknowledgement, which is unwinnable once a network is involved.
    pub fn on_installed(&mut self, key: &K, route: RouteId, epoch: u16) -> ImportEffect {
        let Some(import) = self.entries.get_mut(key) else {
            // Retirement already completed; nothing references this route.
            return ImportEffect::Retire { route, epoch };
        };
        debug_assert_eq!(
            import.state,
            ImportState::Installing,
            "install acknowledged for an import that was not installing"
        );
        if import.subscribers == 0 {
            import.state = ImportState::Retiring { route, epoch };
            return ImportEffect::Retire { route, epoch };
        }
        import.state = ImportState::Active { route, epoch };
        ImportEffect::None
    }

    /// Returns [`ImportEffect::Retire`] only when the last subscriber leaves an
    /// active import. Leaving during `Installing` defers to [`Self::on_installed`].
    pub fn unsubscribe(&mut self, key: &K) -> ImportEffect {
        let Some(import) = self.entries.get_mut(key) else {
            return ImportEffect::None;
        };
        import.subscribers = import.subscribers.saturating_sub(1);
        if import.subscribers > 0 {
            return ImportEffect::None;
        }
        match import.state {
            ImportState::Active { route, epoch } => {
                import.state = ImportState::Retiring { route, epoch };
                ImportEffect::Retire { route, epoch }
            }
            ImportState::Installing | ImportState::Retiring { .. } => ImportEffect::None,
        }
    }

    /// The install never happened, so the import returns to Absent.
    ///
    /// [`Self::subscribe`] moves to `Installing` before the caller has a route,
    /// which means a failed install would otherwise leave an entry no later
    /// subscribe can advance and no unsubscribe can clear — the stream becomes
    /// permanently undeliverable on this shard. Cross-node an install is a
    /// request to a peer and failing is ordinary, so this is the normal path,
    /// not an exceptional one.
    ///
    /// Only legal while `Installing`: once a route exists, retirement is the
    /// way back, and cancelling would leak it.
    pub fn cancel_install(&mut self, key: &K) {
        let Some(import) = self.entries.get(key) else {
            return;
        };
        debug_assert_eq!(
            import.state,
            ImportState::Installing,
            "cancel_install on an import that already has a route"
        );
        if matches!(import.state, ImportState::Installing) {
            self.entries.remove(key);
        }
    }

    /// Retirement completed. A subscriber that arrived while it was in flight
    /// reinstalls rather than resurrecting the retired route.
    pub fn on_retired(&mut self, key: &K) -> ImportEffect {
        let Some(import) = self.entries.get_mut(key) else {
            return ImportEffect::None;
        };
        if import.subscribers > 0 {
            import.state = ImportState::Installing;
            return ImportEffect::Install;
        }
        self.entries.remove(key);
        ImportEffect::None
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;

    fn names() -> RouteNames {
        RouteNames {
            room_id: Some(RoomId::from_external(
                &crate::entity::ExternalRoomId::new("room1").unwrap(),
            )),
            origin: ParticipantId::from_bytes([7u8; 16]),
            track_id: None,
            topic: None,
        }
    }

    fn action() -> RouteAction {
        RouteAction::Audio {
            room: RoomKey::default(),
            track: LocalTrackKey::default(),
        }
    }

    fn envelope(route: RouteId, epoch: u16) -> MediaEnvelope {
        MediaEnvelope {
            epoch,
            route,
            link_seq: 0,
            playout_ntp32: 0,
        }
    }

    #[test]
    fn route_id_shard_and_slot_round_trip() {
        let id = RouteId::new(ShardId::new(0xABC), 0xF_FFFF);
        assert_eq!(id.shard(), ShardId::new(0xABC));
        assert_eq!(id.slot(), 0xF_FFFF);

        let zero = RouteId::new(ShardId::new(0), 0);
        assert_eq!(zero.shard(), ShardId::new(0));
        assert_eq!(zero.slot(), 0);
    }

    #[test]
    fn route_id_shard_bits_do_not_bleed_into_the_slot() {
        let same_slot_different_shards: Vec<RouteId> = (0..8)
            .map(|shard| RouteId::new(ShardId::new(shard), 42))
            .collect();
        for id in &same_slot_different_shards {
            assert_eq!(id.slot(), 42, "the slot must not depend on the shard bits");
        }
        let mut shards: Vec<_> = same_slot_different_shards
            .iter()
            .map(|id| id.shard())
            .collect();
        shards.dedup();
        assert_eq!(
            shards.len(),
            8,
            "each shard must decode back to a distinct value"
        );
    }

    #[test]
    fn route_id_wire_value_round_trips_through_from_raw() {
        let id = RouteId::new(ShardId::new(3), 100);
        assert_eq!(RouteId::from_raw(id.get()), id);
    }

    #[test]
    fn envelope_is_exactly_sixteen_bytes() {
        assert_eq!(ENVELOPE_LEN, 16);
        assert_eq!(envelope(RouteId::from_raw(1), 1).encode().len(), 16);
    }

    #[test]
    fn envelope_encodes_big_endian_at_documented_offsets() {
        let env = MediaEnvelope {
            epoch: 0x1122,
            route: RouteId::from_raw(0x3344_5566),
            link_seq: 0x7788_99AA,
            playout_ntp32: 0xBBCC_DDEE,
        };
        let bytes = env.encode();
        assert_eq!(
            bytes,
            [
                ENVELOPE_VERSION,
                EnvelopeType::Media.as_u8(),
                0x11,
                0x22,
                0x33,
                0x44,
                0x55,
                0x66,
                0x77,
                0x88,
                0x99,
                0xAA,
                0xBB,
                0xCC,
                0xDD,
                0xEE,
            ]
        );
    }

    /// The reverse lane shares the one header rather than paying for a
    /// second: what used to be a shorter frame is now the same 16 bytes with
    /// an unused `extension`, which is the trade that leaves exactly one
    /// route offset on the wire for the steering program to read.
    #[test]
    fn a_timeline_free_frame_uses_the_same_sixteen_byte_header() {
        let encoded = RouteEnvelope::feedback(RouteHandle::new(RouteId::from_raw(1), 1)).encode();
        assert_eq!(encoded.len(), ENVELOPE_LEN);
        assert_eq!(
            crate::route::peek_shard(&encoded),
            Some(RouteId::from_raw(1).shard()),
            "the route sits at the same offset as it does on a media frame"
        );
    }

    #[test]
    fn reverse_envelope_round_trips() {
        for env in [
            RouteEnvelope::feedback(RouteHandle::new(RouteId::from_raw(u32::MAX), u16::MAX)),
            RouteEnvelope::telemetry(RouteHandle::new(RouteId::from_raw(0), 0)),
        ] {
            assert_eq!(RouteEnvelope::decode(&env.encode()).unwrap(), env);
        }
    }

    /// Cross-node every payload family shares one socket and one header, so a
    /// receiver tells them apart by the `type` byte rather than by guessing a
    /// header length. That is what lets the steering program read `route`
    /// without knowing what the payload is.
    #[test]
    fn the_payload_families_are_distinguishable_on_the_wire() {
        let media = envelope(RouteId::from_raw(3), 4).encode();
        let feedback =
            RouteEnvelope::feedback(RouteHandle::new(RouteId::from_raw(3), 4)).encode();
        let telemetry =
            RouteEnvelope::telemetry(RouteHandle::new(RouteId::from_raw(3), 4)).encode();

        assert_eq!(peek_type(&media).unwrap(), EnvelopeType::Media);
        assert_eq!(peek_type(&feedback).unwrap(), EnvelopeType::Feedback);
        assert_eq!(peek_type(&telemetry).unwrap(), EnvelopeType::Telemetry);

        // The route is at one offset for all of them, whatever the type.
        for encoded in [&media, &feedback, &telemetry] {
            assert_eq!(
                crate::route::peek_shard(encoded),
                Some(RouteId::from_raw(3).shard())
            );
        }

        // And decoding one family as another is refused rather than misread.
        assert_eq!(
            MediaEnvelope::decode(&feedback),
            Err(EnvelopeError::UnknownType {
                ty: EnvelopeType::Feedback.as_u8()
            })
        );
        assert_eq!(
            RouteEnvelope::decode(&media),
            Err(EnvelopeError::UnknownType {
                ty: EnvelopeType::Media.as_u8()
            })
        );
    }

    #[test]
    fn envelope_round_trips() {
        let env = MediaEnvelope {
            epoch: 65_535,
            route: RouteId::from_raw(u32::MAX),
            link_seq: u32::MAX,
            playout_ntp32: u32::MAX,
        };
        assert_eq!(MediaEnvelope::decode(&env.encode()).unwrap(), env);
    }

    #[test]
    fn decode_rejects_truncated_input() {
        let full = envelope(RouteId::from_raw(1), 1).encode();
        for len in 0..ENVELOPE_LEN {
            assert_eq!(
                MediaEnvelope::decode(&full[..len]),
                Err(EnvelopeError::Truncated { len })
            );
        }
    }

    #[test]
    fn decode_rejects_unknown_version_and_unknown_type() {
        let mut bytes = envelope(RouteId::from_raw(1), 1).encode();
        bytes[0] = ENVELOPE_VERSION + 1;
        assert_eq!(
            MediaEnvelope::decode(&bytes),
            Err(EnvelopeError::UnsupportedVersion {
                ver: ENVELOPE_VERSION + 1
            })
        );

        let mut bytes = envelope(RouteId::from_raw(1), 1).encode();
        bytes[1] = 0xff;
        assert_eq!(
            MediaEnvelope::decode(&bytes),
            Err(EnvelopeError::UnknownType { ty: 0xff }),
            "an unrecognised payload family is rejected, not skipped past"
        );
    }

    /// A route this table installs must carry this table's own shard, not a
    /// bare sequential index — that is what lets a misrouted packet be
    /// forwarded by reading the id's high bits alone.
    #[tokio::test(start_paused = true)]
    async fn an_installed_route_carries_its_table_s_shard() {
        let mut table = RouteTable::new(ShardId::new(41));
        let (route, _epoch) = table
            .install(action(), names(), NtpTime::ZERO, Instant::now())
            .unwrap();
        assert_eq!(route.shard(), ShardId::new(41));
    }

    /// Growth is bounded, so a churn storm fails an install rather than
    /// consuming the node's memory until the allocator decides for us.
    #[tokio::test(start_paused = true)]
    async fn a_table_at_its_cap_refuses_instead_of_growing() {
        let mut table = RouteTable::with_max_slots(ShardId::new(0), 2);
        let now = Instant::now();

        for _ in 0..2 {
            table
                .install(action(), names(), NtpTime::ZERO, now)
                .expect("within the cap");
        }

        assert_eq!(
            table
                .install(action(), names(), NtpTime::ZERO, now)
                .unwrap_err(),
            RouteError::Exhausted { max_slots: 2 },
        );

        // Quarantine still returns slots, so the cap bounds concurrency rather
        // than the total number of routes a shard may ever install.
        table.retire(RouteId::from_raw(0), 0, now);
        table
            .install(action(), names(), NtpTime::ZERO, now + ROUTE_QUARANTINE)
            .expect("a quarantined slot comes back");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_epoch_never_resolves_to_a_recycled_slot() {
        let mut table = RouteTable::new(ShardId::new(0));
        let now = Instant::now();
        let (id, epoch) = table
            .install(action(), names(), NtpTime::ZERO, now)
            .unwrap();

        table.retire(id, epoch, now);
        let later = now + ROUTE_QUARANTINE;
        let (id2, epoch2) = table
            .install(action(), names(), NtpTime::ZERO, later)
            .unwrap();

        assert_eq!(id2, id, "the slot should be reused after quarantine");
        assert_ne!(epoch2, epoch, "reuse must bump the epoch");
        assert_eq!(
            table.resolve(&envelope(id, epoch)).err(),
            Some(RouteError::Stale { route: id, epoch })
        );
        assert!(table.resolve(&envelope(id2, epoch2)).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_slot_is_not_reused_inside_its_quarantine() {
        let mut table = RouteTable::new(ShardId::new(0));
        let now = Instant::now();
        let (id, epoch) = table
            .install(action(), names(), NtpTime::ZERO, now)
            .unwrap();
        table.retire(id, epoch, now);

        let too_soon = now + ROUTE_QUARANTINE - Duration::from_millis(1);
        let (id2, _) = table
            .install(action(), names(), NtpTime::ZERO, too_soon)
            .unwrap();
        assert_ne!(id2, id, "must not reuse a slot still in quarantine");
    }

    #[tokio::test(start_paused = true)]
    async fn resolve_rejects_free_and_out_of_range_slots() {
        let mut table = RouteTable::new(ShardId::new(0));
        let now = Instant::now();
        let (id, epoch) = table
            .install(action(), names(), NtpTime::ZERO, now)
            .unwrap();

        let far = RouteId::from_raw(999);
        assert_eq!(
            table.resolve(&envelope(far, 0)).err(),
            Some(RouteError::OutOfRange { route: far })
        );

        table.retire(id, epoch, now);
        assert_eq!(
            table.resolve(&envelope(id, epoch)).err(),
            Some(RouteError::Stale { route: id, epoch })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retire_is_idempotent() {
        let mut table = RouteTable::new(ShardId::new(0));
        let now = Instant::now();
        let (id, epoch) = table
            .install(action(), names(), NtpTime::ZERO, now)
            .unwrap();
        assert!(table.retire(id, epoch, now));
        assert!(
            !table.retire(id, epoch, now),
            "a second retire must be a no-op"
        );
        assert_eq!(table.len(), 0);
    }

    /// A teardown in flight for a superseded incarnation must not retire the
    /// one that replaced it — the epoch check is what makes `retire` safe to
    /// call with a stale handle, the same way `resolve` already is.
    #[tokio::test(start_paused = true)]
    async fn retire_with_a_stale_epoch_does_not_touch_the_live_incarnation() {
        let mut table = RouteTable::with_max_slots(ShardId::new(0), 1);
        let now = Instant::now();
        let (id, epoch) = table
            .install(action(), names(), NtpTime::ZERO, now)
            .unwrap();
        table.retire(id, epoch, now);
        let later = now + ROUTE_QUARANTINE;
        let (id2, epoch2) = table
            .install(action(), names(), NtpTime::ZERO, later)
            .unwrap();
        assert_eq!(id2, id, "single-slot table must reuse the same slot");
        assert_ne!(epoch2, epoch);

        assert!(
            !table.retire(id, epoch, later),
            "retiring the old incarnation's stale epoch must be a no-op"
        );
        assert!(
            table.resolve(&envelope(id2, epoch2)).is_ok(),
            "the live incarnation must survive a stale-epoch retire"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn link_seq_accounting_is_modulo_2_32() {
        let mut table = RouteTable::new(ShardId::new(0));
        let now = Instant::now();
        let (id, epoch) = table
            .install(
                RouteAction::Video {
                    local_track: LocalTrackKey::default(),
                },
                names(),
                NtpTime::ZERO,
                now,
            )
            .unwrap();

        // Straddle the wrap: the successor of u32::MAX is 0, not a 4-billion gap.
        let seqs = [u32::MAX - 2, u32::MAX - 1, u32::MAX, 0, 1];
        for seq in seqs {
            let mut env = envelope(id, epoch);
            env.link_seq = seq;
            let entry = table.resolve(&env).unwrap();
            entry.observe(seq);
        }

        let stats = table.get(id).unwrap().stats;
        assert_eq!(stats.received, seqs.len() as u64);
        assert_eq!(stats.lost, 0, "contiguous across the wrap");
        assert_eq!(stats.duplicated, 0);
        assert_eq!(stats.reordered, 0);
    }

    /// The churn sequence from the design: subscribe, another subscribe before
    /// the ack, an unsubscribe before the ack, the ack, then retire. Exactly
    /// one installation must occur.
    #[test]
    fn churn_before_the_install_ack_installs_exactly_once() {
        let mut imports = ImportTable::new();
        let key = "trk";
        let mut installs = 0;
        let mut retires = 0;

        if imports.subscribe(key) == ImportEffect::Install {
            installs += 1;
        }
        if imports.subscribe(key) == ImportEffect::Install {
            installs += 1;
        }
        assert_eq!(imports.state(&key), Some(ImportState::Installing));

        assert_eq!(imports.unsubscribe(&key), ImportEffect::None);
        assert_eq!(
            imports.subscribers(&key),
            1,
            "one subscriber remains; the import stays pending"
        );

        let (route, epoch) = (RouteId::from_raw(0), 0);
        assert_eq!(imports.on_installed(&key, route, epoch), ImportEffect::None);
        assert_eq!(
            imports.state(&key),
            Some(ImportState::Active { route, epoch })
        );

        if let ImportEffect::Retire { .. } = imports.unsubscribe(&key) {
            retires += 1;
        }
        assert_eq!(imports.on_retired(&key), ImportEffect::None);
        assert_eq!(imports.state(&key), None);

        assert_eq!(installs, 1, "exactly one route installation");
        assert_eq!(retires, 1);
    }

    #[test]
    fn losing_every_subscriber_mid_install_finishes_then_retires() {
        let mut imports = ImportTable::new();
        let key = "trk";
        assert_eq!(imports.subscribe(key), ImportEffect::Install);
        assert_eq!(imports.unsubscribe(&key), ImportEffect::None);

        let (route, epoch) = (RouteId::from_raw(3), 9);
        assert_eq!(
            imports.on_installed(&key, route, epoch),
            ImportEffect::Retire { route, epoch },
            "the install completes, then retires immediately"
        );
        assert_eq!(imports.on_retired(&key), ImportEffect::None);
        assert_eq!(imports.state(&key), None);
    }

    /// A failed install must leave no trace, or the stream is undeliverable on
    /// this shard forever: `Installing` absorbs later subscribes and ignores
    /// unsubscribes, so nothing would ever retry.
    #[test]
    fn an_install_that_failed_can_be_attempted_again() {
        let mut imports = ImportTable::new();
        let key = "trk";
        assert_eq!(imports.subscribe(key), ImportEffect::Install);

        imports.cancel_install(&key);
        assert_eq!(imports.state(&key), None, "back to Absent");
        assert_eq!(imports.subscribers(&key), 0);

        assert_eq!(
            imports.subscribe(key),
            ImportEffect::Install,
            "the next subscriber drives a fresh install"
        );
        let (route, epoch) = (RouteId::from_raw(7), 2);
        assert_eq!(imports.on_installed(&key, route, epoch), ImportEffect::None);
        assert_eq!(
            imports.state(&key),
            Some(ImportState::Active { route, epoch })
        );
    }

    /// Without a rollback the entry is stuck: this pins the two transitions
    /// that would otherwise silently absorb every later attempt.
    #[test]
    fn an_import_wedged_in_installing_absorbs_everything() {
        let mut imports = ImportTable::new();
        let key = "trk";
        imports.subscribe(key);

        assert_eq!(
            imports.subscribe(key),
            ImportEffect::None,
            "a later subscriber attaches to the pending install"
        );
        assert_eq!(imports.unsubscribe(&key), ImportEffect::None);
        assert_eq!(imports.unsubscribe(&key), ImportEffect::None);
        assert_eq!(
            imports.state(&key),
            Some(ImportState::Installing),
            "no unsubscribe can clear an import that never installed"
        );
    }

    #[test]
    fn local_churn_with_a_subscriber_remaining_touches_no_route() {
        let mut imports = ImportTable::new();
        let key = "trk";
        imports.subscribe(key);
        let (route, epoch) = (RouteId::from_raw(1), 0);
        imports.on_installed(&key, route, epoch);

        for _ in 0..100 {
            assert_eq!(imports.subscribe(key), ImportEffect::None);
            assert_eq!(imports.unsubscribe(&key), ImportEffect::None);
        }
        assert_eq!(
            imports.state(&key),
            Some(ImportState::Active { route, epoch }),
            "the cluster route is untouched by local churn"
        );
    }

    #[test]
    fn subscribing_during_retirement_reinstalls() {
        let mut imports = ImportTable::new();
        let key = "trk";
        imports.subscribe(key);
        let (route, epoch) = (RouteId::from_raw(1), 0);
        imports.on_installed(&key, route, epoch);
        assert_eq!(
            imports.unsubscribe(&key),
            ImportEffect::Retire { route, epoch }
        );

        assert_eq!(imports.subscribe(key), ImportEffect::None);
        assert_eq!(
            imports.on_retired(&key),
            ImportEffect::Install,
            "the late subscriber gets a fresh route, not the retired one"
        );
        assert_eq!(imports.state(&key), Some(ImportState::Installing));
    }

    #[test]
    fn unsubscribe_and_retire_are_idempotent() {
        let mut imports = ImportTable::new();
        let key = "trk";
        assert_eq!(imports.unsubscribe(&key), ImportEffect::None);
        assert_eq!(imports.on_retired(&key), ImportEffect::None);

        imports.subscribe(key);
        imports.on_installed(&key, RouteId::from_raw(0), 0);
        imports.unsubscribe(&key);
        imports.on_retired(&key);
        assert_eq!(imports.unsubscribe(&key), ImportEffect::None);
        assert_eq!(imports.on_retired(&key), ImportEffect::None);
    }

    #[tokio::test(start_paused = true)]
    async fn link_seq_detects_loss_duplication_and_reorder() {
        let mut table = RouteTable::new(ShardId::new(0));
        let now = Instant::now();
        let (id, epoch) = table
            .install(action(), names(), NtpTime::ZERO, now)
            .unwrap();

        for seq in [10u32, 11, 14, 14, 13] {
            let mut env = envelope(id, epoch);
            env.link_seq = seq;
            let entry = table.resolve(&env).unwrap();
            entry.observe(seq);
        }

        let stats = table.get(id).unwrap().stats;
        assert_eq!(stats.received, 5);
        assert_eq!(stats.duplicated, 1, "14 seen twice");
        assert_eq!(stats.reordered, 1, "13 arrived after 14");
        assert_eq!(stats.lost, 1, "12 never arrived; 13 was late, not lost");
    }

    // ── Phase 1: packed route primitives ─────────────────────────────────

    #[test]
    fn a_packed_route_round_trips_every_corner_of_its_two_fields() {
        let corners = [
            (0usize, 0u32),
            (0, PackedRoute::MAX_SLOT),
            (PackedRoute::MAX_SHARD as usize, 0),
            (PackedRoute::MAX_SHARD as usize, PackedRoute::MAX_SLOT),
            (0xa5c, 0x5_a5a5),
            (1, 1),
        ];
        for (shard, slot) in corners {
            let packed = PackedRoute::new(ShardId::new(shard), slot);
            assert_eq!(packed.shard(), ShardId::new(shard), "shard {shard} slot {slot}");
            assert_eq!(packed.slot(), slot, "shard {shard} slot {slot}");
            assert_eq!(
                PackedRoute::from_raw(packed.get()),
                packed,
                "raw round trip for shard {shard} slot {slot}"
            );
        }
    }

    /// The two fields must partition the u32 exactly: no bit belongs to both
    /// and none to neither, or a route would decode as a different one.
    #[test]
    fn the_two_fields_partition_the_whole_word() {
        let all_slot = PackedRoute::new(ShardId::new(0), PackedRoute::MAX_SLOT);
        let all_shard = PackedRoute::new(ShardId::new(PackedRoute::MAX_SHARD as usize), 0);
        assert_eq!(all_slot.get() & all_shard.get(), 0, "fields must not overlap");
        assert_eq!(all_slot.get() | all_shard.get(), u32::MAX, "fields must cover the word");
    }

    #[test]
    fn try_new_rejects_out_of_range_rather_than_truncating() {
        assert!(PackedRoute::try_new(ShardId::new(0), PackedRoute::MAX_SLOT).is_some());
        assert!(
            PackedRoute::try_new(ShardId::new(0), PackedRoute::MAX_SLOT.saturating_add(1))
                .is_none(),
            "a slot one past the field must fail, not wrap into the shard bits"
        );
        let max_shard = PackedRoute::MAX_SHARD as usize;
        assert!(PackedRoute::try_new(ShardId::new(max_shard), 0).is_some());
        assert!(
            PackedRoute::try_new(ShardId::new(max_shard.saturating_add(1)), 0).is_none(),
            "a shard one past the field must fail, not truncate"
        );
    }

    /// The families share a representation and must not share a meaning: the
    /// same bits read as a transport route and as a route id decode
    /// identically, which is exactly why nothing may convert between them
    /// implicitly.
    #[test]
    fn the_two_route_families_share_bits_but_not_identity() {
        let bits = PackedRoute::new(ShardId::new(9), 1234);
        let transport = TransportRoute::from_packed(bits);
        let endpoint = RouteId::from_packed(bits);
        assert_eq!(transport.shard(), endpoint.shard());
        assert_eq!(transport.slot(), endpoint.slot());
        assert_eq!(transport.get(), endpoint.get());
        assert_ne!(
            transport.to_string(),
            endpoint.to_string(),
            "they must at least be distinguishable in a log line"
        );
    }

    // ── Phase 1: the transport table ─────────────────────────────────────

    fn participant(n: u32) -> ParticipantKey {
        use slotmap::KeyData;
        ParticipantKey::from(KeyData::from_ffi(u64::from(n) | (1 << 32)))
    }

    #[tokio::test(start_paused = true)]
    async fn a_transport_route_resolves_to_its_participant_until_retired() {
        let mut table = TransportTable::new(ShardId::new(5));
        let key = participant(1);
        let handle = table.install(key, Instant::now()).unwrap();

        assert_eq!(handle.shard(), ShardId::new(5), "the route carries its owner");
        assert_eq!(table.resolve(handle), Some(key));

        assert!(table.retire(handle, Instant::now()));
        assert_eq!(table.resolve(handle), None, "a retired route resolves to nothing");
        assert_eq!(table.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn retiring_a_transport_route_twice_is_harmless() {
        let mut table = TransportTable::new(ShardId::new(0));
        let handle = table.install(participant(1), Instant::now()).unwrap();
        assert!(table.retire(handle, Instant::now()));
        assert!(
            !table.retire(handle, Instant::now()),
            "a redelivered teardown must not report success"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_epoch_cannot_reach_the_participant_that_replaced_it() {
        let mut table = TransportTable::new(ShardId::new(0));
        let first = table.install(participant(1), Instant::now()).unwrap();
        assert!(table.retire(first, Instant::now()));

        tokio::time::advance(ROUTE_QUARANTINE).await;
        let second = table.install(participant(2), Instant::now()).unwrap();

        assert_eq!(second.route, first.route, "the slot should come back");
        assert_ne!(second.epoch, first.epoch, "but as a new incarnation");
        assert_eq!(
            table.resolve(first),
            None,
            "the old handle must not reach the new tenant"
        );
        assert_eq!(table.resolve(second), Some(participant(2)));
    }

    /// A retired slot is quarantined by its *slot* number. Storing the packed
    /// route instead worked only on shard 0, where the shard bits are zero —
    /// on any other shard the queue held a number far outside the table and
    /// the slot never came back.
    #[tokio::test(start_paused = true)]
    async fn a_slot_returns_from_quarantine_on_a_shard_other_than_zero() {
        for table_shard in [0usize, 1, 41] {
            let mut table = RouteTable::new(ShardId::new(table_shard));
            let now = Instant::now();
            let (id, epoch) = table
                .install(action(), names(), NtpTime::ZERO, now)
                .unwrap();
            assert!(table.retire(id, epoch, now));

            tokio::time::advance(ROUTE_QUARANTINE).await;
            let (reused, reused_epoch) = table
                .install(action(), names(), NtpTime::ZERO, Instant::now())
                .unwrap();

            assert_eq!(reused, id, "shard {table_shard}: the slot must come back");
            assert_ne!(
                reused_epoch, epoch,
                "shard {table_shard}: reuse must bump the epoch"
            );
            assert_eq!(
                reused.shard(),
                ShardId::new(table_shard),
                "shard {table_shard}: a reused route still carries its owner"
            );
        }
    }

    /// The two families index different tables, so the same slot number is
    /// two unrelated things. Nothing may leak between them.
    #[tokio::test(start_paused = true)]
    async fn the_two_namespaces_allocate_independently() {
        let shard = ShardId::new(2);
        let mut routes = RouteTable::new(shard);
        let mut transports = TransportTable::new(shard);
        let now = Instant::now();

        let (endpoint, _) = routes
            .install(action(), names(), NtpTime::ZERO, now)
            .unwrap();
        let transport = transports.install(participant(1), now).unwrap();

        assert_eq!(
            endpoint.slot(),
            transport.route.slot(),
            "both namespaces start at slot zero, which is the point"
        );
        assert_eq!(transports.resolve(transport), Some(participant(1)));
        assert!(routes.get(endpoint).is_some());
    }
}
