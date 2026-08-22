# Thread-per-core

This SFU is thread-per-core. A shard owns its mutable participants, routes,
packet buffers and packet runtime outright, runs on one core, and reaches other
shards only by message. `ShardWorker` is `!Send` on purpose. Immutable,
coherent snapshots may be shared across cores when one node-local writer owns
publication.

That is unusual enough that the obvious "improvement" is usually a regression.
This file says what the rules are and why, so the reasoning survives the person
who had it. **`clippy.toml` denies the shared-state primitives; this is the
document its `reason` strings point at.**

## The rules

1. **A shard does not share mutable packet runtime with another shard.** Not
   behind a lock, not behind a shared collection, and not through per-packet
   reference counting. If two shards need to agree on mutable runtime, one
   sends the other a message.
2. **A shard never blocks on another shard.** No `Mutex`, no awaiting a full
   queue held by a peer. (The one place two directions could deadlock is
   documented at `ShardWorker::flush_shard_events`.)
3. **Immutable snapshots may cross cores.** A node-local snapshot has one
   publication locus, replaces the complete value, and may have readers on any
   core. `ArcSwap` and `left-right` are valid mechanisms for this contract.
4. **Anything crossing a node boundary is a value, not a handle.** A local
   snapshot handle may be shared within its node, but wire messages contain
   owned/materialized values.
5. **Atomics represent independent facts.** A single atomic may hold a flag,
   counter, monotonic value, or complete one-word state machine. Several atomic
   loads do not form a coherent snapshot.

## Why, concretely

### Shared mutable state is not free because the lock is absent.

An uncontended atomic increment is a few nanoseconds. A *contended* one — where
two cores each hold the cache line in their L1 and take turns invalidating each
other — costs hundreds, and it stalls the other core too. Immutable snapshot
reads are a different shape: readers share stable data while one writer
publishes a replacement, rather than mutating packet-path state in place.

This is why `RtpPacket::to_transit` copies the payload instead of sharing the
`Arc`: the refcount header sits immediately before the bytes, so a remote drop
invalidates the line a reader is mid-way through. It is also why
`RtpPacket::rehome_extensions` exists — the payload copy left a hole, because
cloning str0m's extension map clones the `Arc<dyn Any>` inside it.

### Several atomics do not make a snapshot

This is the failure mode that keeps getting rediscovered, so it is worth being
precise.

It bit this codebase, so the example is real rather than hypothetical.
`StreamStateInner` held a stream's measurements as separate atomics, and
`AllocationEngine::new` read eight of them to build one `LayerSnap`. Each read
was individually atomic. **The set was not.** A writer landing between any two
let the allocator decide against a state that never existed: `decode_targets`
from a new Dependency Descriptor structure paired with a `decode_target_kbps`
ladder from the previous one, costing a rung that did not exist.

The trap is that the local fix looks like it works. Two of those fields —
reactive and stable bitrate — *were* packed into one `AtomicU64`, with a comment
explaining that a torn read between them had been found. That is a correct fix
for one pair and no help at all for the other six, and it does not generalise
past 64 bits. If you find yourself packing fields to make a snapshot atomic, the
snapshot wants to be a value.

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

## How measurements actually move

Worth reading before proposing a shortcut, because the obvious one was tried.

Measurements used to be `StreamState`: an `Arc` of eight atomics that a
publisher's shard wrote and every subscriber's shard read directly, plus a
node-global `RwLock` registry to hand the handles out. That was wrong three
ways — the refcount crossed cores on a per-packet path, the eight reads never
formed a snapshot, and none of it can work when the subscriber is on another
node.

Now:

- `StreamMonitor` keeps its measurements as **plain fields** and produces a
  `StreamStats` **value** on demand. It is `Copy`; there is nothing to share.
- On the slow poll a publishing participant hands its own shard a snapshot per
  track (`ShardInternalEvent::TrackStatsUpdated`). Within one shard, so still
  no sharing.
- The shard applies it locally and sends `ShardFrame::Telemetry` to every
  destination holding a route for that track — route-addressed, best-effort,
  latest-wins. Losing one costs a slightly stale allocation and nothing else.
- A destination replaces its `layer_states` wholesale. **One message is one
  coherent view**, which is the property eight atomics could not give.

The same path works unchanged when the destination is another node, because it
was never reading memory it did not own.

Do not replace a coherent snapshot with independently shared fields. A shared
handle is appropriate when it has one writer, immutable readers, and an owned
cross-node representation.

### Metrics travel the same way

`metrics::counter!` and friends are welcome anywhere, including the packet
path — what matters is which recorder they resolve against. A process-global
recorder is the failure this document is about: `metrics_util::Registry` is a
locked map whose cache lines every shard writes and which `render()` walks from
the control thread, and it put spikes in the forwarding tail in production.

So each shard installs its own `shard::recorder::ShardRecorder` around its tick
(`with_local_recorder`), and once a second copies out a `ShardStatsReport` — a
plain value, counters and fixed histogram buckets, no keys — and `try_send`s it
to `control::stats_aggregator`, which sums across shards and renders the
Prometheus text. Two properties make that lane free to drop:

- The values are **cumulative and absolute**, never deltas, so a lost report
  costs staleness until the next one and nothing else.
- The aggregator **replaces** a shard's contribution and never clears it. A
  shard leaving the sum would make an exported counter fall, and Prometheus
  reads any counter decrease as a reset.

Metric names are summed across shards by default. Only the short allowlist in
`stats_aggregator` keeps a `shard` label, because a shard dimension on every
series multiplies cardinality by core count.

Installing the recorder per tick rather than per thread is deliberate: under
`WorkerExecution::SharedRuntime` every shard of a node shares one thread, so
attribution has to come from the installed recorder, not from thread identity.

## Shared-state boundaries

`#[allow(clippy::disallowed_types)]` with a comment saying which boundary it
implements. The important distinction is mutable packet runtime versus
immutable node-local publication.

- **Tests.** A test is not a shard; a counter for unique ids is convenience, not
  architecture. Allowed at the test module, not the file, so it cannot drift
  over production code in the same file.
- **Immutable snapshots.** One writer per node-local logical snapshot, with
  cross-core readers. The writer capability is not duplicated, and every
  publication replaces one complete value (`ArcSwap<T>` or `left-right`).
- **Startup wiring.** One `Arc` per shard, cloned before any shard runs, never
  touched again (`Arc<ShardMetrics>`).
- **Fixed preallocated counters.** `ShardMetrics`, per the rule above.
- **Forced by a dependency.** str0m's `RtpWrite` takes `Arc<[u8]>` and its
  extension map stores `Arc<dyn Any>`. `metrics::Counter::from_arc` demands
  `Arc<F: Send + Sync>`, so `shard::recorder`'s slots are `Arc<AtomicU64>` —
  registered, written and read by one shard on one core, so nothing contends.
  Keep them core-local; do not let one cross a shard boundary.
- **Below the shard model.** `pulsebeam-runtime` implements the seams shards are
  built from. Nothing there licenses mutable shared packet state inside a
  shard.
- **Not a shard.** The agent, simulator and CLI are ordinary async programs.

## Enforcement

`make lint-check` fails rather than warns, and CI runs it as its own job.
`cargo clippy --fix` leaves everything as a warning, and a warning in a
hundred-line build log is not a gate.

Three places, deliberately:

- **`clippy.toml`** — the architectural rules, one `reason` string each:
  `disallowed-types` (shared state) and `disallowed-methods` (ambient clock,
  unseeded randomness, blocking).
- **`[workspace.lints]` in `Cargo.toml`** — the levels, plus the correctness,
  allocation and readability tiers. Tiered by *measured* violation count, with
  a note on what is deliberately left out and why, so it does not get
  re-litigated from scratch.
- **Module-level `#![allow]`** — every exception, next to the code it excuses.

### Determinism is not enforced here, on purpose

An early version of this config denied `SystemTime::now`,
`std::time::Instant::now` and `thread_rng` on determinism grounds. That was
wrong, and it is worth recording why so it does not come back.

`pulsebeam-simulator` overrides `clock_gettime` (both `CLOCK_REALTIME` and
`CLOCK_MONOTONIC`) and `getrandom(2)` for the whole process. Under a plan those
calls already return simulated time and seeded bytes. The shim is *stronger*
than a lint could be, because it also reaches inside dependencies — tracing,
hashers, DTLS key generation — which no rule about our own source can touch.
That is the entire reason `sim_clock.rs` and `sim_rand.rs` exist.

So banning the calls would forbid something the harness has already made safe,
and would still not cover the case the harness was written for. Read those two
modules before proposing a lint here.

`std::thread::sleep` is denied, but on blocking grounds rather than
determinism: it parks the shard, and under simulation it burns real time
turmoil was about to skip.

`node.rs` still reads the wall clock once, for the node's single `WallAnchor`.
That is an architectural convention — one timeline per node — not something the
lint enforces.
