# `pulsebeam-routing`

The single source of truth for STUN/ufrag parsing, the fixed inter-node
envelope, and shard steering. The same code is compiled into:

- the allocator-free Aya eBPF program;
- the Linux userspace demuxer;
- the deterministic simulator.

These consumers must never classify the same bytes differently.

## Constraints

Keep the crate `no_std`, heap-free, panic-free on malformed input, and legible
to the eBPF verifier. Loops need visible finite bounds; packet-derived offsets
need checked arithmetic and bounds-checked reads. Narrow lookup-table indexing
exceptions require a reasoned lint allowance.

Run `cargo test -p pulsebeam-routing` while iterating. Any change consumed by
the BPF program must also pass root `just ebpf`.
