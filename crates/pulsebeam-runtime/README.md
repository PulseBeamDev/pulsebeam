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
