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
`createAgent(config)` is the one public caller-owned Agent construction API in
`@pulsebeam/web`; close it on application teardown. React applications instead
use `useAgent` from `@pulsebeam/react`, which does not export `createAgent`.
Managed activation/release is an internal package bridge, not a public Web
constructor or lifecycle contract.

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

`agent.remote.participant(participantId)` returns a stable participant handle even
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

Use `ordered` for chat, `lossy` for transient events, and `latest` for replaceable
state. Every topic requires an explicit mode. Given the connected Agent above,
this helper takes callbacks from your existing UI:

```ts
import type { Agent } from "@pulsebeam/web";

export function liveChat(
  agent: Agent,
  appendMessage: (message: {
    key: string;
    participantId: string;
    text: string;
  }) => void,
  showNotice: (message: string) => void,
) {
  const chat = agent.topic<{ text: string }>("chat", { mode: "ordered" });
  const controller = new AbortController();
  const messages = (async () => {
    for await (const event of chat.subscribe({ signal: controller.signal })) {
      if (event.type === "gap") {
        showNotice(
          `Chat skipped sequences [${event.expectedSequence}, ${event.availableSequence}) from ${event.participantId}.`,
        );
      } else {
        appendMessage({
          key: `${event.dataTrackId}:${event.sequence}`,
          participantId: event.participantId,
          text: event.value.text,
        });
      }
    }
  })().catch((error: unknown) => showNotice(String(error)));

  return {
    async send(text: string) {
      try {
        await chat.publish({ text });
      } catch (error) {
        showNotice(`Message not accepted: ${String(error)}`);
      }
    },
    async stop() {
      controller.abort();
      await messages;
    },
  };
}
```

Wire `send(text)` to your send control and call `stop()` on teardown. Join two
participants with the same topic name and mode: a received row identifies its
server-authored sender. Sending does not loop back to the publisher. Local echo,
if wanted, is application-owned and is not evidence of remote delivery.

| Mode | Delivery within one publisher data track |
| --- | --- |
| `latest` | Replaceable state; older sequences are suppressed and queued values may coalesce for that track only |
| `lossy` | Distinct best-effort events; no intentional coalescing, ordering, or recovery |
| `ordered` | Increasing messages with finite recovery; an inline gap precedes continuation when a known range is skipped |

Every mode suppresses duplicate `(dataTrackId, sequence)` identities before they
reach the application. Lossy delivery retains a finite reordering window and may
discard a unique event that arrives after that window has advanced. There is no
global order across publishers, initial state snapshot, offline replay, or durable
history. New ordered consumers start at a live frontier, not at sequence zero. Reopening or reconnecting creates a fresh `dataTrackId` and sequence space;
recovery does not span those publication lifetimes.

Each yielded `TopicEvent<T>` is a tagged `message` or `gap`. Both carry
`dataTrackId`, external `participantId`, and `topic`. Messages add `sequence`
and `value`; gaps describe `[expectedSequence, availableSequence)`. Sequence
fields are lossless `bigint`, not JavaScript numbers. Treat IDs as opaque and use
`(dataTrackId, sequence)` for received-message identity, never a sender field
inside untrusted JSON.

Names are 1 to 85 ASCII characters in `[A-Za-z0-9_-]`. UTF-8 JSON payloads are
capped at 65,536 bytes, excluding the envelope. Generic types do not validate
received JSON. Each iterator queues at most 256 events. Latest coalesces per track;
lossy can discard buffered events under pressure. Ordered overflow retains explicit
skipped ranges before later messages. Latest/ordered consumers fail explicitly if
their per-track state cannot fit safely, rather than silently losing continuity.
Ending iteration or aborting releases only that subscription.

`publish()` resolves on **local transmission acceptance**, not remote delivery,
persistence, or an application acknowledgment. Initial authoritative binding waits
at most 10 seconds. Invalid payloads, unavailable publications, capacity exhaustion,
sequence exhaustion, and replacement of a selected generation/binding reject;
pending old-publication work is not retargeted to a replacement. Queue pressure,
latest supersession, or reconnect may still discard an accepted value.
[Exact topic contract and coordinated upgrade](../../docs/react/reference.md#topics).

## Low-level state and runtime behavior

Prefer local/remote handles and typed topics above. `setState()` replaces complete
low-level intent: omitted publication, video, audio-pinning, and topic collections
are empty and retract previous values. Handle-managed publications remain managed
when unrelated intent changes. Selectors resolve against the current Catalog;
missing tracks remain desired until discovered. `trackId` and `audio.pinned`
accept Catalog track IDs. Low-level topics use explicit `latest`, `lossy`, or
`ordered` registrations and `sendTopic()`. Raw events expose message bytes,
`dataTrackId`, external `participantId`, lossless sequences, recovery gaps,
admission/drop outcomes, and genuine channel failures. Prefer typed inline events
for application messages.

Normal public `participantId` and `roomId` are authoritative application identities,
not protocol-internal canonical IDs. There are no `participantExternalId` or
`roomExternalId` aliases. Remote participant handles likewise expose `participantId`.

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
./bazel test //agents/pulsebeam-agent-web:node_runtime_tests
./bazel test //agents/pulsebeam-agent-web:web_runtime_browser
```

Direct JavaScript tests execute production topic, catalog, immutable-state and
local-handle modules without a DOM. Typed public consumers check package and
generated contracts. Deterministic native/SFU simulation owns three-mode delivery,
Catalog identity, publication replacement, ordered loss/recovery/gaps, and
live late-subscription proof.

Pinned-Node artifact tests instantiate emitted WASM and exercise supported
lifecycle, sender ownership, cancellation, handle-policy paths, and lossless topic
sequences/gaps. Controlled peer outcomes exercise binding/admission and generation
fences, not real WebRTC or application support for Node.

The required Web browser owner retains only real-platform proof: shipped SDK
media through a local SFU, generated native-track handoff, decoded receiver
replacement, layout/observer/device-pixel plumbing, isolated early/late
trusted-gesture premises and an adapter round trip after an owned node restart.
It builds no Meet export or React acceptance fixture. Live nodes own ephemeral
HTTP and RTC listeners. See the [client QA boundaries](../../docs/build.md#client-qa-boundaries)
for ownership and fidelity limits, and [editor setup](../../docs/ide.md).
`./bazel run //:web_dev` serves the repository Web example, not an installed npm consumer.
