//! Compiled routes and the wire envelope that addresses them.
//!
//! A route id's slot is allocated by the *destination*, because it indexes
//! that destination's table — but the id also carries which shard that
//! table belongs to (see [`RouteId`]), so it is a node-scoped address, not
//! purely a destination-local one. Semantic ids (participant, track, room,
//! topic) never appear on the wire.
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
use crate::id::ShardId;

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

    /// Packs a shard and its local slot. Invalid values are a programming or
    /// configuration error: truncating either one would address a different
    /// worker or slot.
    pub const fn new(shard: ShardId, slot: u32) -> Self {
        assert!(slot <= Self::MAX_SLOT, "route slot overflows 20 bits");
        assert!(
            shard.index() <= Self::MAX_SHARD as usize,
            "shard id overflows 12 bits"
        );
        #[allow(clippy::cast_possible_truncation)]
        let shard_bits = shard.index() as u32;
        Self((shard_bits << ROUTE_SLOT_BITS) | slot)
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

/// The one typed view of the fixed wire header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Envelope {
    pub ty: EnvelopeType,
    pub handle: RouteHandle,
    pub extension: u64,
}

/// The extension interpretation selected by the envelope type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameBody {
    Media { link_seq: u32, playout_ntp32: u32 },
    Feedback,
    Telemetry,
}

impl Envelope {
    pub fn media(handle: RouteHandle, link_seq: u32, playout_ntp32: u32) -> Self {
        Self {
            ty: EnvelopeType::Media,
            handle,
            extension: (u64::from(link_seq) << 32) | u64::from(playout_ntp32),
        }
    }

    pub fn feedback(handle: RouteHandle) -> Self {
        Self {
            ty: EnvelopeType::Feedback,
            handle,
            extension: 0,
        }
    }

    pub fn telemetry(handle: RouteHandle) -> Self {
        Self {
            ty: EnvelopeType::Telemetry,
            handle,
            extension: 0,
        }
    }

    pub fn encode(self) -> [u8; ENVELOPE_LEN] {
        pulsebeam_routing::envelope::Envelope {
            ty: self.ty,
            epoch: self.handle.epoch,
            route: to_wire_route(self.handle.route),
            extension: self.extension,
        }
        .encode()
    }

    pub fn decode(buf: &[u8]) -> Result<Self, EnvelopeError> {
        let wire = pulsebeam_routing::envelope::Envelope::decode(buf)?;
        Ok(Self {
            ty: wire.ty,
            handle: RouteHandle::new(from_wire_route(wire.route), wire.epoch),
            extension: wire.extension,
        })
    }
}

/// Decode the shared header and its type-specific extension in one operation.
pub fn decode_frame(buf: &[u8]) -> Result<(Envelope, FrameBody), EnvelopeError> {
    let env = Envelope::decode(buf)?;
    let body = match env.ty {
        EnvelopeType::Media => FrameBody::Media {
            #[allow(clippy::cast_possible_truncation)]
            link_seq: (env.extension >> 32) as u32,
            #[allow(clippy::cast_possible_truncation)]
            playout_ntp32: env.extension as u32,
        },
        EnvelopeType::Feedback => FrameBody::Feedback,
        EnvelopeType::Telemetry => FrameBody::Telemetry,
    };
    Ok((env, body))
}

/// Read the type tag of an encoded frame, which is what says how to interpret
/// its `extension` and its payload.
pub fn peek_type(buf: &[u8]) -> Result<EnvelopeType, EnvelopeError> {
    Envelope::decode(buf).map(|env| env.ty)
}

fn to_wire_route(route: RouteId) -> pulsebeam_routing::RouteId {
    pulsebeam_routing::RouteId::from_raw(route.get())
}

fn from_wire_route(route: pulsebeam_routing::RouteId) -> RouteId {
    RouteId::from_raw(route.get())
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteAction {
    Forward {
        target: crate::keys::TrackKey,
    },
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
    /// No `origin` field: the track key resolves to
    /// an arena entry that already knows its own publisher.
    Reverse {
        target: crate::keys::TrackKey,
    },
}

/// The shard-owned route slot. Its epoch rejects stale control messages while
/// the action and packet accounting stay in the same cache line.
#[derive(Debug)]
pub(crate) struct RouteRuntimeEntry {
    pub epoch: u16,
    pub action: RouteAction,
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

impl RouteRuntimeEntry {
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
    /// Whether each slot is currently in `quarantine`, so a double retire is
    /// an O(1) refusal rather than a scan of the queue.
    quarantined: Vec<bool>,
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
            quarantined: Vec::with_capacity(prealloc),
            max_slots,
        }
    }

    /// `Ok((slot, epoch))`. A `slot` equal to the pre-call [`Self::high_water`]
    /// is fresh and the caller must push one entry; anything lower is a
    /// quarantined slot coming back, already bumped to a new epoch.
    pub(crate) fn allocate(&mut self, now: Instant) -> Result<(u32, u16), RouteError> {
        self.allocate_inner(now, true)
    }

    pub(crate) fn allocate_transport(&mut self, now: Instant) -> Result<(u32, u16), RouteError> {
        self.allocate_inner(now, false)
    }

    fn allocate_inner(
        &mut self,
        now: Instant,
        inject_failure: bool,
    ) -> Result<(u32, u16), RouteError> {
        // Exhaustion is the one failure every caller has written a rollback for and none has ever
        // taken: a namespace only fills under a participant count no plan reaches. Injecting it is
        // what puts those recovery paths under test at all. It lives here rather than at the table
        // it used to serve because this is now the only place an address can fail to exist.
        if inject_failure && pulsebeam_runtime::buggify!("route table exhausted") {
            return Err(RouteError::Exhausted {
                max_slots: self.max_slots,
            });
        }
        // FIFO from the oldest retirement, so a slot is only reused once no
        // datagram addressed to its previous incarnation could still arrive.
        if let Some(&(slot, retired_at)) = self.quarantine.front()
            && now.saturating_duration_since(retired_at) >= ROUTE_QUARANTINE
        {
            self.quarantine.pop_front();
            if let Some(flag) = self.quarantined.get_mut(slot as usize) {
                *flag = false;
            }
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
        self.quarantined.push(false);
        debug_assert_eq!(self.epochs.len(), self.quarantined.len());
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
        // A slot quarantined twice is handed out twice, at two different
        // epochs. Both incarnations install into the same `RouteImage` index,
        // so the second silently overwrites the first and the first
        // participant's route stops resolving with no error anywhere — the one
        // place in this model where a control-plane bug becomes silent media
        // loss instead of a visible failure. Refuse the duplicate.
        match self.quarantined.get_mut(slot as usize) {
            Some(flag) if !*flag => *flag = true,
            Some(_) => {
                debug_assert!(false, "slot {slot} was retired while already quarantined");
                metrics::counter!("route_double_retire").increment(1);
                return;
            }
            None => {
                debug_assert!(false, "retired a slot outside the namespace");
                return;
            }
        }
        self.quarantine.push_back((slot, now));
    }
}

/// The shard's route table, addressed by slot.
#[derive(Debug)]
pub(crate) struct RouteRuntime {
    shard_id: ShardId,
    /// Inline, not boxed. One packet touches exactly one entry, so a boxed
    /// slot would cost a pointer fetch and then a second line for the target;
    /// at 56 bytes the entry fits a cache line on its own. The counters stay
    /// beside the timeline state for the same reason — `observe` writes them
    /// on every packet, so moving them out would touch two lines rather than
    /// making one colder.
    slots: Vec<Option<RouteRuntimeEntry>>,
}

/// Working set preallocated up front so steady-state operation never
/// allocates.
const ROUTE_TABLE_PREALLOCATED_SLOTS: usize = 1 << 14;

/// The only way allocation fails now that resolution belongs to the view: a
/// stale or out-of-range address is the view's `None`, not an error type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteError {
    /// No slot is out of quarantine and the allocator is at its cap.
    Exhausted { max_slots: u32 },
}

impl RouteRuntime {
    pub fn new(shard_id: ShardId) -> Self {
        Self {
            shard_id,
            slots: Vec::with_capacity(ROUTE_TABLE_PREALLOCATED_SLOTS),
        }
    }

    /// The accounting behind a route, created on first use.
    ///
    /// Nothing tells the shard a route exists: the published view is the
    /// announcement, and this materialises when a packet actually arrives on
    /// it. A stale entry for a slot the view has since reused is harmless —
    /// it is epoch-checked, and the next install overwrites it — so there is
    /// no retirement message either.
    pub fn accounting_mut(
        &mut self,
        handle: RouteHandle,
        ntp_ref: NtpTime,
    ) -> &mut RouteRuntimeEntry {
        let idx = handle.route.index();
        if idx >= self.slots.len() {
            self.slots.resize_with(idx.saturating_add(1), || None);
        }
        let stale = self
            .slots
            .get(idx)
            .and_then(Option::as_ref)
            .is_none_or(|entry| entry.epoch != handle.epoch);
        if stale {
            self.install(handle, ntp_ref);
        }
        let Some(Some(entry)) = self.slots.get_mut(idx) else {
            pulsebeam_runtime::fatal!("route accounting must exist after installing it")
        };
        if entry.last_link_seq.is_none() {
            entry.expander = NtpExpander::new(ntp_ref);
        }
        entry
    }

    /// Build the accounting behind an address the control plane granted.
    pub fn install(&mut self, handle: RouteHandle, ntp_ref: NtpTime) {
        debug_assert_eq!(
            handle.shard(),
            self.shard_id,
            "a route's accounting only exists at the shard that owns it"
        );
        let idx = handle.route.index();
        if idx >= self.slots.len() {
            self.slots.resize_with(idx.saturating_add(1), || None);
        }
        let Some(slot) = self.slots.get_mut(idx) else {
            debug_assert!(false, "the resize above guarantees this slot exists");
            return;
        };
        *slot = Some(RouteRuntimeEntry {
            epoch: handle.epoch,
            action: RouteAction::Forward {
                target: crate::keys::TrackKey::default(),
            },
            expander: NtpExpander::new(ntp_ref),
            last_link_seq: None,
            stats: RouteStats::default(),
        });
    }

    pub fn install_action(&mut self, handle: RouteHandle, action: RouteAction) {
        debug_assert_eq!(
            handle.shard(),
            self.shard_id,
            "a route's action only exists at the shard that owns it"
        );
        let idx = handle.route.index();
        if idx >= self.slots.len() {
            self.slots.resize_with(idx.saturating_add(1), || None);
        }
        let Some(slot) = self.slots.get_mut(idx) else {
            debug_assert!(false, "the resize above guarantees this slot exists");
            return;
        };
        if let Some(entry) = slot.as_mut()
            && entry.epoch == handle.epoch
        {
            entry.action = action;
            return;
        }
        *slot = Some(RouteRuntimeEntry {
            epoch: handle.epoch,
            action,
            expander: NtpExpander::new(NtpTime::ZERO),
            last_link_seq: None,
            stats: RouteStats::default(),
        });
    }

    /// Idempotent, and epoch-checked: a redelivered teardown must not drop the
    /// accounting of the incarnation that replaced the one it names.
    pub fn retire(&mut self, handle: RouteHandle) -> bool {
        let Some(slot) = self.slots.get_mut(handle.route.index()) else {
            return false;
        };
        match slot {
            Some(entry) if entry.epoch == handle.epoch => {}
            _ => return false,
        }
        *slot = None;
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_routing_counter("route_retired");
        true
    }

    #[cfg(test)]
    pub fn entry_mut(&mut self, handle: RouteHandle) -> Option<&mut RouteRuntimeEntry> {
        match self.slots.get_mut(handle.route.index()) {
            Some(Some(entry)) if entry.epoch == handle.epoch => Some(entry),
            _ => None,
        }
    }

    pub fn entry(&self, handle: RouteHandle) -> Option<&RouteRuntimeEntry> {
        match self.slots.get(handle.route.index()) {
            Some(Some(entry)) if entry.epoch == handle.epoch => Some(entry),
            _ => None,
        }
    }

    pub fn resolve(&self, handle: RouteHandle) -> Option<RouteAction> {
        self.entry(handle).map(|entry| entry.action)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}

#[cfg(test)]
mod tests {

    /// A slot quarantined twice would be handed out twice, at two different
    /// epochs. Both install into the same `RouteImage` index, so the second
    /// silently overwrites the first and the first participant's route stops
    /// resolving with no error anywhere — a control-plane bug becoming silent
    /// media loss. The allocator refuses the duplicate; in a build with
    /// assertions on it aborts instead, because a caller doing this is broken.
    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "already quarantined")]
    async fn a_double_retire_is_refused() {
        let mut alloc = SlotAllocator::with_max_slots(ShardId::new(0), 8);
        let now = Instant::now();
        let (slot, _) = alloc.allocate(now).unwrap();
        alloc.retire(slot, now);
        alloc.retire(slot, now);
    }

    /// The ordinary path the guard must not disturb: retire, wait out the
    /// quarantine, and the slot comes back exactly once with a fresh epoch.
    #[tokio::test(start_paused = true)]
    async fn a_retired_slot_returns_once_with_a_new_epoch() {
        let mut alloc = SlotAllocator::with_max_slots(ShardId::new(0), 8);
        let (slot, first_epoch) = alloc.allocate(Instant::now()).unwrap();
        alloc.retire(slot, Instant::now());

        tokio::time::advance(ROUTE_QUARANTINE).await;
        let (reused, second_epoch) = alloc.allocate(Instant::now()).unwrap();
        assert_eq!(reused, slot);
        assert_ne!(first_epoch, second_epoch);

        // And it is not still queued: the next allocation is a fresh slot.
        let (next, _) = alloc.allocate(Instant::now()).unwrap();
        assert_ne!(next, slot, "a reused slot must leave the quarantine queue");

        // Retiring it again is legal once it is live again.
        alloc.retire(slot, Instant::now());
    }
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;

    fn envelope(route: RouteId, epoch: u16) -> Envelope {
        Envelope::media(RouteHandle::new(route, epoch), 0, 0)
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
        let env = Envelope {
            ty: EnvelopeType::Media,
            handle: RouteHandle::new(RouteId::from_raw(0x3344_5566), 0x1122),
            extension: (u64::from(0x7788_99AAu32) << 32) | u64::from(0xBBCC_DDEE_u32),
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
        let encoded = Envelope::feedback(RouteHandle::new(RouteId::from_raw(1), 1)).encode();
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
            Envelope::feedback(RouteHandle::new(RouteId::from_raw(u32::MAX), u16::MAX)),
            Envelope::telemetry(RouteHandle::new(RouteId::from_raw(0), 0)),
        ] {
            assert_eq!(Envelope::decode(&env.encode()).unwrap(), env);
        }
    }

    /// Cross-node every payload family shares one socket and one header, so a
    /// receiver tells them apart by the `type` byte rather than by guessing a
    /// header length. That is what lets the steering program read `route`
    /// without knowing what the payload is.
    #[test]
    fn the_payload_families_are_distinguishable_on_the_wire() {
        let media = envelope(RouteId::from_raw(3), 4).encode();
        let feedback = Envelope::feedback(RouteHandle::new(RouteId::from_raw(3), 4)).encode();
        let telemetry = Envelope::telemetry(RouteHandle::new(RouteId::from_raw(3), 4)).encode();

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

        assert_eq!(
            decode_frame(&media).unwrap().1,
            FrameBody::Media {
                link_seq: 0,
                playout_ntp32: 0
            }
        );
        assert_eq!(decode_frame(&feedback).unwrap().1, FrameBody::Feedback);
        assert_eq!(decode_frame(&telemetry).unwrap().1, FrameBody::Telemetry);
    }

    #[test]
    fn envelope_round_trips() {
        let env = Envelope {
            ty: EnvelopeType::Media,
            handle: RouteHandle::new(RouteId::from_raw(u32::MAX), 65_535),
            extension: u64::MAX,
        };
        assert_eq!(Envelope::decode(&env.encode()).unwrap(), env);
    }

    #[test]
    fn decode_rejects_truncated_input() {
        let full = envelope(RouteId::from_raw(1), 1).encode();
        for len in 0..ENVELOPE_LEN {
            assert_eq!(
                Envelope::decode(&full[..len]),
                Err(EnvelopeError::Truncated { len })
            );
        }
    }

    #[test]
    fn decode_rejects_unknown_version_and_unknown_type() {
        let mut bytes = envelope(RouteId::from_raw(1), 1).encode();
        bytes[0] = ENVELOPE_VERSION + 1;
        assert_eq!(
            Envelope::decode(&bytes),
            Err(EnvelopeError::UnsupportedVersion {
                ver: ENVELOPE_VERSION + 1
            })
        );

        let mut bytes = envelope(RouteId::from_raw(1), 1).encode();
        bytes[1] = 0xff;
        assert_eq!(
            Envelope::decode(&bytes),
            Err(EnvelopeError::UnknownType { ty: 0xff }),
            "an unrecognised payload family is rejected, not skipped past"
        );
    }

    // ── allocation, quarantine, and the accounting behind a route ────────

    fn alloc(shard: usize) -> SlotAllocator {
        SlotAllocator::with_max_slots(ShardId::new(shard), PackedRoute::MAX_SLOT.saturating_add(1))
    }

    fn handle(shard: usize, slot: u32, epoch: u16) -> RouteHandle {
        RouteHandle::new(RouteId::new(ShardId::new(shard), slot), epoch)
    }

    /// An address handed out for a shard must carry that shard, so a
    /// misdelivered packet is recognised by reading its high bits alone.
    #[tokio::test(start_paused = true)]
    async fn an_allocated_route_carries_the_shard_it_was_allocated_for() {
        let mut allocator = alloc(41);
        let (slot, _) = allocator.allocate(Instant::now()).unwrap();
        assert_eq!(
            RouteId::new(ShardId::new(41), slot).shard(),
            ShardId::new(41)
        );
    }

    /// Growth is bounded, so a churn storm fails an allocation rather than
    /// consuming the node's memory until the allocator decides for us.
    #[tokio::test(start_paused = true)]
    async fn an_allocator_at_its_cap_refuses_instead_of_growing() {
        let mut allocator = SlotAllocator::with_max_slots(ShardId::new(0), 2);
        let now = Instant::now();

        for _ in 0..2 {
            allocator.allocate(now).expect("within the cap");
        }
        assert_eq!(
            allocator.allocate(now),
            Err(RouteError::Exhausted { max_slots: 2 }),
            "past the cap it must fail rather than grow"
        );
    }

    /// A slot only comes back once no datagram addressed to its previous
    /// incarnation could still be in flight, and it comes back as a new
    /// epoch so the old handle cannot reach the new tenant.
    #[tokio::test(start_paused = true)]
    async fn a_recycled_slot_comes_back_under_a_new_epoch() {
        let mut allocator = alloc(0);
        let now = Instant::now();
        let (slot, epoch) = allocator.allocate(now).unwrap();
        allocator.retire(slot, now);

        tokio::time::advance(ROUTE_QUARANTINE).await;
        let (reused, reused_epoch) = allocator.allocate(Instant::now()).unwrap();

        assert_eq!(reused, slot, "the slot should be reused after quarantine");
        assert_ne!(reused_epoch, epoch, "but as a new incarnation");
    }

    #[tokio::test(start_paused = true)]
    async fn a_slot_is_not_reused_inside_its_quarantine() {
        let mut allocator = alloc(0);
        let now = Instant::now();
        let (slot, _) = allocator.allocate(now).unwrap();
        allocator.retire(slot, now);

        let (next, _) = allocator.allocate(now).unwrap();
        assert_ne!(next, slot, "must not reuse a slot still in quarantine");
    }

    /// The accounting behind a route is epoch-checked the same way its
    /// resolution is: a frame for a superseded incarnation must not fold into
    /// the counters of the one that replaced it.
    #[tokio::test(start_paused = true)]
    async fn accounting_is_scoped_to_one_incarnation() {
        let mut runtime = RouteRuntime::new(ShardId::new(0));
        runtime.install(handle(0, 0, 1), NtpTime::ZERO);

        assert!(runtime.entry_mut(handle(0, 0, 1)).is_some());
        assert!(
            runtime.entry_mut(handle(0, 0, 2)).is_none(),
            "a different epoch is a different route"
        );
        assert!(
            runtime.entry_mut(handle(0, 9, 1)).is_none(),
            "a slot that was never installed has no accounting"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn same_epoch_action_updates_preserve_packet_accounting() {
        let mut runtime = RouteRuntime::new(ShardId::new(0));
        let route = handle(0, 0, 7);
        runtime.install_action(
            route,
            RouteAction::Forward {
                target: crate::keys::TrackKey::default(),
            },
        );
        {
            let entry = runtime.entry_mut(route).unwrap();
            entry.observe(100);
            entry.observe(103);
        }
        let before = runtime.entry(route).unwrap().stats;
        let expander_before = runtime.entry(route).unwrap().expander;
        runtime.install_action(
            route,
            RouteAction::Reverse {
                target: crate::keys::TrackKey::default(),
            },
        );

        let entry = runtime.entry(route).unwrap();
        assert_eq!(
            entry.action,
            RouteAction::Reverse {
                target: crate::keys::TrackKey::default()
            }
        );
        assert_eq!(entry.stats, before);
        assert_eq!(entry.last_link_seq, Some(103));
        assert_eq!(entry.expander.reference(), expander_before.reference());
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_epoch_restarts_packet_accounting() {
        let mut runtime = RouteRuntime::new(ShardId::new(0));
        let old = handle(0, 0, 7);
        runtime.install_action(
            old,
            RouteAction::Forward {
                target: crate::keys::TrackKey::default(),
            },
        );
        runtime.entry_mut(old).unwrap().observe(100);

        let replacement = handle(0, 0, 8);
        runtime.install_action(
            replacement,
            RouteAction::Forward {
                target: crate::keys::TrackKey::default(),
            },
        );

        let entry = runtime.entry(replacement).unwrap();
        assert_eq!(entry.stats, RouteStats::default());
        assert_eq!(entry.last_link_seq, None);
        assert!(runtime.entry(old).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn retiring_accounting_is_idempotent_and_epoch_checked() {
        let mut runtime = RouteRuntime::new(ShardId::new(0));
        runtime.install(handle(0, 0, 4), NtpTime::ZERO);

        assert!(
            !runtime.retire(handle(0, 0, 3)),
            "a teardown for a superseded incarnation must not touch this one"
        );
        assert!(runtime.entry(handle(0, 0, 4)).is_some());

        assert!(runtime.retire(handle(0, 0, 4)));
        assert!(
            !runtime.retire(handle(0, 0, 4)),
            "a redelivered teardown is a no-op"
        );
        assert_eq!(runtime.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn link_seq_detects_loss_duplication_and_reorder() {
        let mut runtime = RouteRuntime::new(ShardId::new(0));
        let route = handle(0, 0, 0);
        runtime.install(route, NtpTime::ZERO);

        for seq in [10u32, 11, 14, 14, 13] {
            let entry = runtime.entry_mut(route).unwrap();
            entry.observe(seq);
        }

        let stats = runtime.entry(route).unwrap().stats;
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
            assert_eq!(
                packed.shard(),
                ShardId::new(shard),
                "shard {shard} slot {slot}"
            );
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
        assert_eq!(
            all_slot.get() & all_shard.get(),
            0,
            "fields must not overlap"
        );
        assert_eq!(
            all_slot.get() | all_shard.get(),
            u32::MAX,
            "fields must cover the word"
        );
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

    /// A retired slot is quarantined by its *slot* number. Storing the packed
    /// route instead worked only on shard 0, where the shard bits are zero —
    /// on any other shard the queue held a number far outside the namespace
    /// and the slot never came back.
    #[tokio::test(start_paused = true)]
    async fn a_slot_returns_from_quarantine_on_a_shard_other_than_zero() {
        for shard in [0usize, 1, 41] {
            let mut allocator = alloc(shard);
            let now = Instant::now();
            let (slot, epoch) = allocator.allocate(now).unwrap();
            allocator.retire(slot, now);

            tokio::time::advance(ROUTE_QUARANTINE).await;
            let (reused, reused_epoch) = allocator.allocate(Instant::now()).unwrap();

            assert_eq!(reused, slot, "shard {shard}: the slot must come back");
            assert_ne!(
                reused_epoch, epoch,
                "shard {shard}: reuse must bump the epoch"
            );
            assert_eq!(
                RouteId::new(ShardId::new(shard), reused).shard(),
                ShardId::new(shard),
                "shard {shard}: a reused route still carries its owner"
            );
        }
    }
}

#[cfg(test)]
mod layout {
    use super::*;

    /// A packet touches exactly one runtime entry and one view binding. Both
    /// are indexed inline, so the only way either costs more than a single
    /// cache line is if it grows past one — which is what this pins.
    #[test]
    fn the_per_packet_entries_each_fit_a_cache_line() {
        const CACHE_LINE: usize = 64;
        assert!(
            size_of::<Option<RouteRuntimeEntry>>() <= CACHE_LINE,
            "route accounting is {} bytes; a packet would straddle two lines",
            size_of::<Option<RouteRuntimeEntry>>()
        );
    }
}
