# Thread-per-core

This SFU is thread-per-core. A shard owns its participants, its routes, its
packet buffers and its measurements outright, runs on one core, and reaches
other shards only by message. `ShardWorker` is `!Send` on purpose.

That is unusual enough that the obvious "improvement" is usually a regression.
This file says what the rules are and why, so the reasoning survives the person
who had it. **`clippy.toml` denies the shared-state primitives; this is the
document its `reason` strings point at.**

## The rules

1. **A shard does not share mutable state with another shard.** Not behind a
   lock, not behind atomics. If two shards need to agree on something, one sends
   the other a message.
2. **A shard never blocks on another shard.** No `Mutex`, no awaiting a full
   queue held by a peer. (The one place two directions could deadlock is
   documented at `ShardWorker::flush_shard_events`.)
3. **Refcounts are core-local.** An `Arc` whose count is touched by two cores on
   a per-packet path is a bug, even when it is "just" an increment.
4. **Anything crossing a shard boundary is a value, not a handle.** A handle
   implies shared memory, and cross-node there is none.

## Why, concretely

### Sharing is not slow because of the lock. It is slow because of the line.

An uncontended atomic increment is a few nanoseconds. A *contended* one — where
two cores each hold the cache line in their L1 and take turns invalidating each
other — costs hundreds, and it stalls the other core too. The cost does not
appear in a profile as "synchronisation"; it appears as every function on both
cores getting slower.

This is why `RtpPacket::to_transit` copies the payload instead of sharing the
`Arc`: the refcount header sits immediately before the bytes, so a remote drop
invalidates the line a reader is mid-way through. It is also why
`RtpPacket::rehome_extensions` exists — the payload copy left a hole, because
cloning str0m's extension map clones the `Arc<dyn Any>` inside it.

### Several atomics do not make a snapshot

This is the failure mode that keeps getting rediscovered, so it is worth being
precise.

`rtp::monitor::StreamStateInner` holds a stream's measurements as separate
atomics. `AllocationEngine::new` reads eight of them to build one `LayerSnap`.
Each read is individually atomic. **The set is not.** A writer can land between
any two of them, so the allocator can decide against a state that never existed:
`decode_targets` from a new Dependency Descriptor structure paired with
`decode_target_kbps` from the previous one, and a rung costed against a ladder
that does not have it.

The field comment on `bitrates` shows the shape of the trap — two values were
packed into one `AtomicU64` precisely because a torn read between them was
found. That fix does not generalise: it works for 64 bits of state and there is
more than that.

Atomics are fine when nothing reads two of them expecting agreement.
`ShardMetrics` qualifies: a fixed, preallocated counter set, written by its own
shard, read for load reporting where a skewed pair changes nothing.

### The design has to work with no shared memory at all

The goal is more than one node. A destination node cannot read a publisher
node's atomics — there is no address space to reach into. So any design that
depends on shared measurement state is not a shortcut to be optimised later; it
is a design that stops working at the boundary the project is aimed at.

Message-passing is not the slower option here. It is the only option that scales
past one box, and it gives consistent snapshots for free, because a message is
one coherent value.

## Known violations

Both are annotated at their definitions and both are on the way out. They are
listed here so nobody mistakes them for precedent.

| Where | What | Why it is wrong |
|---|---|---|
| `rtp::monitor::StreamState` | `Arc` of eight loose atomics, shared publisher-shard → every subscriber shard | Torn snapshots (above); cross-core refcount; cannot cross a node |
| `stream_registry::StreamRegistry` | node-global `RwLock<HashMap>` | A shard reaching into shared state to find those handles |

The fix for both is the same and is the cross-node design anyway: the
publisher's shard periodically sends each destination an immutable
`StreamStats` value on the best-effort lane — route-addressed, latest-wins,
losing one just means a slightly stale estimate. One message is one consistent
snapshot, the refcount problem disappears with the handle, and the registry has
nothing left to hold.

## Escape hatches

`#[allow(clippy::disallowed_types)]` with a comment saying which of these it is:

- **Startup wiring.** One `Arc` per shard, cloned before any shard runs, never
  touched again (`Arc<ShardMetrics>`).
- **Fixed preallocated counters.** `ShardMetrics`, per the rule above.
- **Forced by a dependency.** str0m's `RtpWrite` takes `Arc<[u8]>` and its
  extension map stores `Arc<dyn Any>`. Keep them core-local; do not let one
  cross a shard boundary.
- **Below the shard model.** `pulsebeam-runtime` implements the seams shards are
  built from. Nothing there licenses an `Arc` inside a shard.
- **Not a shard.** The agent, simulator and CLI are ordinary async programs.

If a use does not fit one of those, it is rule 1 and the answer is a message.
