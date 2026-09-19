# `pulsebeam-routing`

The single source of truth for STUN/ufrag parsing, the fixed inter-node
envelope, and shard steering. The same routing contract is consumed by the Linux userspace demuxer and the
deterministic simulator. Higher-level steering extensions may consume it too.

These consumers must never classify the same bytes differently.

## Constraints

Keep the crate `no_std`, heap-free, and panic-free on malformed input.
Packet-derived offsets need checked arithmetic and bounds-checked reads. Narrow
lookup-table indexing exceptions require a reasoned lint allowance.

Run `cargo test -p pulsebeam-routing` while iterating. Higher-level steering
extensions are responsible for validating any additional platform-specific
constraints they impose.
