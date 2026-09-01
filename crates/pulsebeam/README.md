# `pulsebeam`

The PulseBeam WebRTC SFU server. The control plane owns topology and compiles
owned routing updates; thread-per-core shards own participants, transports,
packet buffers, routes, and forwarding state outright.

Read these contracts before changing the corresponding subsystem:

- [Thread-per-core ownership](docs/thread-per-core.md)
- [Routing and compiled plans](docs/routing.md)
- [Linux and eBPF requirements](docs/linux-only.md)
- [Architecture diagram](docs/architecture.svg)

Shared mutable packet state, cross-shard handles, blocking calls, and
multi-atomic snapshots are architectural regressions. Cross-shard and
cross-node coordination uses owned messages.

Run focused crate tests while iterating. Before handoff, run root `just check`
and `just test`; use root `just ebpf` when that boundary is touched. Server
development, profiling, traffic shaping, and cleanup helpers live in this
crate's `Justfile`.
