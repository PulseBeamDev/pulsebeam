# Shard-Affine Dataplane

- The shard, not the worker thread or CPU core, is the unit of mutable dataplane ownership. A shard exclusively owns its ingress, fanout, egress, participant, transport, routing, and packet state, and it must never execute concurrently on two workers.
- Workers execute many shards and may steal runnable shards. Placement is best effort: the controller should colocate related work when possible, but correctness must not depend on worker or core affinity.
- Keep cross-shard work bounded and non-blocking. Shared primitives are judged by coherence behavior, not by type alone: read-mostly or infrequently published state and independent atomics can be appropriate; hot cross-core writes/RMWs, false sharing, global serialization, and multi-atomic snapshots are not.
- Anything crossing a node boundary is a materialized value, never a node-local memory handle.
- `docs/shard-affine-dataplane.md` explains the model; `clippy.toml` is the executable review boundary for shared-state and blocking primitives.
