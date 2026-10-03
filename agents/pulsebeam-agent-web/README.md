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
const camera = agent.local.video("camera");
camera.setSource(source);
agent.connect();
// Keep publication identity while sending black video.
camera.setEnabled(false);
camera.setEnabled(true);
// Later: disconnect without ending capture, or detach without releasing the label.
agent.disconnect();
camera.setSource(null);
stream.getTracks().forEach((track) => track.stop());
agent.close();
```

`agent.local.audio(label)` reserves a separate audio namespace; repeated lookups
return the same handle. Capacity exhaustion throws `LocalTrackCapacityError`
with `kind`, `label`, and `capacity`, even after clearing a source. Another
Agent may borrow the same source. Each runtime owns a clone for its sender,
so disabling one handle never disables another Agent or the original capture.
`setEnabled(false)` retains the source and publication, sending black video or
silent audio. It survives source replacement and detachment; `setSource(null)`
explicitly withdraws the publication. Handle publications remain managed even
when low-level `setState` replaces unrelated intent. `@pulsebeam/react` owns capture and Agent
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
The runtime serializes sender attachment per slot and discards superseded
queued replacements. Disabling and detaching silence owned clones without
waiting for a pending sender operation; replacement and close stop owned clones,
never the caller’s capture.

Logging is configured independently for each agent with `logging.level`.
Messages use the browser console. The default level is `warn`. Chrome hides
`debug` and `trace` console messages unless Verbose output is enabled.

The catalog-backed `agent.remote.videoTracks` and `agent.remote.audioTracks`
expose stable handles with external participant identity, media kind, and
application label. `agent.remote.participant(externalId)` returns a stable participant
even before discovery, with `video(label)` and `audio(label)` lookup and per-participant
collections. `agent.remote.participants` excludes self, as do all remote collections.
Lookup does not discover a track or request video. Handles survive unpublication,
departure/rejoin, publication-ID replacement, remapping, and reconnection.
Collections retain identity until membership changes.

A remote video handle's `setReceiveOptions({ minHeight, minFps, priority, playoutDelay })` replaces its policy, but does not request media
until a video element is attached. `attachRemoteVideo(handle, video)` observes
visible layout in physical pixels, shares the maximum demand across elements,
and detaches on `close()`. Hidden or off-screen elements do not reserve a
receiver, even with a minimum-height policy. Excess visible tracks remain
unmapped and warn rather than evict existing visible consumers or grow the
fixed topology. Video policy and mounted demand survive temporary Catalog absence.
Video attachment is the non-React counterpart of `@pulsebeam/react`'s `<Video>`.
Generic attachments accept only video elements and ignore audio tracks.

Remote audio is SDK-played automatically through one Web Audio route per mapped
receiver, without an application audio element, component, attachment, or analysis
read. A private detached, hidden, muted decoder element activates native browser
decoding where needed; it is never mounted or exposed and adds no audible route.
Connected Agents request automatic server-selected audio by default. Explicit
`audio.automatic: false` in `setState` opts out of automatic selection, not
playback of explicitly selected/pinned mapped audio. This deliberately replaces
the former attachment-driven automatic demand.

A logical audio handle's `receiving` is true exactly when Mapping binds its
current publication to an audio receiver. Catalog presence, track readiness,
silence, suspended browser audio, and analysis reads do not determine it.
Mapping changes notify track subscribers, including same-receiver reassignment.

`readWaveform(buffer)` synchronously fills normalized time-domain amplitudes;
`readSpectrum(buffer)` fills decibel bins in ascending frequency order.
Analysis uses AnalyserNode defaults: a 2048-point FFT and default spectral
smoothing. Smaller buffers receive leading samples/bins, not different FFT
resolution. Oversized tails are zero waveform or negative-infinity spectrum.
Unmapped/absent handles and unavailable signal clear the entire caller buffer
to those silence values. Empty buffers are valid. Pull reads do not publish
signal arrays or notify subscribers. Remapping resets analyser history.

`agent.remote.resumeAudio(): Promise<void>` is exceptional user-gesture recovery
for browsers blocking autoplay, not a normal playback prerequisite. It works
before media arrives, resolves only when the context is running, is safe to
repeat, and rejects on recovery failure or close. Automatic failure is logged
without terminating the Agent or changing Mapping. Applications call recovery
directly from a gesture; the SDK adds no global gesture listeners.

Disconnect removes routes and stale analysis; reconnect restores current
receivers without duplicate output. Agent-owned contexts are isolated and close
is terminal/idempotent. Existing handles become inactive/nonreceiving and read
silence after close; new remote lookups fail. Borrowed local capture is never
routed to the speakers or stopped by this feature.

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

`./bazel build //:web` builds the package, WASM runtime, and bindings in graph order.
`./bazel test //agents/pulsebeam-agent-web:public_contract //agents/pulsebeam-agent-web:uniffi_types //agents/pulsebeam-agent-web:uniffi_contract`
checks public TypeScript and generated contracts. `./bazel test
//agents/pulsebeam-agent-web:unit_tests` runs the deterministic Rust boundary tests.
`./bazel test //agents/pulsebeam-agent-web:browser` provisions the pinned browser,
builds declared Web/React fixtures, and runs the browser contracts.
`./bazel run //:web_dev` serves the Web example. See [editor setup](../../docs/ide.md).
