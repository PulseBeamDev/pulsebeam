//! Compiled routes and the wire envelope that addresses them.
//!
//! A route id is allocated by the *destination*, because it indexes that
//! destination's table. Semantic ids (participant, track, room, topic) never
//! appear on the wire; they survive only in [`RouteNames`] for logs.
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
use crate::shard::router::{DataStreamKey, LocalTrackKey, ReliableStreamKey, RoomKey};
use crate::track::Topic;

pub const MEDIA_ENVELOPE_LEN: usize = 16;
pub const ROUTE_ENVELOPE_LEN: usize = 8;
pub const ENVELOPE_VERSION: u8 = 1;

/// Which direction a frame is travelling, carried in `flags` bit 0.
///
/// Both lanes share one socket cross-node, so the receiver has to demux them
/// before it can know how long the header is. `ver` and `flags` sit at the same
/// two offsets in both envelopes precisely so this bit can be read first.
///
/// This is the first defined flag bit. It is an addition to a field that was
/// wholly reserved, not a reinterpretation of one that meant something else —
/// the distinction the version rules turn on.
const FLAG_LANE: u8 = 0b0000_0001;
const FLAG_LANE_MEDIA: u8 = 0;
const FLAG_LANE_REVERSE: u8 = FLAG_LANE;

/// Bits with no meaning yet. They are the reserved surface for further
/// compatible v1 extensions, and must be zero until one is defined — a set bit
/// we do not understand is a bug or a version mismatch, not something to skip
/// past.
const FLAGS_RESERVED: u8 = !FLAG_LANE;

/// How long a retired slot waits before it can be handed out again.
///
/// `epoch` is the primary guard against a delayed datagram landing on a
/// recycled slot; this is the second line of defence, and what makes the
/// "a slot cannot complete 65,536 generations within one stale-datagram
/// lifetime" invariant trivially true.
pub const ROUTE_QUARANTINE: Duration = Duration::from_secs(60);

/// `shard(12) | slot(20)`, so a packet landing on the wrong shard — which
/// happens on every node, because `SO_REUSEPORT` picks the shard by 5-tuple
/// hash and knows nothing about routes — is forwarded by reading 12 bits
/// instead of hashing a name back to a shard. 4096 shards, 1M slots per
/// shard: core counts grow predictably, per-shard route counts don't.
///
/// This makes `RouteId` a node-scoped address with a network part (the
/// shard) and a host part (the slot), not purely "allocated by the
/// destination, because it indexes that destination's table" the way the
/// module doc otherwise describes it — the destination still allocates the
/// slot, but the shard bits are fixed by which table it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RouteId(u32);

const ROUTE_SLOT_BITS: u32 = 20;
const ROUTE_SHARD_BITS: u32 = 12;
const ROUTE_SLOT_MASK: u32 = (1 << ROUTE_SLOT_BITS) - 1;
const ROUTE_SHARD_MASK: u32 = (1 << ROUTE_SHARD_BITS) - 1;

impl RouteId {
    /// Packs a destination shard and its local slot into one wire id.
    /// `slot` must fit in 20 bits and `shard` in 12 — both are asserted, not
    /// silently truncated, because a truncated shard id would forward a
    /// packet to the wrong worker without ever failing loudly.
    pub const fn new(shard: ShardId, slot: u32) -> Self {
        debug_assert!(slot <= ROUTE_SLOT_MASK, "route slot overflows 20 bits");
        // Asserted below to fit in 12 bits, so the truncation clippy warns
        // about cannot happen for any shard this format actually supports.
        #[allow(clippy::cast_possible_truncation)]
        let shard_bits = shard.index() as u32;
        debug_assert!(shard_bits <= ROUTE_SHARD_MASK, "shard id overflows 12 bits");
        Self(((shard_bits & ROUTE_SHARD_MASK) << ROUTE_SLOT_BITS) | (slot & ROUTE_SLOT_MASK))
    }

    /// Reconstructs a `RouteId` from its wire representation. Unlike `new`,
    /// this trusts the bits as given — the network handed them to us already
    /// packed, and re-deriving shard/slot from them is exactly `shard()`/
    /// `slot()`.
    pub const fn from_raw(bits: u32) -> Self {
        Self(bits)
    }

    /// The destination shard this route was allocated on.
    pub const fn shard(self) -> ShardId {
        ShardId::new((self.0 >> ROUTE_SLOT_BITS) as usize)
    }

    /// The route's index within its shard's own table — what a `RouteTable`
    /// indexes by.
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

impl std::fmt::Display for RouteId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rt{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeError {
    Truncated {
        len: usize,
    },
    UnsupportedVersion {
        ver: u8,
    },
    ReservedFlags {
        flags: u8,
    },
    /// Decoded as one lane but the header says the other.
    WrongLane {
        want: Lane,
    },
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated { len } => write!(f, "envelope truncated: {len} bytes"),
            Self::UnsupportedVersion { ver } => write!(f, "unsupported envelope version {ver}"),
            Self::ReservedFlags { flags } => write!(f, "reserved envelope flags set: {flags:#04x}"),
            Self::WrongLane { want } => write!(f, "envelope is not on the {want:?} lane"),
        }
    }
}

/// Which lane a frame on the wire belongs to, read before anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Media,
    Reverse,
}

/// Read the lane of an encoded frame without committing to a header length.
///
/// Cross-node both lanes arrive on one socket, so this is the first thing a
/// receiver does; the two envelopes deliberately agree on `ver` and `flags`
/// offsets so it can.
pub fn peek_lane(buf: &[u8]) -> Result<Lane, EnvelopeError> {
    let (ver, flags) = match buf {
        [ver, flags, ..] => (*ver, *flags),
        _ => return Err(EnvelopeError::Truncated { len: buf.len() }),
    };
    if ver != ENVELOPE_VERSION {
        return Err(EnvelopeError::UnsupportedVersion { ver });
    }
    if flags & FLAGS_RESERVED != 0 {
        return Err(EnvelopeError::ReservedFlags { flags });
    }
    Ok(if flags & FLAG_LANE == FLAG_LANE_REVERSE {
        Lane::Reverse
    } else {
        Lane::Media
    })
}

/// The 16-byte header on every media frame crossing a link.
///
/// Encoded big-endian at fixed offsets rather than by casting a Rust struct —
/// `repr(C)` layout is not a portable wire format.
///
/// ```text
/// 0       1       2               4                       8
/// +-------+-------+---------------+-----------------------+
/// | ver   | flags | epoch         | route (u32)           |
/// +-------+-------+---------------+-----------------------+
/// 8                              12                      16
/// +-------------------------------+-----------------------+
/// | link_seq (u32)                | playout_ntp32 (u32)   |
/// +-------------------------------+-----------------------+
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
    pub fn encode(&self) -> [u8; MEDIA_ENVELOPE_LEN] {
        let mut out = [0u8; MEDIA_ENVELOPE_LEN];
        out[0] = ENVELOPE_VERSION;
        out[1] = FLAG_LANE_MEDIA;
        out[2..4].copy_from_slice(&self.epoch.to_be_bytes());
        out[4..8].copy_from_slice(&self.route.get().to_be_bytes());
        out[8..12].copy_from_slice(&self.link_seq.to_be_bytes());
        out[12..16].copy_from_slice(&self.playout_ntp32.to_be_bytes());
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, EnvelopeError> {
        if buf.len() < MEDIA_ENVELOPE_LEN {
            return Err(EnvelopeError::Truncated { len: buf.len() });
        }
        if peek_lane(buf)? != Lane::Media {
            return Err(EnvelopeError::WrongLane { want: Lane::Media });
        }
        let [
            _,
            _,
            e0,
            e1,
            r0,
            r1,
            r2,
            r3,
            s0,
            s1,
            s2,
            s3,
            p0,
            p1,
            p2,
            p3,
            ..,
        ] = *buf
        else {
            return Err(EnvelopeError::Truncated { len: buf.len() });
        };
        Ok(Self {
            epoch: u16::from_be_bytes([e0, e1]),
            route: RouteId::from_raw(u32::from_be_bytes([r0, r1, r2, r3])),
            link_seq: u32::from_be_bytes([s0, s1, s2, s3]),
            playout_ntp32: u32::from_be_bytes([p0, p1, p2, p3]),
        })
    }
}

/// The 8-byte header on every frame that carries no timeline.
///
/// Used by both directions that need addressing without one: upstream requests
/// travelling back to a publisher, and forward telemetry travelling out to a
/// destination. Half the size of [`MediaEnvelope`] because neither needs the
/// two fields that make up the difference. `link_seq` exists to observe
/// loss on a link, but every reverse body is a request the sender repeats if it
/// still needs it, so a lost one costs a round trip and there is nothing to
/// account for. `playout_ntp32` places a packet on a timeline; a request has
/// none.
///
/// ```text
/// 0       1       2               4                       8
/// +-------+-------+---------------+-----------------------+
/// | ver   | flags | epoch         | route (u32)           |
/// +-------+-------+---------------+-----------------------+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteEnvelope {
    pub epoch: u16,
    pub route: RouteId,
}

impl RouteEnvelope {
    pub fn new(handle: ReverseRoute) -> Self {
        Self {
            epoch: handle.epoch,
            route: handle.route,
        }
    }

    pub fn encode(&self) -> [u8; ROUTE_ENVELOPE_LEN] {
        let mut out = [0u8; ROUTE_ENVELOPE_LEN];
        out[0] = ENVELOPE_VERSION;
        out[1] = FLAG_LANE_REVERSE;
        out[2..4].copy_from_slice(&self.epoch.to_be_bytes());
        out[4..8].copy_from_slice(&self.route.get().to_be_bytes());
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, EnvelopeError> {
        if buf.len() < ROUTE_ENVELOPE_LEN {
            return Err(EnvelopeError::Truncated { len: buf.len() });
        }
        if peek_lane(buf)? != Lane::Reverse {
            return Err(EnvelopeError::WrongLane {
                want: Lane::Reverse,
            });
        }
        let [_, _, e0, e1, r0, r1, r2, r3, ..] = *buf else {
            return Err(EnvelopeError::Truncated { len: buf.len() });
        };
        Ok(Self {
            epoch: u16::from_be_bytes([e0, e1]),
            route: RouteId::from_raw(u32::from_be_bytes([r0, r1, r2, r3])),
        })
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

/// A sender-side handle to a reverse route, handed out with the stream it
/// belongs to by the control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReverseRoute {
    pub route: RouteId,
    pub epoch: u16,
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
/// [`MAX_ROUTES_PER_SHARD`] enforces that reasoning rather than leaving it as
/// prose: the families above are all bounded, so passing the cap means one of
/// them is not behaving as described, and failing the install says so while
/// there is still a process to say it in.
#[derive(Debug)]
pub(crate) struct RouteTable {
    shard_id: ShardId,
    slots: Vec<Slot>,
    epochs: Vec<u16>,
    /// Retired slots, oldest first, with the instant they were retired.
    quarantine: VecDeque<(u32, Instant)>,
    max_slots: u32,
}

/// Ceiling on slots one shard's table may grow to.
///
/// Sized for headroom, not for fit: every route family is proportional to
/// participants on this shard, so a healthy shard uses orders of magnitude
/// fewer. It exists to convert unbounded growth — a reconnect storm churning
/// routes faster than [`ROUTE_QUARANTINE`] returns them — into a bounded
/// failure that names itself, instead of an allocator death spiral 4 billion
/// slots later.
pub const MAX_ROUTES_PER_SHARD: u32 = 1 << 20;

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
        Self::with_max_slots(shard_id, MAX_ROUTES_PER_SHARD)
    }

    pub fn with_max_slots(shard_id: ShardId, max_slots: u32) -> Self {
        Self {
            shard_id,
            slots: Vec::new(),
            epochs: Vec::new(),
            quarantine: VecDeque::new(),
            max_slots,
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
                max_slots: self.max_slots,
            });
        }
        let id = self.allocate(now)?;
        let epoch = self.epochs.get(id.index()).copied().unwrap_or(0);
        let Some(slot) = self.slots.get_mut(id.index()) else {
            debug_assert!(false, "allocate() returned a slot outside the table");
            return Err(RouteError::Exhausted {
                max_slots: u32::try_from(self.slots.len()).unwrap_or(u32::MAX),
            });
        };
        *slot = Slot::Live(Box::new(RouteEntry {
            epoch,
            action,
            names,
            expander: NtpExpander::new(ntp_ref),
            last_link_seq: None,
            stats: RouteStats::default(),
        }));
        Ok((id, epoch))
    }

    /// Idempotent: retiring an already-free slot is a no-op, so a redelivered
    /// teardown cannot desync the table.
    ///
    /// `epoch` must match the live incarnation. Every other operation on this
    /// table checks it; retire was the one exception, and it is the one
    /// operation here that destroys state — a teardown for a superseded
    /// incarnation must not retire the one that replaced it.
    pub fn retire(&mut self, id: RouteId, epoch: u16, now: Instant) -> bool {
        let Some(slot) = self.slots.get_mut(id.index()) else {
            return false;
        };
        match slot {
            Slot::Live(entry) if entry.epoch == epoch => {}
            _ => return false,
        }
        *slot = Slot::Free;
        self.quarantine.push_back((id.get(), now));
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

    fn allocate(&mut self, now: Instant) -> Result<RouteId, RouteError> {
        // FIFO from the oldest retirement, so a slot is only reused once no
        // datagram addressed to its previous incarnation could still arrive.
        if let Some(&(idx, retired_at)) = self.quarantine.front()
            && now.saturating_duration_since(retired_at) >= ROUTE_QUARANTINE
        {
            self.quarantine.pop_front();
            debug_assert!(
                matches!(self.slots.get(idx as usize), Some(Slot::Free)),
                "a quarantined slot must still be free"
            );
            if let Some(epoch) = self.epochs.get_mut(idx as usize) {
                if *epoch == u16::MAX {
                    tracing::warn!(route = idx, "route epoch wrapped");
                }
                *epoch = epoch.wrapping_add(1);
            }
            return Ok(RouteId::new(self.shard_id, idx));
        }

        let idx = u32::try_from(self.slots.len()).unwrap_or(u32::MAX);
        if idx >= self.max_slots {
            return Err(RouteError::Exhausted {
                max_slots: self.max_slots,
            });
        }
        self.slots.push(Slot::Free);
        self.epochs.push(0);
        Ok(RouteId::new(self.shard_id, idx))
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
        assert_eq!(MEDIA_ENVELOPE_LEN, 16);
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
                0x00,
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

    #[test]
    fn reverse_envelope_is_exactly_eight_bytes() {
        assert_eq!(ROUTE_ENVELOPE_LEN, 8);
        assert_eq!(
            RouteEnvelope {
                epoch: 1,
                route: RouteId::from_raw(1),
            }
            .encode()
            .len(),
            8,
            "the reverse lane pays for addressing and nothing else"
        );
    }

    #[test]
    fn reverse_envelope_round_trips() {
        let env = RouteEnvelope {
            epoch: u16::MAX,
            route: RouteId::from_raw(u32::MAX),
        };
        assert_eq!(RouteEnvelope::decode(&env.encode()).unwrap(), env);
    }

    /// Cross-node both lanes share a socket, so a receiver must be able to tell
    /// them apart before it knows how long the header is. The lane bit is at a
    /// fixed offset both envelopes agree on, so peeking never needs the length.
    #[test]
    fn the_two_lanes_are_distinguishable_on_the_wire() {
        let media = envelope(RouteId::from_raw(3), 4).encode();
        let reverse = RouteEnvelope {
            epoch: 4,
            route: RouteId::from_raw(3),
        }
        .encode();

        assert_eq!(peek_lane(&media).unwrap(), Lane::Media);
        assert_eq!(peek_lane(&reverse).unwrap(), Lane::Reverse);

        // Peeking works on the shorter of the two, so it never over-reads.
        assert_eq!(peek_lane(&reverse[..2]).unwrap(), Lane::Reverse);

        // And decoding one as the other is refused rather than misread.
        assert_eq!(
            MediaEnvelope::decode(&reverse),
            Err(EnvelopeError::Truncated { len: 8 })
        );
        assert_eq!(
            RouteEnvelope::decode(&media),
            Err(EnvelopeError::WrongLane {
                want: Lane::Reverse
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
        for len in 0..MEDIA_ENVELOPE_LEN {
            assert_eq!(
                MediaEnvelope::decode(&full[..len]),
                Err(EnvelopeError::Truncated { len })
            );
        }
    }

    #[test]
    fn decode_rejects_unknown_version_and_reserved_flags() {
        let mut bytes = envelope(RouteId::from_raw(1), 1).encode();
        bytes[0] = ENVELOPE_VERSION + 1;
        assert_eq!(
            MediaEnvelope::decode(&bytes),
            Err(EnvelopeError::UnsupportedVersion {
                ver: ENVELOPE_VERSION + 1
            })
        );

        let mut bytes = envelope(RouteId::from_raw(1), 1).encode();
        bytes[1] = 0b0000_0010;
        assert_eq!(
            MediaEnvelope::decode(&bytes),
            Err(EnvelopeError::ReservedFlags { flags: 0b0000_0010 })
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
}
