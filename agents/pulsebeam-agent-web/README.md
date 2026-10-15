# PulseBeam Web

Connect a browser application to a PulseBeam room with `@pulsebeam/web`.
Publish camera and microphone tracks, play another participant's media, and
exchange live messages without owning WebRTC signaling or reconnection.

For a React application, use the [React SDK quickstart](../../docs/react/quickstart.md)
instead. It uses short, framework-independent snippets and owns capture
and session cleanup through hooks. The complete app is in
[PulseBeam Meet](../../apps/meet). The direct browser API below is for applications managing their own UI.

## Install and configure your browser app

```sh
npm install @pulsebeam/web
```

Before joining, you need:

- A compatible PulseBeam server's absolute HTTP(S) **base URL**, without
  `/api/v1/native`, and an unexpired participant token from your backend.
  Keep signing secrets out of the browser. If you need a local server and tokens,
  follow the [development server setup](../../docs/react/quickstart.md#1-run-the-pulsebeam-media-server).
- A browser-rendered app on **HTTPS or localhost**, with WebRTC, WebAssembly,
  Web Audio, and camera/microphone access. An HTTPS page cannot use an insecure
  remote HTTP endpoint. Start capture from a user action and handle permission errors.
- A bundler that serves the package's `pulsebeam_agent_web_bg.wasm` at its loader's
  module-resolved URL. The npm package includes this separate WASM asset; installing
  JavaScript alone does not guarantee that your bundler emits it correctly.

For Vite 5, add this to `vite.config.mjs` to avoid moving the loader away from its
WASM asset during development:

```js
export default {
  optimizeDeps: { exclude: ["@pulsebeam/web"] },
};
```

For another bundler or a WASM loading failure, follow the
[SDK initialization diagnostics](../../docs/react/troubleshooting.md#sdk-initialization).
Initialize the SDK only in the browser, not during server-side rendering.

## Join, publish, and receive

This TypeScript helper integrates with your **existing browser UI**; token fetching
and the UI itself are caller-owned. Call it from your Join handler with the endpoint,
token, other participant's external ID, a mounted visible `<video>` element, and
a callback that displays snapshot connection/failure state. Reserve one outgoing
and one incoming track of each media kind for this two-participant example.

```ts
import {
  attachRemoteVideo,
  createAgent,
  createCaptureSource,
  type AgentSnapshot,
  type RemoteMediaAttachment,
} from "@pulsebeam/web";

export async function joinRoom(
  endpoint: string,
  token: string,
  peerId: string,
  remoteVideo: HTMLVideoElement,
  onSnapshot: (snapshot: AgentSnapshot) => void,
) {
  const agent = createAgent({
    endpoint,
    token,
    topology: {
      localAudios: 1,
      localVideos: 1,
      remoteAudios: 1,
      remoteVideos: 1,
    },
  });
  let stream: MediaStream | undefined;
  let attachment: RemoteMediaAttachment | undefined;
  let unsubscribe = () => {};
  const leave = () => {
    unsubscribe();
    attachment?.close();
    agent.close();
    stream?.getTracks().forEach((track) => track.stop());
  };

  try {
    stream = await navigator.mediaDevices.getUserMedia({ audio: true, video: true });
    agent.local.audio("microphone").setSource(
      createCaptureSource(stream.getAudioTracks()[0]!, "audio"),
    );
    agent.local.video("camera").setSource(
      createCaptureSource(stream.getVideoTracks()[0]!, "video"),
    );
    remoteVideo.autoplay = true;
    remoteVideo.playsInline = true;
    attachment = attachRemoteVideo(
      agent.remote.participant(peerId).video("camera"),
      remoteVideo,
    );
    const update = () => onSnapshot(agent.getSnapshot());
    unsubscribe = agent.subscribe(update);
    update();
    agent.connect();
    return {
      agent,
      leave,
      resumeAudio: () => agent.remote.resumeAudio(),
      retryVideo: () => attachment!.retryPlayback(),
    };
  } catch (error) {
    leave();
    throw error;
  }
}
```

Try your UI in two tabs with fresh tokens for **the same room and different
participant IDs**. In Alice's tab pass `peerId: "bob"`; in Bob's pass `"alice"`.
Both use publication label `camera`. Give the remote video element nonzero visible
size, for example `width: 320px; max-width: 100%`. Disable repeated joins while
awaiting the helper; display capture errors and allow a new attempt.

**Success:** both snapshots report `connected`, and waving in one tab changes
the other tab's remote video. Remote microphone audio plays automatically without
an audio element. Use headphones to avoid feedback. If the browser blocks playback,
wire user-gesture buttons directly to `resumeAudio()` or `retryVideo()` and report
rejections. For a missing track, check the peer ID, publication label, and video visibility.

Call the returned `leave()` on Leave and application teardown. It closes the
attachment, unsubscribes, closes the Agent, and stops this helper's captured tracks.
Disconnecting or closing an Agent alone never stops caller-owned capture.
For longer sessions, [renew authorization before expiry](../../docs/react/guides.md#connection-renewal-and-errors)
with `agent.renewAuthorization(token)`. A different room or participant needs a new Agent.

## Local tracks and capacity

`agent.local.video(label)` and `agent.local.audio(label)` reserve separate sender
namespaces; repeated lookups return the same handle. A handle borrows a typed
capture source without taking ownership of its native track. Each runtime owns
a sender clone, so disabling one handle does not disable another Agent or the
original capture.

`setEnabled(false)` retains the source and publication, sending black video or
silent audio. It survives source replacement and detachment. `setSource(null)`
withdraws the publication but does not release the reserved label or stop capture.
Capacity exhaustion throws `LocalTrackCapacityError` with `kind`, `label`, and
`capacity`, even after a source is cleared.

Topology reserves sender and receiver media sections. Omitted counts default to
2 local videos, 2 local audios, 16 remote videos, and 8 remote audios. Set explicit
counts for your application. Labels bind to sender slots on first use and cannot
be changed or reused for another slot of the same kind during the Agent lifetime.

## Remote tracks and playback

`agent.remote.participant(externalId)` returns a stable participant handle even
before discovery. Its `video(label)` and `audio(label)` handles survive unpublication,
departure/rejoin, publication replacement, remapping, and reconnection. Remote
collections exclude self and retain identity until membership changes. Lookup
alone neither discovers a track nor requests video.

`attachRemoteVideo(handle, element)` requests video according to visible layout
in physical pixels and shares maximum demand across elements. Hidden or off-screen
elements do not reserve a receiver, even with `setReceiveOptions({ minHeight })`.
Excess visible tracks remain unmapped and warn rather than evicting consumers or
growing topology. `setReceiveOptions({ minHeight, minFps, priority, playoutDelay })`
replaces the handle's policy. Policy and mounted demand survive temporary Catalog
absence. Close attachments when removing elements; `agent.close()` also detaches them.

Remote audio uses one SDK-owned Web Audio route per mapped receiver. Connected
Agents request automatic server-selected audio by default. Low-level
`audio.automatic: false` opts out of automatic selection, not playback of explicitly
selected or pinned audio. `resumeAudio()` is exceptional recovery for blocked
autoplay, not a normal prerequisite. Call it directly from a user gesture; the SDK
adds no global gesture listeners. It rejects on recovery failure or close.

An audio handle's `receiving` reflects Mapping, not Catalog presence or audible
signal. `readWaveform(buffer)` fills normalized time-domain amplitudes;
`readSpectrum(buffer)` fills decibel bins. Analysis uses the default 2048-point
FFT and smoothing. Smaller buffers receive leading samples/bins; oversized tails
and unavailable signals are zero waveform or negative-infinity spectrum. Reads
do not notify subscribers. Remapping resets analysis; close makes existing
handles inactive and reads silent. New remote lookups after close fail.

## Live messages

Typed topics register when used. Given the connected `agent` returned above:

```ts
const chat = agent.topic<{ text: string }>("chat", { mode: "reliable" });
const controller = new AbortController();
const messages = (async () => {
  for await (const message of chat.subscribe({ signal: controller.signal })) {
    console.log(message.text);
  }
})();
const stopMessages = async () => {
  controller.abort();
  await messages;
};
try {
  await chat.publish({ text: "hello" });
} catch (error) {
  await stopMessages();
  throw error;
}
```

Call `stopMessages()` on application teardown. Use the same topic name on both participants. `reliable` uses ordered delivery;
`unreliable` uses latest-value delivery. Names are 1 to 85 ASCII characters in
`[A-Za-z0-9_-]`. Messages use UTF-8 JSON, capped at 65,536 bytes before sending.
Each subscription queues at most 256 values; ordered overflow drops the oldest,
and latest mode coalesces queued values. Ending iteration or aborting releases
subscription interest. Send admission is not a delivery acknowledgment.

## Low-level state and runtime behavior

Prefer local/remote handles and typed topics above. `setState()` replaces complete
low-level intent: omitted publication, video, audio-pinning, and topic collections
are empty and retract previous values. Handle-managed publications remain managed
when unrelated intent changes. Selectors resolve against the current Catalog;
missing tracks remain desired until discovered. `trackId` and `audio.pinned`
accept Catalog track IDs. Low-level topics use `latest`/`ordered` registrations
and `sendTopic()`. Event subscriptions expose message bytes and publisher/stream/
sequence metadata plus admission, drop, resynchronization, and failure events.

Snapshots separate `catalog` (discovery) from `mapping` (accepted Intent revision
and receiver bindings). `snapshot.tracks` exposes available remote platform tracks
by publication ID. Records and collections are immutable and stable until observable
updates; the platform track objects are not frozen. See the
[public types](web/types.ts) for exact fields.

`replaceLocalTrack` and `setLocalMuted` operate on reserved local slots. Default
sender settings use three video encodings or one audio encoding; explicit settings
require those same counts. Detaching with `null` ignores sender settings. Sender
attachment is serialized per slot and superseded queued replacements are discarded.
Disabling or detaching silences owned clones immediately. Replacement and close
stop clones, never caller-owned capture.

`createAgent()` is synchronous while WASM initializes asynchronously. Only the
latest complete desired state is retained during initialization. `connect()` and
`disconnect()` change desired state synchronously; use snapshots to observe completion.
`reconnect()` retains desired state. `close()` is terminal/idempotent and fences
initialization and pending operations. Unsubscribe functions are also idempotent.
Disconnect removes audio routes and stale analysis; reconnect restores current
receivers without duplicate output.

Logging is per Agent via `logging.level`, defaults to `warn`, and uses the browser
console. Enable Chrome's Verbose console output to see `debug` and `trace` messages.

## Contributor development

Run from a build-ready repository root:

```sh
./bazel build //:web
./bazel test //agents/pulsebeam-agent-web:public_contract //agents/pulsebeam-agent-web:uniffi_types
./bazel test //agents/pulsebeam-agent-web:unit_tests //agents/pulsebeam-agent-web:js_unit_tests
./bazel test //agents/pulsebeam-agent-web:web_runtime_browser
```

Direct JavaScript tests compile only the topic and catalog facade boundary with
the pinned toolchain. Typed public consumers reject untyped fields and forbidden
APIs in generated contracts. The Web browser owner covers WASM/runtime
ownership, real layout and media, autoplay recovery, and reliable-topic recovery.
Its live nodes own ephemeral HTTP and RTC listeners. Metrics startup remains
in the production-executable quickstart boundary.

`//agents/pulsebeam-agent-web:browser` selects all Web, React, Meet, and documented
quickstart browser owners without duplicating their execution. Each owner builds
only its required fixtures. See the
[contributor testing contract](../../docs/build.md#testing-contract) for shared
routing and [editor setup](../../docs/ide.md). `./bazel run //:web_dev` serves the
repository Web example, not an installed npm consumer.
