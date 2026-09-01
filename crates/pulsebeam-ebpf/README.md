# `pulsebeam-ebpf`

Aya `SK_REUSEPORT` programs that steer client and inter-node UDP packets to the
owning PulseBeam shard. Packet classification and wire parsing come from
[`pulsebeam-routing`](../pulsebeam-routing/README.md) so userspace, simulation,
and the kernel cannot disagree.

The BPF target is `no_std`, allocator-free, and verifier constrained. Host tests
only prove shared classifier signatures; they do not prove that the program
loads or attaches.

Use `just --justfile crates/pulsebeam-ebpf/Justfile build` for the verifier
build and `smoke` for the privileged attach/load test. Changes to this crate or
shared routing contracts must pass the root `just ebpf` gate.
