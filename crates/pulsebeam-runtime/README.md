# `pulsebeam-runtime`

Runtime seams underneath PulseBeam's shard model: task spawning, deterministic
randomness, fatal handling, network adapters, and real or simulated UDP
implementations.

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

Run `./bazel test //crates/pulsebeam-runtime:unit_tests //crates/pulsebeam-runtime:steering_tests`
for fallback, packet-shape and simulated steering boundaries.
`./bazel test //crates/pulsebeam-runtime:transport_conformance` requires real
Linux IPv4/IPv6 loopback, dual-stack UDP, UDP segmentation and UDP GRO. It sends
marked segments through production sockets and requires actual GRO ancillary
segment boundaries, source/destination integrity and receive-clock preservation.
Unsupported capability and failed exercise are failures, not passing skips.
Use a Linux test host with IPv6 enabled on loopback and kernel UDP offload
support; the tests do not change networking, sysctls or privileges.

The strict route belongs to root `//:check` and complete `//:test`, not an
additional root-fast network matrix. Ordinary runtime fallback remains supported.
The simulator's batch adapter models segment boundaries and preserved clocks
over its scalar socket path, not kernel syscalls or offload. The production
node's complementary [SDK witness](../pulsebeam/README.md) proves concurrent
ICE selection and application delivery, which raw packet tests do not.
