# `pulsebeam-runtime`

Runtime seams underneath PulseBeam's shard model: bounded mailboxes, task
spawning, deterministic randomness, fatal handling, network adapters, and real
or simulated UDP/TCP implementations.

This crate is allowed to contain shared primitives because it implements the
boundary on which shards run. That exception does not permit shared mutable
packet state in `pulsebeam`; read the
[thread-per-core contract](../pulsebeam/docs/thread-per-core.md) before changing
an ownership or synchronization boundary.

Network implementations must preserve the same observable contract under real
I/O and simulation. Run `cargo test -p pulsebeam-runtime`, then root `just test`.
