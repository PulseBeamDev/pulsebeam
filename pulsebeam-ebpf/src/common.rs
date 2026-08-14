//! Layout shared between the eBPF program and the userspace loader: map
//! sizing bounds and the diagnostic counter index table. No `aya-ebpf`
//! dependency here on purpose, so this module compiles unmodified on the
//! hosted target the loader builds for.

/// Capacity of `SOCKARRAY`. Bounds the number of shards a single node can
/// steer to; the loader must not populate an index `>= MAX_SHARDS`, and
/// `SHARD_COUNT` (the `.rodata` config global) must not exceed it either.
pub const MAX_SHARDS: u32 = 1024;

/// Capacity of `FLOWS`, the established-client flow-affinity map. LRU-backed
/// so a node under connection churn evicts oldest entries instead of
/// rejecting new ones.
pub const MAX_FLOWS: u32 = 131_072;

/// Diagnostic counter slots, one `u64` per outcome, exposed as
/// `COUNTERS[<index>]` (a `PerCpuArray`, summed by the loader across CPUs).
/// These are kernel-side diagnostics only — route epoch validation stays
/// authoritative in userspace, per `docs/routing.md`.
pub mod counters {
    /// STUN header failed to parse, or had no USERNAME attribute.
    pub const MALFORMED_STUN: u32 = 0;
    /// Envelope shorter than `ENVELOPE_LEN` or otherwise truncated.
    pub const MALFORMED_ENVELOPE: u32 = 1;
    /// USERNAME token was the wrong length or not valid Crockford base32.
    pub const INVALID_UFRAG: u32 = 2;
    /// Ufrag or Envelope version byte did not match the expected constant.
    pub const INVALID_VERSION: u32 = 3;
    /// Envelope `type` byte did not match a known `EnvelopeType`.
    pub const INVALID_TYPE: u32 = 4;
    /// Ufrag decoded to a `cluster_id` other than this node's `CLUSTER_ID`.
    pub const WRONG_CLUSTER: u32 = 5;
    /// Ufrag decoded to a `node_id` other than this node's `NODE_ID`.
    pub const WRONG_NODE: u32 = 6;
    /// Decoded shard was `>= SHARD_COUNT` (client) or the resolved route's
    /// shard came back out of bounds from a flow-affinity hit.
    pub const INVALID_SHARD: u32 = 7;
    /// Non-STUN datagram with no matching entry in `FLOWS`.
    pub const UNKNOWN_FLOW: u32 = 8;
    /// `FLOWS` hit, but the recorded shard is no longer valid (e.g. the node
    /// shrank `SHARD_COUNT` after the entry was installed).
    pub const STALE_ROUTE: u32 = 9;
    /// A packet was steered to a shard socket successfully.
    pub const SELECTED: u32 = 10;

    pub const COUNT: u32 = 11;
}
