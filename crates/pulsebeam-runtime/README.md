# `pulsebeam-runtime`

Runtime seams underneath PulseBeam's shard model: bounded mailboxes, task
spawning, deterministic randomness, fatal handling, network adapters, and real
or simulated UDP/TCP implementations.

This crate may contain shared primitives at the boundary on which shards run.
Mutable packet runtime in `pulsebeam` must remain shard-owned, even when a shard
moves between workers. Read the
[shard-owned work-stealing contract](../pulsebeam/docs/shard-owned-dataplane.md)
before changing an ownership or synchronization boundary.

Network implementations preserve complete-packet receive clock samples through
batching, TCP first-frame routing, and simulated steering queues. Keep
`RecvPacketBatch.received_at` unchanged through application handoffs. A TCP batch
also carries its `TcpFlowId`, a non-reused incarnation within the owning transport.
Use `try_send_on_flow` and `close_flow` for saved output and retirement so an old
stream cannot affect its replacement at the same peer address.

Run `./bazel test //crates/pulsebeam-runtime:unit_tests //crates/pulsebeam-runtime:steering_tests`
for real socket and simulated steering boundaries, then root `./bazel test //:test`.
