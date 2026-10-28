# PulseBeam server: Linux-only

The PulseBeam server currently targets Linux. Portable protocol, core, routing,
runtime, and simulator crates can build on other platforms, but the server crate
has an explicit compile-time Linux gate.

## Compile-time gate

The gate lives in `pulsebeam/src/lib.rs` before the server modules:

```rust
#[cfg(not(target_os = "linux"))]
compile_error!(
    "pulsebeam server currently requires Linux. Portable crates (protocol, core, \
     simulator) build elsewhere; the server binary does not."
);
```

Keeping the gate near the crate root makes unsupported server builds fail
immediately instead of partway through platform-specific code.

## UDP steering

PulseBeam OSS owns the generic steering contract, not a specific kernel
implementation. `NodeBuilder::with_steering` accepts an extension factory after
the server has bound its UDP sockets, and the extension returns a
`Box<dyn Steering>`.

The default OSS server runs without an injected steering extension and retains
the userspace cross-shard forwarding path. A higher layer may provide a
Linux-specific implementation, including an eBPF-based one, without changing
the OSS server or routing contracts.

The deterministic simulator provides its own steering implementation so tests
can exercise the same ownership and flow-pinning behavior without kernel
dependencies.

## CI

Complete acceptance requires a Linux host with working IPv4 and IPv6 loopback,
a dual-stack UDP listener, `UDP_SEGMENT` and `UDP_GRO` support. IPv6 must be
enabled on `lo`; global/routed IPv6, hardware NIC offload and privileged
network setup are not required. If a required capability is unavailable, use
an appropriately configured host/kernel. The tests report unsupported capability
as a failure and never change sysctls or silently skip it.

Root `./bazel test //:check` includes
`//crates/pulsebeam-runtime:transport_conformance` for actual packet/offload
contracts and `//crates/pulsebeam:transport_conformance` for one concurrent
dual-family SDK/SFU witness. The latter supplies explicit loopback interfaces
but retains real native OS sockets. It does not certify stock deployed
interface discovery or global IPv6. Connection/family evidence, recipient
payloads and cleanup have bounded event-driven operation deadlines of 30, 30
and 10 seconds; these are infrastructure limits, not application SLAs.

`//:fast` retains fallback/component/simulation proof and the distinct stock
production-native media witness. `//:slow` covers browser and broader simulation
gates; `//:test` includes all three routes. The OSS CI has no privileged kernel
steering job. Any proprietary steering implementation owns its own build,
verifier, capability and attach/load tests outside this repository.
