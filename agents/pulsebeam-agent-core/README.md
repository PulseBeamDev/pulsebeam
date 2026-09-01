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
* Shared UniFFI-compatible domain types

## Does not own

* HTTP execution
* WebRTC implementation
* Timers or async runtime
* Media devices
* Threads or executors
* Browser or native platform APIs

```text
desired state + events
        ↓
     agent-core
        ↓
 effects + snapshots
```

Environment runtimes execute effects and feed resulting events back into the core.

## Verification

Run `cargo test -p pulsebeam-agent-core` while iterating. The repository gates
are `just check` and `just test` from the workspace root.
