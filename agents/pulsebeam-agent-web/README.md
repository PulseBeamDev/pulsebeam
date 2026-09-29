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
      selector: { participantExternalId: "speaker", label: "camera" },
      height: 720,
      minHeight: 180,
      minFps: 15,
      priority: 100,
    },
  ],
  audio: {
    selected: [{ participantExternalId: "speaker", label: "microphone" }],
    automatic: true,
  },
  topics: [{ name: "presence", mode: "latest", subscribe: true }],
});

const unsubscribe = agent.subscribe(() => render(agent.getSnapshot()));
const unsubscribeEvents = agent.subscribeEvents((event) => {
  if (event.type === "topic-message") consume(event.payload);
});
```

For direct browser integration, local logical tracks reserve sender slots by
kind and label, independent of capture ownership. `connect()` and
`disconnect()` change desired connection state synchronously. A handle may
borrow a typed capture source without taking ownership of the native track:

```ts
import { createCaptureSource } from "@pulsebeam/web";

const stream = await navigator.mediaDevices.getUserMedia({ video: true });
const source = createCaptureSource(stream.getVideoTracks()[0], "video");
const camera = agent.localVideoTrack("camera");
camera.setSource(source);
agent.connect();
// Later: disconnect without ending capture, or detach without releasing the label.
agent.disconnect();
camera.setSource(null);
stream.getTracks().forEach((track) => track.stop());
agent.close();
```

`localAudioTrack(label)` reserves a separate audio namespace; repeated lookups
return the same handle. Capacity exhaustion throws `LocalTrackCapacityError`
with `kind`, `label`, and `capacity`, even after clearing a source. Another
Agent may borrow the same source. `@pulsebeam/react` owns capture and Agent
lifecycle for typical React applications.

`createAgent()` is synchronous and safe to call while its private WASM module
is still initializing. The facade retains only the latest complete desired
state during initialization and then gives it to the browser runtime. Omitted
publication, video, audio-pinning, and topic collections are empty, retracting
their previous desired values. Selectors resolve against the current Catalog;
missing tracks stay desired but are not sent in Intent until they appear. Low-level
callers can use Catalog TrackIds through `trackId` and `audio.pinned` instead.

The endpoint is the absolute HTTP(S) PulseBeam server endpoint; the core adds
`/api/v1/native`. The opaque token is sent only as bearer authorization. Local
counts reserve sender slots `v0`, `v1`, ... and `a0`, `a1`, ...; each direction
supports up to 32 media sections. A publication's label is bound to its sender
slot on first use and cannot be changed or reused for another slot of the same
kind during the Agent lifetime.

Low-level integrations can use `replaceLocalTrack` and `setLocalMuted` for
reserved local slots. The
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

The catalog-backed `agent.remoteVideoTracks` and `agent.remoteAudioTracks`
expose stable handles with external participant identity, media kind, and
application label. `agent.remoteAudio` is one Agent-owned aggregate playback
source. A remote video handle's `setReceiveOptions({ minHeight, minFps,
priority, playoutDelay })` replaces its policy, but does not request media
until a video element is attached. `attachRemoteVideo(handle, video)` observes
visible layout in physical pixels, shares the maximum demand across elements,
and detaches on `close()`. Hidden or off-screen elements do not reserve a
receiver, even with a minimum-height policy. Excess visible tracks remain
unmapped and warn rather than evict existing visible consumers or grow the
fixed topology. `attachRemoteAudio(agent.remoteAudio, audio)` plays only that
Agent's currently mapped audio and detaches on `close()`; connection alone
does not create playback. These helpers are the non-React counterpart of
`@pulsebeam/react`'s `<Video>` and `<Audio>` components.

Snapshots keep `catalog` (revision, participants, publications) separate from
`mapping` (accepted Intent revision and receiver-index bindings). Publications
remain discoverable independently of whether media is currently bound. Available
remote `MediaStreamTrack` objects are exposed in `snapshot.tracks`, keyed by
publication ID; offer-specific MIDs remain private to the host. Snapshot records
and collections are immutable and retain identity until an observable update;
platform track objects themselves are not frozen.

Typed topics register only when used:

```ts
const chat = agent.topic<{ text: string }>("chat", { mode: "reliable" });
await chat.publish({ text: "hello" });
const controller = new AbortController();
for await (const message of chat.subscribe({ signal: controller.signal })) {
  render(message.text);
}
```

`reliable` uses ordered delivery; `unreliable` uses latest-value delivery.
Each subscription has its own bounded queue; ending iteration or aborting
releases its interest. Messages use UTF-8 JSON and payloads over 64 KiB are
rejected before sending. Lower-level `latest`/`ordered` registrations and
sends remain available through `setState()`/`sendTopic()`. Event subscriptions
preserve message bytes plus publisher, stream, and sequence metadata, and
distinguish admission, drop, resynchronization, channel failure, and agent
failure events. Send admission is not a delivery acknowledgment.

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
