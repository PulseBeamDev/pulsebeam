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

## Media TCP fallback

UDP is the optimized dataplane. Passive ICE-TCP uses one optional
`pb-ice-tcp` OS thread with a Tokio current-thread runtime and one task per
admitted connection. The thread uses Linux `SCHED_IDLE` and a spare non-dataplane
core when available. Startup fails rather than advertising TCP if binding or
the required scheduling policy fails. Thread separation does not isolate kernel,
cache or NIC work. `CAP_SYS_NICE` is not required: the dataplane's
`SCHED_FIFO` promotion is a best-effort optimization, and the TCP executor's
`SCHED_IDLE` policy does not require that capability.

`NodeBuilder::ice_tcp(IceTcpConfig::Disabled)` and `udp_only()` create no media
TCP listener, executor, tasks, shard inboxes or candidate. HTTP signaling and
metrics listeners are independent. Default configuration retains passive TCP;
`IceTcpConfig::Passive { bind, limits }` supplies an explicit listener and
`IceTcpLimits`. External candidate addresses must be reachable at their
advertised TCP ports. Direct ICE-TCP is not HTTPS, TURN or TLS multiplexing.

TCP admission caps pending, active, per-IP and total connection tasks. The total
task cap also bounds delayed terminal notices. Each enabled shard has a separate
TCP inbox drained after UDP and normal shard work, with at most eight items and
65,535 payload bytes per tick. A frame quota follows input through deferred RTC
processing, so removing an inbox item cannot escape its byte limit. Initial
limits use a 1,500-byte frame cap and 32-frame ingress/outbound queues. Configure
a larger frame cap explicitly for endpoints that require it. These are finite
defaults, not deployment capacity guarantees; configure them to fit the
node's resource budget.

The controller is not in the connection or packet path. First-STUN ufrags are
untrusted routing hints; the destination shard checks the current route epoch
and RTC performs authentication. Adoption precedes subsequent input, and flow
incarnations fence delayed frames, replies and close events. Full ingress or
egress closes that flow instead of dropping middle frames or blocking a shard.
Sockets and admission release before bounded terminal-notice waits.

Observe `ice_tcp_pending`, `ice_tcp_active`, `ice_tcp_rejected`,
`ice_tcp_ingress_close`, `ice_tcp_egress_close`,
`ice_tcp_stalled_write_close` and `ice_tcp_thread_cpu_ns` separately from UDP and
control-plane metrics. Thread CPU is sampled on the TCP executor in production,
not estimated from simulation time.

Nodes require an NTP/PTP-disciplined host wall clock in a common NTP-compatible
time domain. Controller admission and shard service refresh paired host-clock
samples instead of extrapolating a startup-only anchor. Backward wall-clock steps
plateau execution time until the host catches up; ordinary clock slewing is
followed. `clock::full_ntp_micros` converts wall-clock samples to microseconds
since 1900 without truncating to the NTP 32.32 wire era. Packet receive samples
remain separate and unchanged
through queueing and shard handoffs.

Run focused crate tests while iterating. Before handoff, run root `./bazel test //:check`
and `./bazel test //:test`. The production-configured owner target
`//crates/pulsebeam:transport_conformance` uses one once-bound dual-stack UDP
listener and two concurrent threaded native SDK peers per UDP/TCP witness. It requires actual
IPv4/IPv6 HTTP candidate advertisements, selected-pair statistics and distinct
bidirectional application topic payloads, followed by owned graph/node cleanup.
Its explicit loopback interface enumeration is test-only, not qualification of
default deployed interface discovery. See [Linux host requirements](docs/linux-only.md#ci).
UDP steering is an extension point: higher layers can provide
a `Steering` implementation through `NodeBuilder::with_steering`. Server development and profiling entrypoints are documented in
[the Bazel workflow](../../docs/build.md). Privileged traffic shaping remains
an optional manual diagnostic, not package preparation or acceptance.
