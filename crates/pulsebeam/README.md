# `pulsebeam`

The PulseBeam WebRTC SFU server. The control plane owns topology and compiles
owned routing updates; logical shards own participants, transports,
packet buffers, routes, and forwarding state outright.

Read these contracts before changing the corresponding subsystem:

- [Naming boundaries](docs/naming.md)
- [Shard-owned movable dataplane](docs/shard-owned-dataplane.md)
- [Routing and compiled plans](docs/routing.md)
- [Cluster topology and ownership (proposed)](docs/cluster.md)
- [Linux server requirements](docs/linux-only.md)
- [Architecture diagram](docs/architecture.svg)

Shared mutable packet state, cross-shard handles, blocking calls, and
multi-atomic snapshots are architectural regressions. Cross-shard and
cross-node coordination uses owned messages.

## Runtime profiles

`--rt adaptive` is the process default. Each dataplane worker has a current-thread
runtime and normally hosts several logical shards. The server defaults to
16 shards per worker; `--shards` changes that factor. The library default remains
one worker and one shard. `NodeBuilder::adaptive`, `runtime_profile`,
`work_stealing`, `thread_per_core`, `data_threads` and `workers` are explicit
configuration operations backed by `pulsebeam-runtime`.

Restart with `--rt work-stealing` to use the dedicated multithreaded scheduler,
or `--rt thread-per-core` for one shard per dedicated worker. The latter defaults
to one shard per worker and rejects an explicit incompatible `--shards` value.
Profiles cannot change in a running process. Control and optional TCP roles
remain separate from dataplane execution.

The node controller owns initial placement and conservative continuous balancing.
Its placement module reads the existing room registry and runtime observations,
without a second placement registry. Existing room partitions keep their logical
shard. New partitions prefer measured worker occupancy, with participant counts
as a fallback or tie-breaker, not an assumption of equal media cost. Explicit
round-robin placement remains available for simulations.
Adaptive balancing uses elapsed-time-normalized shard busy time across five
two-second windows. A candidate must remain strongly beneficial for ten seconds,
including pessimistic destination headroom and a migration reserve. At most one
whole-shard move is outstanding. Every attempt is followed by a thirty-second
observation-only hold and entirely fresh measurement history. These thresholds
are conservative heuristics, not measured migration-cost or latency guarantees.
Runtime-owned handoff preserves the same pinned service
allocation, UDP descriptor and queued input, logical identity and TCP inbox.
Every bounded shard tick returns to the executor, including under continuous
readiness. Registration failures roll back once; unsafe restoration shuts down
the node rather than rebuilding or abandoning a shard.

Simulation retains `with_current_runtime()` with synthetic thread-local sockets
and virtual time, without launching native dataplane threads. Cross-reactor
migration requires Linux. The default cutover is qualified by correctness;
comparative latency and efficiency have not been measured.

## Native signaling ownership

The controller owns each connection's native session, resource declarations,
source resolution, authorization, and signaling projection. It compiles transport
bindings and per-stream policy before sending bounded execution commands to the
shard. Native participants use the controller's canonical room topology rather
than an endpoint visibility catalog or default room-wide subscriptions.

The shard owns applied bindings, eligible source descriptors, actual routes, and
adaptive allocation. Layer selection, bandwidth filtering, demand, and physical
handover remain local. Route activation and retirement reflect actual active,
staging, draining, and desired streams, not merely a new controller assignment.
RTC remains the sole owner of installed transport state, physical identity reuse,
and cancellable data-channel progression.

Only the designated native signaling channel crosses into control. Application
media and topic payloads retain their direct dataplane path. Per-connection
handoff is finite and nonblocking, with native input separated from required
lifecycle and progress facts. Capacity failure terminates the affected connection
rather than blocking the shard or silently discarding accepted signaling.

The opaque shard writer owns at most one pending output batch. Handoff does not
advance the native cursor: the controller commits row versions only after actual,
timestamped RTC send admission. Delayed observation does not restart the original
progress deadline. Pending materialization and full shard destinations are
serviced independently so one blocked connection does not stop unrelated control
work. These are ownership and progress contracts, not a throughput guarantee.

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

TCP frames do not pass through the controller. First-STUN ufrags are
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

UDP sockets use [`pulsebeam-udp`](../pulsebeam-udp/README.md). Advertised ICE
candidates belong to the node, not to physical socket identity or destination
metadata. The host accepts the full legal UDP payload range and copies compatible
logical datagrams into bounded owned batches for shard handoff, preserving source,
destination and arrival identity without charging the shard budget per GRO segment.
The host accounts only the exact submitted prefix of a UDP batch;
unsubmitted irrevocable RTC output is discarded as local loss, never retried or
credited as a UDP submission. RTC remains decoupled and assumes its emitted
output will be sent; there is no socket receipt or rollback API into RTC.

Run focused crate tests while iterating. Before handoff, run root `./bazel test //:check`
and `./bazel test //:test`. The production-configured owner target
`//crates/pulsebeam:transport_conformance` reuses two concurrent threaded native
SDK peers per UDP/TCP witness. It requires actual IPv4/IPv6 HTTP candidate
advertisements, selected-pair statistics, completed adaptive moves and distinct
bidirectional application topic payloads before, during and after handoff,
followed by owned graph/node cleanup. UDP descriptors remain once-bound.
Its explicit loopback interface enumeration is test-only, not qualification of
default deployed interface discovery. See [Linux host requirements](docs/linux-only.md#ci).
UDP steering is an extension point: higher layers can provide
a `Steering` implementation through `NodeBuilder::with_steering`. Server development and profiling entrypoints are documented in
[the Bazel workflow](../../docs/build.md). Privileged traffic shaping remains
an optional manual diagnostic, not package preparation or acceptance.
