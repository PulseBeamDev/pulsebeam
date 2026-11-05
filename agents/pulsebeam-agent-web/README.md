# PulseBeam Web

`@pulsebeam/web` is the caller-owned browser SDK. Use typed logical handles and
explicit resources for publication, reception, playback, data, and observation.
React applications should use [`@pulsebeam/react`](../react/README.md), whose
components own these lifetimes.

## Create and connect

Issue a short-lived participant token on your application backend. Keep signing
keys and API keys out of browser code.

```ts
import { createAgent } from "@pulsebeam/web";

const agent = await createAgent({
  endpoint: "https://media.example.com",
  token: "SHORT_LIVED_PARTICIPANT_TOKEN",
});
await agent.connect();
```

Creation initializes the browser runtime but does not connect or acquire media.
`connect()` records connection intent; it is not a delivery or media-readiness
acknowledgment. `disconnect()` clears that intent without closing the Agent or
its resources. The Agent retries transient failures while connection is desired.
Replace the Agent when endpoint, room identity, participant identity, or topology
changes. Use `await agent.renewAuthorization(token)` only for the same logical
session. `await agent.close()` is terminal and releases its resources.

## Look up handles without acquiring resources

```ts
const camera = agent.video("camera");
const microphone = agent.audio("microphone");
const chat = agent.data("chat", "ordered");
const participant = agent.participant("alice");
const remoteCamera = participant.video("camera");
const remoteChat = participant.data("chat", "ordered");
```

Lookup is inert and returns stable logical handles. A remote handle can exist
before its participant or publication appears and survives temporary absence or
replacement. Resource factories, not lookup, acquire ownership. This API replaces
descriptors, `.track(spec)`, and `.remote.participant(id)` without aliases.

## Publish caller-owned capture

```ts
import { VideoSource } from "@pulsebeam/web";

const capture = await navigator.mediaDevices.getUserMedia({ video: true });
const source = VideoSource.fromTrack(capture.getVideoTracks()[0]!);
const publication = await camera.publish(source, { enabled: true });
await publication.configure({ enabled: false });
await publication.configureSender({ contentHint: "text" });
```

Use `AudioSource.fromTrack(track)` with an audio handle. The source kind must
match the handle. Ordinary configuration and sender configuration are separate:
`configure({ enabled })` controls mute, while `configureSender(...)` controls
sender settings. Use `await publication.setSource(replacementSource)` to replace
the producer without changing the publication's logical name.

Sources and publications have separate lifetimes. Closing a publication releases
that binding, not its source. Closing a source makes that producer terminal;
it cannot be reused. The SDK owns its publication-track clones, not the browser
capture tracks passed to `fromTrack`. Stop those caller-owned tracks yourself
when capture is no longer needed:

```ts
await publication.close();
await source.close();
for (const track of capture.getTracks()) track.stop();
```

## Receive and present video

For ordinary DOM playback, an attachment owns receive demand and presentation:

```ts
import { attachRemoteVideo } from "@pulsebeam/web";

const element = document.querySelector("video")!;
const presentation = attachRemoteVideo(remoteCamera, element, undefined, {
  preferences: { minHeight: 360, minFps: 15, priority: 100 },
});
await presentation.ready;
```

The attachment follows element geometry and source replacement. It supports
`setPreferences`, `retryPlayback`, and asynchronous `close`. Supply its playback
error callback when the application needs to offer a trusted-gesture retry.
Close the attachment when the element no longer owns receive demand.

For custom media consumers, acquire a subscription explicitly:

```ts
const subscription = await remoteCamera.subscribe({
  visible: true,
  height: 720,
  minHeight: 360,
  minFps: 15,
  priority: 100,
  playoutDelay: { mode: "fixed", minMs: 100, maxMs: 200 },
});
const observation = await subscription.observe();
const receivedTrack: MediaStreamTrack | null = observation.track();
```

An observation follows one physical receiver incarnation, not every future track
for that name. Reacquire an observation after source retirement. Closing an
observation releases that observation, not its subscription. Await
`observation.close()` and `subscription.close()` when their respective owners end.
The DOM attachment performs this observation management for you.

## Select audio separately from playback

```ts
import { attachRemoteAudio } from "@pulsebeam/web";

const selection = await agent.selectAudio({ automatic: true });
const playback = attachRemoteAudio(agent, {
  onPlaybackError(failure) {
    // Call failure.retry() directly from the application's user-gesture handler.
    showEnableAudioButton(() => failure.retry());
  },
});
await playback.ready;
```

`showEnableAudioButton` is application UI in this example. Audio selection owns
receive policy; playback owns the decoded output route and autoplay recovery.
Attaching playback does not implicitly select remote audio. Configure explicit
selection with `await selection.configure({ automatic: false, selected:
[{ participantId: "alice", label: "microphone" }] })`.

Close `playback` and `selection` independently when their owners end. A custom
audio consumer can instead use `participant.audio(label).subscribe(...)` and
source-scoped observations.

## Send and receive bytes

Applications own codecs, schema validation, versioning, and malformed-payload
policy. Local data handles can send and receive. Participant-scoped data handles
are receive-only.

```ts
const messages = await remoteChat.receive();
const reading = (async () => {
  while (true) {
    const update = await messages.next();
    if (update.type === "closed") return;
    const event = update.event;
    if (event.type === "gap") {
      showGap(event.from, event.to);
    } else {
      handleBytes(event.data);
    }
  }
})();
await chat.send(new TextEncoder().encode("hello"));
```

`showGap` and `handleBytes` are application handlers. Handle processing failures
on `reading`. To stop reception, call `await messages.close()`, then await the
reading task. Closing one receiver does not close other receivers or their Agent.
There is no implicit JSON codec, automatic iterator, or callback subscription.

Delivery modes are `latest` (current value), `lossy` (available live messages),
and `ordered` (ordered messages with explicit gaps). Message events include
`trackId`, `participantId`, `sequence`, and `data`; gaps include `trackId`,
`participantId`, `from`, and `to`. Sequence and gap positions are `bigint`.
Physical publication replacement can change IDs and sequence spaces. Transport
is live and finite, not durable storage. Accepted sends are not delivery
acknowledgments.

## Observe state and release ownership

`agent.snapshot()` returns current immutable state. `agent.snapshots()` yields an
initial snapshot followed by updates; `agent.events()` yields future events.
Each cursor has `next()`, `close()`, and `isClosed()`. Reads return discriminated
`snapshot` or `event` values, `lagged` with a `bigint` skipped count, or `closed`.
Updates are bounded; observation is not durable history. These cursors do not
acquire media or connection intent. Closing a cursor is independent of closing
the Agent, and parent closure retires pending reads.

Stop application loops by closing their cursors, close playback and receive
resources, close publications and sources, then await `agent.close()`. Finally,
stop caller-owned capture tracks. Closing resources is idempotent; closed objects
cannot be reused.

## Contributor development

Run from a build-ready repository root:

```sh
./bazel build //:web
./bazel test //agents/pulsebeam-agent-web:public_contract //agents/pulsebeam-agent-web:uniffi_types
./bazel test //agents/pulsebeam-agent-web:unit_tests //agents/pulsebeam-agent-web:js_unit_tests
./bazel test //agents/pulsebeam-agent-web:node_runtime_tests
./bazel test //agents/pulsebeam-agent-web:web_runtime_browser
```

Direct JavaScript tests exercise production data, catalog, immutable-state, and
local-resource modules without a DOM. Typed consumers check public and generated
contracts. Deterministic native/SFU simulation owns delivery, identity,
publication replacement, ordered loss and gaps, and late-subscription proof.

Pinned-Node artifact tests instantiate emitted WASM and exercise initialization,
resource ownership, cancellation, validation, and generation fences. Their
controlled outcomes do not establish browser compatibility, decoded media,
failed transceiver-stop repair, or application support for Node.

The browser adapter prepares receivers through the existing Peer/executor before
Core requests server admission. Successful description completion, correlated to
the current generation, operation, and owner, establishes local configuration.
Actual track events provide track observations; packets, frames, unmuting, and
exhaustive parameter readback are not configuration gates. Server acceptance is
still required for application-visible subscription readiness. See
[Core's receive flow](../pulsebeam-agent-core/README.md#prepared-reception).

Run the browser owner for shipped-SDK media through a local SFU, native-track
handoff, decoded receiver replacement, SCTP incarnation reuse, layout, and
trusted-gesture playback recovery. It does not build Meet or a React acceptance
fixture. See the [client QA boundaries](../../docs/build.md#client-qa-boundaries),
[testing contract](../../docs/build.md#testing-contract), and
[editor setup](../../docs/ide.md).
`./bazel run //:web_dev` serves the repository example, not an installed npm consumer.
