# `pulsebeam`

The PulseBeam WebRTC SFU server. The control plane owns topology and compiles
owned routing updates; work-stealing shards own participants, transports,
packet buffers, routes, and forwarding state outright.

Read these contracts before changing the corresponding subsystem:

- [Naming boundaries](docs/naming.md)
- [Shard-owned work-stealing dataplane](docs/shard-owned-dataplane.md)
- [Routing and compiled plans](docs/routing.md)
- [Cluster topology and ownership (proposed)](docs/cluster.md)
- [Linux server requirements](docs/linux-only.md)
- [Architecture diagram](docs/architecture.svg)

Shared mutable packet state, cross-shard handles, blocking calls, and
multi-atomic snapshots are architectural regressions. Cross-shard and
cross-node coordination uses owned messages.

Nodes require an NTP/PTP-disciplined host wall clock in a common NTP-compatible
time domain. Controller admission and shard service refresh paired host-clock
samples instead of extrapolating a startup-only anchor. Backward wall-clock steps
plateau execution time until the host catches up; ordinary clock slewing is
followed. `clock::full_ntp_micros` converts wall-clock samples to microseconds
since 1900 without truncating to the NTP 32.32 wire era. Packet receive samples
remain separate and unchanged
through queueing and shard handoffs.

Run focused crate tests while iterating. Before handoff, run root `./bazel test //:check`
and `./bazel test //:test`. UDP steering is an extension point: higher layers can provide
a `Steering` implementation through `NodeBuilder::with_steering`. Server development and profiling entrypoints are documented in
[the Bazel workflow](../../docs/build.md). Privileged traffic shaping remains
an optional manual diagnostic, not package preparation or acceptance.
