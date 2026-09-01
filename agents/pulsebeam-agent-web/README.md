# PulseBeam Agent Web

Browser runtime for the PulseBeam agent.

`agent-web` executes the SANS-I/O effects produced by `agent-core` using browser APIs.

## Owns

* Browser WebRTC integration
* Browser `MediaStreamTrack` integration
* HTTP execution
* Timer execution
* Browser callbacks and runtime plumbing
* WASM / JavaScript API
* Web-specific resource handles and conversions
* UniFFI bindings for the web environment

## Does not own

* Signaling policy
* Reconnection policy
* Topic protocol semantics
* Platform-independent agent behavior
* Framework integrations such as React

```text
JavaScript / Web Framework
          ↓
      agent-web
          ↓
      agent-core
```

## Development

`just --justfile agents/pulsebeam-agent-web/Justfile build` builds the WASM
package, and `serve` runs a local static server. Run the root `check` and `test`
gates before handing off browser-facing changes.
