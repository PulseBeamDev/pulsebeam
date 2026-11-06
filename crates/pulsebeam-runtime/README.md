# `pulsebeam-runtime`

Owned execution underneath PulseBeam's shard model: process profiles, control
bootstrap, dataplane workers, movable services, clocks, cancellation, task joins,
TCP/HTTP adapters and host-owned network simulation. Tokio implements these APIs;
application scheduling policy does not depend on its handles or builders.
Reusable UDP socket I/O belongs to [`pulsebeam-udp`](../pulsebeam-udp/README.md),
which does not depend on this crate.

Bounded ownership handoff lives in [pulsebeam-queue](../pulsebeam-queue/README.md).
Consumers that only need a queue should depend on that crate directly.

This crate may contain shared primitives at the boundary on which shards run.
Mutable packet runtime in `pulsebeam` must remain shard-owned, even when a shard
moves between workers. Read the
[shard-owned dataplane contract](../pulsebeam/docs/shard-owned-dataplane.md)
before changing an ownership or synchronization boundary.

`rt::ExecutionConfig` defaults to one adaptive worker and one logical service.
Adaptive workers own current-thread runtimes. The executor publishes complete
low-rate execution observations and executes explicit placement requests; the
SFU node controller owns placement policy. Consumers borrow observations rather
than maintaining another residency registry. Poll occupancy and runnable-to-poll
delay stay local until publication. Each service observation carries its own
sampling time and migration generation: poll busy time restarts with the worker
envelope, while scheduling wait follows the pinned service lifetime. Consumers
must fence baselines across moves and rollback rather than difference unrelated
counters. No shared packet-rate polling registry is required.

`rt::ManagedService` is allocated and pinned once. Its application logic performs
one bounded poll/tick using runtime-owned UDP readiness and absolute deadlines.
A handoff drops reactor-bound waits, deregisters the same Linux socket and
transfers the same pinned box through reserved bounded control capacity. Failed
destination registration retains ownership for one source rollback. Failed
restoration or a dead service stops the runtime; services are not reconstructed.
`DataExecutor::finish` joins owned workers and accounts for queued handoffs.

`work-stealing` and `thread-per-core` profiles remain restart-time alternatives.
Synthetic sockets use `ExecutionConfig::on_current_runtime` and never leave the
simulation thread. Cross-reactor migration is native Linux only; other native
platforms retain local-worker execution without automatic moves. This default
cutover is correctness-qualified, not evidence of improved latency or efficiency.

UDP implementations preserve complete-packet receive clock samples through
batching and simulated steering queues. Keep `RecvPacketBatch.received_at`
unchanged through application handoffs. Media TCP is not a UDP batch backend:
[`pulsebeam-tcp`](../pulsebeam-tcp/README.md) owns RFC 4571 framing, and the
server's private `ice_tcp` integration owns admission and bounded flow policy.
`role::IdleRole` owns the optional low-priority executor and its joined lifecycle;
`net::tcp` owns native/simulated listener binding, framing adaptation and optional
HTTP serving. Core's legacy listener can transfer into the runtime adapter
without a reverse dependency.

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
