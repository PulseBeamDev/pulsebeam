//! Layout shared between the eBPF program and the userspace loader: map
//! sizing bounds and the diagnostic counter index table. No `aya-ebpf`
//! dependency here on purpose, so this module compiles unmodified on the
//! hosted target the loader builds for.

/// Capacity of `SOCKARRAY`. Bounds the number of shards a single node can
/// steer to; the loader must not populate an index `>= MAX_SHARDS`, and
/// `SHARD_COUNT` (the `.rodata` config global) must not exceed it either.
pub const MAX_SHARDS: u32 = pulsebeam_routing::steer::MAX_SHARDS;

/// Capacity of `FLOWS`, the established-client flow-affinity map. LRU-backed
/// so a node under connection churn evicts oldest entries instead of
/// rejecting new ones.
pub const MAX_FLOWS: u32 = pulsebeam_routing::steer::MAX_FLOWS;

/// Diagnostic counter slots, one `u64` per outcome, exposed as
/// `COUNTERS[<index>]` (a `PerCpuArray`, summed by the loader across CPUs).
/// These are kernel-side diagnostics only — route epoch validation stays
/// authoritative in userspace, per `docs/routing.md`.
pub use pulsebeam_routing::steer::counters;
