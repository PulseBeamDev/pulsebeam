# Agent SDK Boundary

- `pulsebeam-agent-core` is SANS-I/O and owns platform-independent desired state, protocol and state-machine policy, and portable domain types. It emits effects and consumes completion events; it does not execute platform I/O.
- Web and native runtimes own execution and platform resources. They must not reimplement or override signaling, reconnection, topic, or other platform-independent policy; execute effects and feed results back to core.
- Types shared across runtimes are core-owned. Do not create browser- or native-specific copies to work around binding friction.
- Generated bindings are derived artifacts. Change their Rust/source definition or generator and use the owning package's generation recipe.
