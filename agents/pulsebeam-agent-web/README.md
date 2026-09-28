# PulseBeam Web

`@pulsebeam/web` is the browser-facing declarative PulseBeam agent package.

```ts
import { createAgent } from "@pulsebeam/web";

const agent = createAgent({
  endpoint: "https://pulsebeam.example",
  token,
  topology: {
    localAudios: 1,
    localVideos: 2,
    remoteAudios: 3,
    remoteVideos: 7,
  },
  logging: { level: "debug" },
});

agent.setState({
  connected: true,
  publications: [{ slot: "a0", label: "microphone", active: true }],
  video: [
    {
      slot: 0,
      trackId: "speaker-camera",
      height: 720,
      minHeight: 180,
      minFps: 15,
      priority: 100,
    },
  ],
  audio: { pinned: ["speaker-microphone"], automatic: true },
  topics: [{ name: "presence", mode: "latest", subscribe: true }],
});

const unsubscribe = agent.subscribe(() => render(agent.getSnapshot()));
const unsubscribeEvents = agent.subscribeEvents((event) => {
  if (event.type === "topic-message") consume(event.payload);
});
```

`createAgent()` is synchronous and safe to call while its private WASM module
is still initializing. The facade retains only the latest complete desired
state during initialization and then gives it to the browser runtime. Omitted
publication, video, audio-pinning, and topic collections are empty, retracting
their previous desired values.

The endpoint is the absolute HTTP(S) PulseBeam server endpoint; the core adds
`/api/v1/native`. The opaque token is sent only as bearer authorization. Local
counts reserve sender slots `v0`, `v1`, ... and `a0`, `a1`, ...; each direction
supports up to 32 media sections. A publication's label is bound to its sender
slot on first use and cannot be changed or reused for another slot of the same
kind during the Agent lifetime.

Use `replaceLocalTrack` and `setLocalMuted` for reserved local slots. The
runtime validates media kinds and sender settings. Omitted or empty encoding
settings enable the runtime's default three-layer video or single-layer audio
sender configuration; explicit video and audio settings contain three and one
encoding entries respectively. Sender settings are ignored when detaching with
a `null` track. Capture tracks remain owned by the caller: replacement and
`close()` detach them but never stop them.
Local-track operations are serialized per slot so an older replacement cannot
become the final attachment after a newer one.

Logging is configured independently for each agent with `logging.level`.
Messages use the browser console. The default level is `warn`. Chrome hides
`debug` and `trace` console messages unless Verbose output is enabled.

Snapshots contain participants and discoverable publications independently of
whether media is currently bound. Available remote `MediaStreamTrack` objects
are exposed in `snapshot.tracks`, keyed by publication ID. Snapshot records and
collections are immutable and retain identity until an observable update;
platform track objects themselves are not frozen.

Topics support `latest` and `ordered` registrations and sends. Event
subscriptions preserve message bytes plus publisher, stream, and sequence
metadata, and distinguish admission, drop, resynchronization, channel failure,
and agent failure events. Send admission is not a delivery acknowledgment.

`reconnect()` delegates to the runtime's reconnect operation and retains the
complete desired state. Explicit video and pinned-audio policies are per track;
server-directed receiver recreation restores a fresh default when needed. `close()` is
terminal and idempotent, detaches callbacks, aborts browser resources, and
fences initialization and pending operations. Unsubscribe functions are also
idempotent.

## Development

`just --justfile agents/pulsebeam-agent-web/Justfile check` checks the WASM
runtime and strict public TypeScript contract. `just --justfile
agents/pulsebeam-agent-web/Justfile test-fast` builds the package and runs the
deterministic Rust boundary tests. `just --justfile
agents/pulsebeam-agent-web/Justfile test-slow` verifies the pinned browser,
rebuilds the Web and React fixtures, and runs the browser contracts.
