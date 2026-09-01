# `pulsebeam-agent-web`

Browser/WASM host for [`pulsebeam-agent-core`](../pulsebeam-agent-core). It is
responsible for driving the core state machine with browser WebRTC, HTTP, data
channels, timers, scheduling, and application-facing events.

Applications provide already-acquired media tracks. Device capture, permission
flows, DOM binding, and a direct JavaScript SDK are outside this crate.

## Status

The adapter is under active development and is not yet a usable client SDK.
The accepted contract lives in
[`plans/agent-sdk`](../../plans/agent-sdk/spec.md).

## Development

`just --justfile agents/pulsebeam-agent-web/Justfile build` builds the WASM
package, and `serve` runs a local static server. Run the root `check` and `test`
gates before handing off browser-facing changes.
