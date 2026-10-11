# `pulsebeam-runtime`

Runtime seams underneath PulseBeam's shard model: bounded mailboxes, task
spawning, deterministic randomness, fatal handling, network adapters, and real
or simulated UDP/TCP implementations.

This crate may contain shared primitives at the boundary on which shards run.
Mutable packet runtime in `pulsebeam` must remain shard-owned, even when a shard
moves between workers. Read the
[shard-owned work-stealing contract](../pulsebeam/docs/shard-owned-dataplane.md)
before changing an ownership or synchronization boundary.

Network implementations must preserve the same observable contract under real
I/O and simulation. Run `./bazel test //crates/pulsebeam-runtime:unit_tests`, then root `./bazel test //:test`.
