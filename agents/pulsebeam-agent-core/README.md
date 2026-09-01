# `pulsebeam-agent-core`

SANS-I/O state machine for the new PulseBeam client SDK. It owns desired and
observed connection state and emits owned effects for HTTP, WebRTC, data
channels, and timers; a host adapter performs those effects and returns events.

This crate is `no_std`, synchronous, and intentionally unaware of browsers,
async runtimes, sockets, and DOM objects. Generation-tagged events are the
boundary that prevents stale host completions from changing a newer session.

## Status

The state machine is under active development and does not yet implement the
complete contract in [`plans/agent-sdk`](../../plans/agent-sdk/spec.md). Treat
the public surface as unstable until that project is complete.

## Verification

Run `cargo test -p pulsebeam-agent-core` while iterating. The repository gates
are `just check` and `just test` from the workspace root.
