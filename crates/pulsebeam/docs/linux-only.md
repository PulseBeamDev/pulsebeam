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

The OSS CI has no privileged kernel steering job. Root `just test-fast` covers
static checks and fast owner tests; `just test-slow` covers the slower browser
and simulation gates. Any proprietary steering implementation owns its own
build, verifier, capability, and attach/load tests outside this repository.
