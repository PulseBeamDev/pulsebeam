# Shard-Owned Movable Dataplane

PulseBeam uses a **shard-owned dataplane** with runtime-managed execution.

It borrows the discipline of thread-per-core systems: partition hot state,
preserve locality and avoid cross-core synchronization on the packet path.
It is not a strict thread-per-core architecture.

An adaptive worker normally executes multiple logical shards. The node controller
chooses placement; the runtime transfers a whole service at an explicit quiescent
poll boundary. The restart-time work-stealing profile retains the dedicated scheduler
fallback. A shard owns its state regardless of which worker currently executes it.

The design goal is:

> Scale across many cores without making packet processing depend on cache-line contention, global serialization, or another core making progress.

## Ownership model

A shard exclusively owns the mutable dataplane state assigned to it, including its:

* ingress state;
* participant and transport state;
* routing state;
* packet processing state;
* fanout state; and
* egress state.

The scheduler may move execution of that shard between workers, but ownership moves with the shard.

At most one worker may execute a shard at a time.

A worker is therefore an **execution resource**, not an ownership domain.

This distinction is fundamental:

```text
ownership:  participant / transport / routing / fanout / egress
                              │
                            shard
                              │
                  scheduled dynamically onto
                              │
                           worker
                              │
                            core
```

Correctness must not depend on a particular worker or CPU continuing to own a shard.

## Placement

PulseBeam still wants locality.

The placement controller should best-effort colocate shards that communicate heavily so that common packet paths stay on the same worker or nearby execution context whenever possible.

Placement is nevertheless an optimization.

The dataplane must remain correct when:

* communicating shards are placed on different workers;
* a shard migrates to another worker;
* load causes placement to change; or
* ideal colocation cannot be achieved.

Do not encode correctness assumptions into current placement.

A design that is only correct because two shards normally happen to run on the same worker is incorrect.

## Runtime execution and movement

`pulsebeam-runtime` owns process profiles, worker runtimes, task lifecycle,
readiness and timer resources. The SFU supplies budgeted shard logic, logical
identities, queues and absolute deadlines, not backend builders or handles.
Simulation stays on its caller's virtual-time runtime.

The node controller's placement module uses the existing room registry for
initial admission and borrows complete low-rate runtime observations for worker
residency and execution pressure. It holds observation history and cooldowns,
not another room or residency registry. The runtime executes explicit worker
assignments and move requests, without an autonomous balancing policy.
Busy time is normalized by each service's actual sampling interval, then
aggregated over multiple settled windows. Participant counts are only an admission
fallback or tie-breaker; room costs are not assumed equal. Candidate moves must
provide substantial relief across the observed demand range while retaining
destination headroom. A fixed reserve is a heuristic, not a measured warmup bound.
Continuous balancing permits only one outstanding move. Every attempt fences
history through an observation-only hold, followed by fresh windows and candidate
persistence. It moves only an independently useful whole service, not a lone
indivisible hot shard. Every bounded tick explicitly yields before
another tick, even when input remains ready.

Native Linux handoff cancels reactor-bound waits, retains absolute deadlines,
deregisters the same UDP descriptor and transfers exclusive ownership of the
same pinned inner service allocation. The destination registers that descriptor
and initially polls all current input and overdue deadlines with its own waker.
No application-state extraction, socket clone or close/rebind is involved.
Bounded destination capacity is reserved before transfer. Registration failure
retains ownership for one rollback; unsafe restoration or worker failure is a
fatal node error. Shutdown fences new moves and joins resident and queued work.

Movement does not split one shard's mutable runtime across multiple workers.

The scheduler must preserve:

1. single-worker execution of a shard at any instant;
2. the shard's owned state across migration;
3. bounded scheduling overhead;
4. locality when load permits it; and
5. forward progress without requiring unrelated workers to synchronize.

A hot shard must not monopolize a worker indefinitely. Packet processing and control work should remain budgeted so other runnable shards receive service.

Likewise, an idle worker should be able to execute available work without turning the scheduler itself into a highly contended global datapath.

## Shared memory is not the enemy

The architecture does **not** prohibit atomics or shared memory.

The relevant question is:

> What cache-coherence traffic does this access pattern create when PulseBeam runs across many busy cores?

On a coherent multiprocessor, many cores can cheaply hold a stable cache line for reading. Problems arise when cores repeatedly write or perform read-modify-write operations against the same line and ownership of that line must continually move between cores.

The cost comes from the access pattern, not from the Rust type name.

`Relaxed` atomic ordering does not change that fact. It relaxes memory-order constraints; it does not disable cache coherence or make a contended RMW local.

### Usually good shapes

Examples include:

* shard-local mutable state;
* worker-local mutable state;
* independent counters with a single hot writer;
* immutable data shared by readers;
* one-writer, read-mostly state published at control rate;
* complete immutable snapshots replaced infrequently;
* scheduler atomics whose contention is bounded and off the packet hot path; and
* statistics aggregated outside packet-rate processing.

### Suspicious shapes

Review carefully when a design introduces:

* several cores repeatedly writing the same cache line;
* packet-rate atomic RMWs against shared counters;
* global registries touched for every packet;
* locks shared by otherwise independent shards;
* shared reference counts whose increments or decrements occur on different busy cores;
* false sharing between independently owned hot fields;
* queues whose producer and consumer continuously contend on the same control line; or
* a central synchronization point whose cost increases with core count.

These are not forbidden because they contain an atomic or lock. They are dangerous because they can convert independent packet processing into coherence traffic and tail-latency stalls.

## Atomics

Atomics are appropriate when the state genuinely has atomic semantics and the coherence pattern is acceptable.

Good examples include an independent:

* flag;
* counter;
* monotonic value;
* generation;
* scheduler state word; or
* complete state machine encoded in one atomic value.

Several atomics loaded independently do not form a coherent snapshot.

For example:

```text
codec_state.load()
bitrate.load()
decode_targets.load()
```

may observe values from three different logical moments.

If a consumer requires those fields to agree, the state should instead cross the boundary as one coherent value:

```text
StreamSnapshot {
    codec_state,
    bitrate,
    decode_targets,
}
```

The publication mechanism may itself use an atomic or shared pointer. The important property is that readers observe one complete version.

## Cross-shard communication

Mutable packet runtime stays shard-owned.

When one shard needs another shard to do work, communicate through an owned message or another explicit bounded handoff.

A shard must not synchronously wait for another shard to make progress.

Cross-shard lanes must therefore have defined behavior when capacity is exhausted. Packet-path code must not turn queue pressure into an unbounded wait.

The exact policy may vary by lane—drop, replace, coalesce, retry from control state, or another bounded mechanism—but blocking a worker on peer-shard progress is not an acceptable default.

## Packet ownership

Cross-shard packet movement must not accidentally introduce a packet-rate coherence channel.

In particular, shared reference counting deserves scrutiny when allocation happens on one worker and the last reference may be released repeatedly from another. The payload bytes may be immutable while the reference-count cache line is still mutable.

Prefer ownership transfer or destination-owned storage when that avoids hot cross-core lifetime bookkeeping.

This is a performance rule, not a dogma that every byte must always be copied. An alternative is acceptable when its lifetime and access pattern demonstrably keep coherence traffic bounded.

The invariant is:

> Moving packets between shards must not make unrelated workers continuously fight over shared mutable metadata.

## Metrics and observations

Observability must not create a hidden shared packet path.

Packet-rate measurements should normally accumulate in shard-local or otherwise non-contended state.

Aggregation, publication, rendering, and global observation should happen at a lower rate using snapshots, messages, or other bounded publication mechanisms.

A metrics implementation that causes every shard to mutate the same registry or cache lines can violate the dataplane architecture even though metrics are not logically part of forwarding.

Approximate independent measurements are fine when no consumer requires them to describe one coherent instant.

When coherence matters, publish one snapshot.

## Controller boundary

The controller owns coordination decisions such as placement and topology reconciliation.

It must not become part of the per-packet critical path.

Controller-to-shard state arrives as values or explicit operations that the destination shard incorporates into its own state.

A shard forwards packets from the state it currently owns. It does not synchronously consult controller-owned mutable state to process a packet.

Placement decisions may improve locality, but packet processing must continue to obey the same ownership model before, during, and after placement changes.

## Node boundary

Shared-memory mechanisms are always node-local implementation details.

Anything that may cross a node boundary needs a materialized representation.

Do not let a node-local handle, pointer, shared-memory reference, or snapshot implementation leak into the distributed protocol.

The distributed design should remain valid if communicating participants are eventually hosted in different address spaces.

## Blocking

Shard execution must not block a worker waiting for:

* another shard;
* the controller;
* a blocking lock;
* blocking network or filesystem I/O; or
* queue capacity controlled by another execution context.

A worker may execute many participants through many shards. Parking that worker therefore creates latency for unrelated traffic.

Potentially blocking work belongs outside the shard packet path behind an explicit asynchronous or message boundary.

## Performance standard

The architecture should be evaluated for **many-core behavior**, not merely correctness or single-core throughput.

A change to shared state or scheduling deserves additional scrutiny when its cost can increase with:

* worker count;
* shard count;
* packet rate;
* fanout;
* the number of cores touching one cache line; or
* migration frequency.

The desired scaling shape is that adding cores primarily adds independent packet-processing capacity rather than increasing synchronization cost.

Tail behavior matters as much as average throughput. A design that benchmarks well on average but periodically introduces cross-core stalls into forwarding is a regression.

## What this architecture is not

It is not strict thread-per-core.

A shard is not permanently pinned to one thread or CPU.

It is not shared-everything work stealing.

Scheduling and migration change where an ownership domain executes; they do not dissolve that ownership domain.

It is not "no atomics."

Atomics are useful building blocks. Packet-rate cache-line contention is the thing to avoid.

It is not correctness-by-colocation.

The placement controller improves locality, but communication across placement boundaries remains a first-class supported path.

## Review rule

When introducing shared state into the dataplane, answer these questions:

1. Who writes it?
2. Who reads it?
3. At what frequency?
4. Which cores can touch the same cache line?
5. Does any operation require RMW ownership of that line?
6. Can independently owned hot fields falsely share a line?
7. Does the cost increase with packet rate or core count?
8. Does correctness require several independently changing values to agree?
9. Can the same information instead move at control rate as one value?
10. What happens when communicating shards execute on different workers?

The primitive itself does not decide whether the design is valid. The answers do.

## Enforcement

Architectural lints should act as **review gates**, not blanket declarations that a primitive is forbidden.

Types such as `Arc`, atomics, locks, or shared buffers may therefore be linted because their introduction deserves explicit review.

A narrow exception is appropriate when its access pattern preserves this document's ownership, locality, and coherence constraints.

Do not add broad module- or crate-level exceptions merely to silence the gate.

When a hot shared primitive is genuinely necessary, validate its many-core behavior rather than reasoning only from its uncontended cost.
