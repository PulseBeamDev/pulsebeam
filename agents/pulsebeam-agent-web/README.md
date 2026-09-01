# PulseBeam Agent Web

Browser runtime for the PulseBeam agent.

`agent-web` executes the SANS-I/O effects produced by `agent-core` using browser APIs. The handwritten `@pulsebeam/agent-web` TypeScript entry point is the package boundary; generated WASM bindings under `dist/wasm` are private.

## Owns

* Browser WebRTC integration
* Browser `MediaStreamTrack` integration
* HTTP execution
* Timer execution
* Browser callbacks and runtime plumbing
* WASM / JavaScript API
* Web-specific resource handles and conversions
* A cached immutable browser snapshot and serial event delivery
* Production diagnostics through the core `log` facade and browser runtime logs

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

Install package tooling with `pnpm install` in this directory. Browser tests use `thirtyfour` exclusively through WebDriver BiDi. The focused recipes are:

* `just --justfile agents/pulsebeam-agent-web/Justfile check` checks WASM and TypeScript;
* `just --justfile agents/pulsebeam-agent-web/Justfile test` builds the package and runs unit plus deterministic BiDi browser host-boundary tests;
* `just --justfile agents/pulsebeam-agent-web/Justfile server-test` runs the real-browser BiDi vertical slice against a repository server already listening at `127.0.0.1:7070`.

The driver manager discovers an installed Chrome by default. Set `PULSEBEAM_BROWSER_BINARY` when the browser executable lives outside the standard installation paths.

`configureLogging` installs one process-wide level and optional sink. Logs include correlation identifiers where useful and never include request authorization values, SDP bodies, topic payloads, or media bytes.

Run the root `just check` and `just test` gates before handing off browser-facing changes.
