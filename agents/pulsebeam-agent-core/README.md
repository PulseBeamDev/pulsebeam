# PulseBeam Agent Core

SANS-I/O PulseBeam client agent.

`agent-core` contains the platform-independent protocol and state machines shared by every environment. It performs no I/O itself.

## Owns

* Declarative client desired state
* Connection and reconnection state machines
* Signaling protocol
* Publication and subscription state
* Topic semantics
* Timers and retry decisions
* Effects describing required I/O
* Events representing completed I/O
* Snapshots and notifications
* Production diagnostics through the `log` facade
* Shared UniFFI-compatible domain types

## Does not own

* HTTP execution
* WebRTC implementation
* Timers or async runtime
* Media devices
* Threads or executors
* Browser or native platform APIs
* Logger installation, filtering, or output sinks

```text
desired state + events
        ↓
     agent-core
        ↓
 effects + snapshots
```

Environment runtimes execute effects and feed resulting events back into the core.

Core always emits UniFFI metadata for its portable records and enums so native
and web runtimes cannot compile against a reduced contract. Runtime crates
consume those types as external UniFFI types instead of defining platform
copies. TypeScript generation settings live in this crate's `uniffi.toml` so
core-owned byte arrays and strictness follow the core namespace.

## Verification

Run `cargo test -p pulsebeam-agent-core` while iterating. The repository gates
are `just check` and `just test` from the workspace root.
