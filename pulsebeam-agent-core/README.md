# pulsebeam-agent-core

`pulsebeam-agent-core` is the transport-neutral, SANS-IO client contract for
PulseBeam agents. `AgentCore` owns lifecycle state, transport generations,
FIFO effects, reconnect deadlines, and value-owned inputs/events. Runtime
adapters execute `CoreEffect` values and feed resulting `CoreInput` values
back into the core.

The crate has no Tokio, socket, browser, or ambient-clock dependency. Native
and browser adapters are separate workspace crates and must not move lifecycle
or protocol decisions out of the core.

Useful checks from the repository root:

```text
cargo check -p pulsebeam-agent-core
cargo test -p pulsebeam-agent-core
cargo test -p pulsebeam-agent-core --test conformance
```

The WASM check is also exposed by `make agent-wasm`. It requires a Rust
`wasm32-unknown-unknown` target and the native build tools needed by the
workspace's crypto dependency.
