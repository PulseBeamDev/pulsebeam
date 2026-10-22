# PulseBeam Web

`@pulsebeam/web` is the caller-owned browser SDK. It exposes descriptor-based
track lookup and explicit ownership objects for publication, receive, playback,
and data streams. React applications should use `@pulsebeam/react`, whose
components own these lifetimes.

## Install and create an Agent

```sh
npm install @pulsebeam/web
```

```ts
import { createAgent, track } from "@pulsebeam/web";

const agent = createAgent({
  endpoint: "https://media.example.com",
  token,
});

agent.connect();
```

`connect()` records connection intent and `disconnect()` clears it. The Agent
retries transient failures while connection is desired. Replace the Agent when
endpoint, room identity, participant identity, or topology changes. Use
`renewAuthorization(token)` only for the same logical session.

The caller must eventually close every attachment and event iterator, then call
`agent.close()`. Closed objects cannot be reused.

## Descriptors and lookup

```ts
const camera = track.video("camera");
const microphone = track.audio("microphone");
const chat = track.data("chat", "ordered");

const Camera = agent.track(camera);
const Microphone = agent.track(microphone);
const Chat = agent.track(chat);
```

Descriptors are immutable application identities. Keep labels stable. Lookup
returns stable logical handles and has no ownership side effects. It does not
publish, receive, play, subscribe, or consume capacity.

## Capture and publication

Browser tracks become SDK capture sources through `createCaptureSource`.
Publication begins only when the local handle has an attachment.

```ts
import {
  attachLocalTrack,
  createCaptureSource,
} from "@pulsebeam/web";

const stream = await navigator.mediaDevices.getUserMedia({ video: true });
const source = createCaptureSource(stream.getVideoTracks()[0], "video");
const publication = attachLocalTrack(Camera, {
  source,
  enabled: true,
  config: { contentHint: "motion" },
  onError: report,
});
await publication.ready;
```

An attachment supports `setSource`, `setEnabled`, `setConfig`, and `close`.
Closing withdraws publication but does not stop the browser capture track. The
application owns browser capture and must stop it separately.

Capacity is reserved by ownership, not descriptor lookup. Media topology is
fixed when the Agent is created. Replace the Agent to change it.

## Remote discovery and video

```ts
for (const participant of agent.remote.participants) {
  for (const remote of participant.tracks) {
    console.log(remote.kind, remote.label, remote.active);
  }
}
```

`participants` and each participant's `tracks` are immutable current snapshots.
`agent.remote.participant(id)` and `participant.track(descriptor)` return stable
logical handles that survive temporary absence and replacement.

Attach a remote video handle to an `HTMLVideoElement` to own receive and
playback:

```ts
import { attachRemoteVideo } from "@pulsebeam/web";

const remote = agent.remote.participant(participantId).track(camera);
const video = document.querySelector("video")!;
const attachment = attachRemoteVideo(remote, video, (failure, retry) => {
  showRetryVideoButton(() => retry());
}, {
  preferences: {
    priority: 100,
    minFps: 15,
    playoutDelay: { minMs: 100, maxMs: 200 },
  },
});
await attachment.ready;
```

The attachment supports `setPreferences`, `retryPlayback`, and `close`. Close it
when the element no longer owns receive demand.

## Remote audio

One Agent-level audio attachment follows discovered remote audio tracks:

```ts
import { attachRemoteAudio } from "@pulsebeam/web";

const audio = attachRemoteAudio(agent, {
  onPlaybackError(failure) {
    showEnableAudioButton(() => failure.retry());
  },
});
await audio.ready;
```

Call retry directly from a user gesture when autoplay is blocked. Closing the
attachment stops and releases its playback ownership.

## Data tracks

Data tracks carry bytes. Applications own encoding, decoding, schema validation,
versioning, and malformed-payload policy.

```ts
const messages = Chat.events();

void (async () => {
  for await (const event of messages) {
    if (event.type === "gap") {
      showGap(event.from, event.to);
      continue;
    }
    handleBytes(event.data);
  }
})();

await Chat.send(new TextEncoder().encode("hello"));
```

Call `await messages.return()` or pass an `AbortSignal` when ownership ends.
Remote handles also expose `events()` but not `send()`.

Delivery modes are:

- `latest`: current-value semantics, message events only
- `lossy`: available live messages, message events only
- `ordered`: ordered messages with explicit `gap` events when unavailable
  history prevents continuous recovery

Messages include `trackId`, `participantId`, `sequence`, and `data`. Gaps include
`trackId`, `participantId`, `from`, and `to`. IDs and sequence spaces can change
when publications are replaced. Data transport is live and finite, not durable
application storage. Accepted sends are not delivery acknowledgments.

## State and cleanup

The public Agent exposes current connection, error, admitted participant and room
IDs, authorization expiry, `getSnapshot()`, and `subscribe()`. Use
`subscribe()` with `getSnapshot()` for framework integration.

Recommended cleanup order:

1. stop application event loops and return their iterators;
2. close video, audio, and local publication attachments;
3. disconnect if desired;
4. call `agent.close()`;
5. stop browser-owned capture tracks.

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
local-handle modules without a DOM. Typed consumers check public and generated
contracts. Deterministic native/SFU simulation owns delivery, identity,
publication replacement, ordered loss and gaps, and late-subscription proof.

Pinned-Node artifact tests instantiate emitted WASM and exercise lifecycle,
resource ownership, cancellation, generation fences, and binary data streams.
Controlled peer outcomes are not real WebRTC or application support for Node.

The required Web browser owner proves shipped SDK media through a local SFU,
native-track handoff, decoded receiver replacement, layout and observer plumbing,
and early/late trusted-gesture playback recovery. It does not build Meet or a
React acceptance fixture. See the [client QA boundaries](../../docs/build.md#client-qa-boundaries),
[testing contract](../../docs/build.md#testing-contract), and [editor setup](../../docs/ide.md).
`./bazel run //:web_dev` serves the repository example, not an installed npm consumer.
