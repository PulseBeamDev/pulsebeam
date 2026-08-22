# pulsebeam-agent-web

`pulsebeam-agent-web` is the standalone browser/WASM adapter for
`pulsebeam-agent-core`. Browser bindings execute peer-connection, data
channel, media, fetch, timer, and encoded-transform effects. Callback values
carry their `TransportGeneration`, so stale browser events can be rejected by
the participant boundary.

The Rust core retains lifecycle, signaling reduction, intents, allocation,
topics, presets, E2EE framing, and generation decisions. JavaScript is used
only as browser interop; it is not a second protocol implementation.

Host-side mocked tests are useful for deterministic API conformance:

```text
cargo check -p pulsebeam-agent-web --all-targets
cargo test -p pulsebeam-agent-web --all-targets
cargo test -p pulsebeam-agent-web --test conformance
```

WASM and browser checks are exposed by `make agent-wasm` and the CI browser
fixture. Browser artifacts are kept in Cargo's `target/` directory.
