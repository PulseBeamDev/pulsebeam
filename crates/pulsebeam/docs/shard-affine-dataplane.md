# Shard-affine dataplane

PulseBeam uses a shard-affine, work-stealing dataplane. Workers execute many
shards, and runnable shards may move between workers. The **shard**, not the
worker thread or CPU core, is the unit of mutable dataplane ownership.

A shard owns its participant transports, ingress, fanout, egress, routing,
packet buffers, and packet runtime regardless of which worker executes it. The
scheduler may move that execution, but it must never execute the same shard
concurrently on two workers.

This keeps the useful discipline of thread-per-core systems without requiring
fixed core ownership: keep packet state local, keep work bounded and
non-blocking, and keep cross-core coherence traffic out of hot paths.
`clippy.toml` turns primitives that can violate those properties into explicit
review boundaries; it does not mean every atomic or shared value is inherently
wrong.

## The rules

1. **A shard exclusively owns mutable dataplane state.** Another shard does not
   concurrently mutate its participant, transport, routing, ingress, fanout,
   egress, or packet-runtime state through a lock, shared collection, or handle.
2. **A shard has one executor at a time.** Work stealing may move a runnable
   shard between workers, but it transfers execution; it does not create
   concurrent access to shard-owned state.
3. **Placement is an optimization, not a correctness boundary.** The placement
   controller best-effort colocates related work. Routing and forwarding must
   remain correct when related shards run on different workers or cores.
4. **Cross-shard coordination is bounded and non-blocking.** Use owned messages,
   bounded queues, or explicitly reviewed immutable publication. A shard does
   not wait synchronously for another shard to make progress.
5. **Routing and topology state is materialized per owner.** Controller updates
   cross the boundary as values; packet-path correctness does not depend on a
   shared mutable routing image.
6. **Shared state is judged by coherence behavior, not by type alone.**
   Read-mostly state, a single logical writer with infrequent publication, or an
   independent atomic fact can be appropriate. Packet-rate cross-core writes,
   RMWs, ownership transfer, false sharing, or global serialization are not.
7. **Several atomics do not form a coherent snapshot.** When fields must agree,
   publish and read one complete immutable value or one genuine single-word
   state machine.
8. **Anything crossing a node boundary is a value, not a memory handle.** A
   node-local shared snapshot may be an optimization, but the distributed
   representation is materialized and owned.

## Execution and placement

Workers are execution resources, not ownership domains. A worker can run many
shards, and work stealing can rebalance runnable shards when load is uneven.
Code inside a shard therefore must not use thread identity as shard identity or
assume that a shard remains on one CPU for its lifetime.

The placement controller tries to keep related participants and traffic close
because same-worker or same-core execution avoids queueing, copies, cache
misses, and coherence traffic. That affinity is valuable but best effort. A
placement miss, later rebalance, or stolen shard may cost performance; it must
not change behavior.

Ingress, fanout, and egress remain shard-owned through those scheduling
changes. Moving execution is preferable to weakening ownership just to make
work easier to schedule.

## Cache coherence is the shared-state boundary

The important distinction is not "atomic" versus "non-atomic". It is the
access pattern seen by the cache-coherence fabric.

A stable cache line can be read concurrently by many cores cheaply. An
infrequently updated single-writer value can also be reasonable because the
invalidation cost is bounded. The dangerous shape is a hot line that different
cores repeatedly write or modify: each write or RMW requires ownership of the
line and invalidates other cached copies, so the line bounces between cores and
creates latency for unrelated work sharing those cores.

Memory ordering does not remove that cost. `Relaxed` can avoid stronger ordering
constraints, but a `Relaxed` write or RMW still participates in cache coherence.
Likewise, unrelated atomics placed on one hot cache line can false-share and
bounce even though the logical values are independent.

This is why a shared primitive needs an access-pattern argument rather than a
blanket rule. Good questions are:

- How many logical writers are there?
- At what rate do writers modify the line?
- Can readers remain in a shared/read-only state most of the time?
- Can unrelated hot fields land on the same cache line?
- Does the design still behave smoothly when shards run on different cores?
- Does adding cores increase useful throughput, or only coherence traffic?

Cross-shard packet ownership is intentionally conservative. A refcounted payload
whose clone/drop lifecycle executes on different cores can turn the refcount
cache line into packet-rate coherence traffic. Materializing ownership for the
destination avoids making best-effort colocation a hidden performance
requirement. Any change to that trade-off should be justified with the actual
cross-core access pattern and tail-latency measurements, not with the fact that
an atomic increment benchmarks cheaply in isolation.

## Several atomics do not make a snapshot

This failure mode has already occurred in the allocator. `StreamStateInner`
held a stream's measurements as separate atomics, and `AllocationEngine::new`
read eight of them to build one `LayerSnap`. Each read was individually atomic;
the set was not. A writer landing between reads could combine fields from
states that never existed together.

Packing a related pair into one `AtomicU64` can make that pair coherent, but it
does not generalize to a larger logical snapshot. If a consumer expects fields
to agree, the snapshot wants to be one value.

Independent counters are different. `ShardMetrics` can use separate values
because a small skew between counters has no semantic meaning. Its writer is
the shard as a logical owner; the worker executing that shard may change over
time.

## Routing and topology

The control plane owns canonical topology, allocation, and reconciliation. It
publishes owned updates to shards, and each shard materializes the routing and
participant state it needs locally.

Cross-shard media also crosses an explicit ownership boundary. It carries the
materialized packet data and route information needed by the destination rather
than borrowing another shard's mutable runtime. Forwarding never blocks for the
controller or for another shard.

Work stealing does not weaken this model. It changes where a shard executes,
not who owns its state.

## Measurements and immutable publication

A coherent immutable snapshot may be shared within one node when the access
pattern is suitable: one logical writer publishes a complete version,
control-rate readers load that version, and packet forwarding does not mutate it
in place.

`VideoStats` follows this shape. It is a node-local optimization, not the
cross-node representation. Before that state crosses a node boundary it must be
materialized into an owned value and refreshed through the distributed
protocol.

Do not use this exception to turn routing plans or packet runtime into shared
mutable state.

### Metrics follow shard identity, not worker identity

Shard metrics are attributed to the shard even though workers execute many
shards and work stealing may move a shard. A recorder therefore has to be
installed for the shard's execution scope; thread-local identity alone is not a
valid attribution mechanism.

Cumulative shard reports can be copied to the control plane at a slow rate and
aggregated there. That slow-path read pattern is fundamentally different from a
process-global packet-path registry whose locks or writable cache lines every
shard touches.

A `shard` label also scales with **shard count**, not core count, so it should
remain a deliberate low-cardinality exception rather than a default metric
dimension.

## Shared-state review boundaries

`#[allow(clippy::disallowed_types)]` is a review marker, not an admission that
all sharing is forbidden. Keep the exception narrow and explain the access
pattern that makes it safe for the shard-affine model.

Typical legitimate shapes include:

- immutable node-local snapshots with one logical writer and control-rate
  readers;
- fixed preallocated counters with one logical shard writer and infrequent
  control-plane reads;
- atomics required by an external API when they do not create packet-rate
  multi-core writes or RMW contention;
- startup or control-plane state that is outside the dataplane hot path.

A legitimate shared primitive in one of those boundaries does not license
shared mutable participant, routing, ingress, fanout, egress, or packet state.

## Blocking

A worker may execute many shards. Blocking that worker can therefore delay
unrelated shards and create the exact tail-latency hiccups the scheduler is
supposed to smooth out. Dataplane work stays bounded and non-blocking; blocking
work belongs outside the dataplane or behind an explicit offload boundary.

Under simulation, real thread sleeps are additionally wrong because they burn
wall time while the simulated clock could have advanced deterministically.

## Enforcement

`clippy.toml` marks shared ownership, locks, atomics, blocking, and process-global
metric recorders as architectural review boundaries. The reason strings should
point here and describe the performance or ownership property being protected,
not claim that a primitive is forbidden merely because the design is
"thread-per-core."

Workspace lint levels and repository checks are the executable gate. Keep
exceptions beside the code they excuse, and make the exception explain why its
access pattern does not introduce cross-shard mutation, hot cache-line bouncing,
global serialization, or a blocking dependency.
