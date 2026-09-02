# PulseBeam Agent Web

Browser runtime for the PulseBeam agent.

`agent-web` executes the SANS-I/O effects produced by `agent-core` using browser APIs. The handwritten `@pulsebeam/agent-web` TypeScript entry point is the package boundary; generated WASM bindings under `dist/wasm` are private.

## Public contract

`createAgent` asynchronously initializes the package and returns one framework-neutral `Agent`. The agent is directly consumable by React's `useSyncExternalStore`: `getSnapshot()` preserves object identity until observable state changes, and `subscribe()` returns an unsubscriber.

```ts
import { createAgent } from "@pulsebeam/agent-web";

const agent = await createAgent({
  endpoint: "https://sfu.example.com",
  roomId: "standup",
  requestHeaders: { Authorization: "Bearer token" },
  topology: {
    localVideo: ["camera", "screen"],
    localAudio: ["microphone"],
    remoteVideo: 4,
    remoteAudio: 2,
  },
});

await agent.setLocalTrack("camera", cameraTrack, "motion");
agent.setVideoDemand([
  { slot: 0, publicationId, height: 720, minHeight: 180, minFps: 15, priority: 2 },
]);
agent.setAudioDemand({ pinned: [speakerPublicationId], automatic: true });
agent.setPlayoutDelay({ mode: "fixed", minMs: 80, maxMs: 160 });

const chat = agent.openTopic({ name: "chat", mode: "ordered", publish: true, subscribe: true });
chat.send(new TextEncoder().encode("hello"));
agent.connect();

await agent.close();
```

Configuration and topology are copied, validated, and immutable after construction. Local slots accept track replacement, removal, and mute without changing topology. Video policies are `motion` and `detail`; audio policies are `speech` and `music`. Commands update one complete desired model, and the Rust core assigns its monotonic revisions.

Snapshots contain immutable participant, publication, local publication, remote media, binding, pause, audio-level, topic, connection, and typed failure values. Remote media is keyed by publication identity. Its `MediaStream` reference stays stable while physical receive slots and tracks change.

Topics use one handle per name and mode. A handle can publish, subscribe, or act as an async iterator; ordered iteration includes explicit resynchronization outcomes. Closing the handle unregisters it and completes pending iteration. `statistics()` returns bounded transport counters and sender encoding state without exposing the peer connection.

Callers own supplied `MediaStreamTrack` objects. Closing an agent detaches SDK references and browser resources but does not stop caller-owned capture tracks.

## Migration concepts

| Retired browser SDK concept | Agent replacement |
| --- | --- |
| `Participant` and event-emitter state | one async `Agent` with `getSnapshot()` and `subscribe()` |
| `main` / `aux` publishers | named immutable local topology slots plus `setLocalTrack()` |
| mutable remote track wrappers | immutable publication-keyed `remoteVideo` / `remoteAudio` snapshot values |
| per-track layout mutation | one complete `setVideoDemand()` command |
| mutable audio subscription state | `setAudioDemand()` with pins and automatic policy |
| nested topic builders | one `openTopic()` handle per name and mode |
| transport and adapter access | explicit commands and the narrow `statistics()` query |

There are no compatibility aliases for the retired participant, publisher, mutable-track, adapter, store, or topic-builder APIs.

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
