# Shard-Owned Work-Stealing Dataplane

Read `docs/shard-owned-dataplane.md` before changing dataplane ownership, scheduling, routing, fanout, packet movement, metrics, or cross-shard coordination.

* **The shard is the unit of mutable dataplane ownership.** A shard owns its ingress, participant and transport state, routing, fanout, packet state, and egress. Ownership does not belong to a worker thread or CPU core.
* **Workers are execution resources.** A worker executes many shards, and runnable shards may be stolen or migrated. A shard must never execute concurrently on multiple workers.
* **Placement is an optimization, not a correctness boundary.** The placement controller should best-effort colocate communicating work, but correctness must hold under any valid placement.
* **Preserve locality rather than banning shared memory.** Atomics and shared immutable state are appropriate when their access pattern has bounded coherence cost. Reject packet-rate cross-core writes or RMWs, false sharing, shared refcount churn, global serialization, and other designs that make hot cache lines bounce between cores.
* **Memory ordering and cache coherence are separate concerns.** `Relaxed` may reduce ordering constraints but does not make a contended cache line local.
* **Several atomics are not a snapshot.** When fields must agree, publish or transfer one complete immutable value rather than reconstructing state from independently changing atomics.
* **Cross-shard coordination must stay bounded and non-blocking.** Transfer owned work or values; do not make one shard synchronously wait for another.
* **Anything crossing a node boundary is a materialized value, never a node-local memory handle.**
* `clippy.toml` is a review boundary for dangerous primitives, not a claim that those primitives are universally forbidden. Any exception must preserve the ownership and coherence invariants above.
