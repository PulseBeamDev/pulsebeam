# `pulsebeam-runtime`

Runtime seams underneath PulseBeam's shard model: task spawning, deterministic
randomness, fatal handling, and host-owned network simulation. Reusable UDP
socket I/O belongs to [`pulsebeam-udp`](../pulsebeam-udp/README.md), which does
not depend on this crate.

Bounded ownership handoff lives in [pulsebeam-queue](../pulsebeam-queue/README.md).
Consumers that only need a queue should depend on that crate directly.

This crate may contain shared primitives at the boundary on which shards run.
Mutable packet runtime in `pulsebeam` must remain shard-owned, even when a shard
moves between workers. Read the
[shard-owned work-stealing contract](../pulsebeam/docs/shard-owned-dataplane.md)
before changing an ownership or synchronization boundary.

UDP implementations preserve complete-packet receive clock samples through
batching and simulated steering queues. Keep `RecvPacketBatch.received_at`
unchanged through application handoffs. Media TCP is not a UDP batch backend:
[`pulsebeam-tcp`](../pulsebeam-tcp/README.md) owns RFC 4571 framing, and the
server's private `ice_tcp` integration owns optional listener, executor, admission
and bounded flow queues. HTTP sockets remain ordinary Tokio or Turmoil streams.

The simulation adapter supplies UDP device I/O to `pulsebeam-udp`. It owns
reuseport membership, routing classification, and network shaping. Entries are
submitted synchronously and atomically before network loss, duplication, delay
or reordering, without an extra task queue ahead of the modeled link. A notified
release task drives delayed traffic without idle polling.
Simulated receives retain the envelope's actual destination and original arrival
time, including wildcard binds. Host identity, receive metadata and flow keys do
not depend on advertised addresses. Scalar mode has no simulated GSO/GRO. Batch mode models
segment boundaries, not kernel syscalls or hardware offload.

Run `./bazel test //crates/pulsebeam-runtime:unit_tests //crates/pulsebeam-runtime:steering_tests`
for runtime and host steering contracts. Raw Linux socket conformance belongs to
`//crates/pulsebeam-udp:transport_conformance`; the production node's complementary
[SDK witness](../pulsebeam/README.md) proves concurrent ICE selection and
application delivery, which raw packet tests do not.
