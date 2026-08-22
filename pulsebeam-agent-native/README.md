# pulsebeam-agent-native

`pulsebeam-agent-native` is the standalone native adapter for
`pulsebeam-agent-core`. It owns Tokio scheduling, native transport I/O,
`str0m`, RTP routing/recovery, media pipelines, timers, and bounded mailboxes.
It drives one `AgentCore`; shared lifecycle, signaling, allocation, topic, and
E2EE decisions remain in the core crate.

The native adapter is intentionally transport-neutral at its public boundary:
core effects are executed by the driver and value-owned core events are
returned to callers. The in-memory transport and media fixtures make the
adapter contract testable without network services.

```text
cargo check -p pulsebeam-agent-native
cargo test -p pulsebeam-agent-native --all-targets
cargo test -p pulsebeam-agent-native --test conformance
```
