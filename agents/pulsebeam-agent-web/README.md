# PulseBeam Web

`@pulsebeam/web` is the caller-owned browser SDK. Use typed logical handles and
explicit resources for publication, reception, playback, data, and observation.
React applications should use [`@pulsebeam/react`](../react/README.md), whose
hooks own these lifetimes.

## Create and connect

Issue a short-lived participant token on your application backend. Keep signing
keys and API keys out of browser code.

```ts
import { Agent } from "@pulsebeam/web";

const agent = new Agent({
  endpoint: "https://media.example.com",
  token: "SHORT_LIVED_PARTICIPANT_TOKEN",
});
agent.join("conference-42");
```

Construction synchronously initializes the browser runtime and validates local
configuration. It does not connect, select a room, or acquire media. Current Core
state is available immediately through `agent.state()`, without an initialization
Promise.
`join(room)` selects the room and records connection intent. Use a token with
Join permission for that room. The call does not acknowledge admission or
media readiness. The Agent retries transient failures. Call `join(room)` again
for an explicit retry after admission is denied, or `join(otherRoom)` to move
when the token grants access. Use `agent.renew(token)` to replace credential
intent for the same participant, and read `agent.state()` for the result.
Replace the Agent to change endpoint or participant identity.
`await agent.close()` ends the session and releases its resources.

## Look up handles without acquiring resources

```ts
const camera = agent.video("camera");
const microphone = agent.audio("microphone");
const chat = agent.data("chat");
const room = agent.room("conference-42");
const participant = room.participant("alice");
const remoteCamera = participant.video("camera");
const remoteChat = participant.data("chat");
```

Lookup is inert and returns logical handles pinned to the supplied application
room name. A remote handle can exist before its participant or publication
appears and survives temporary absence or replacement, but it never follows the
Agent into another room. Resource factories, not lookup, acquire ownership. Room
references replace unpinned `Agent.participant(id)` lookup. The descriptor and
`.track(spec)` APIs remain absent.

## Own one catalog page

These APIs describe the current source build. Use matching SDK and server
artifacts; compatibility with a released package/server pair is not qualified by
this README.

`const watch = room.watch()` owns one bounded live page. Read
`watch.state()` and observe its current-first, coalesced `watch.updates()` cursor.
Distinguish `WatchStatus.Loading`, `Live`, `Failed` and `Closed`; loading is not an
empty result. A Live snapshot exposes `state.participants` with
application identity from `id()` and metadata from `tracks()`, including presence-only entries.

The default limit is 100, maximum 500, with eight pending/live watches per Agent.
Use `state.hasMore` and `state.nextCursor` to explicitly replace the watch for the
next page with the same room and limit. Do not automatically accumulate every
page. Continuations expire after 60 seconds; invalid/expired/wrong-scope cursors
fail explicitly. Offer a first-page restart without a cursor. Retained page
values stay stable while the SDK retains only its current page.

Discover and Subscribe are independent permissions. Convert selected metadata to
pinned references using `room.participant(info.id())` and the publication's
kind and label; lookup does not subscribe. Known-address reception does not need
Discover. Skip self when creating receive owners. There is no global participant
collection, participant publication listing, or catalog/mapping/media field in
public session snapshots.

Close each update cursor with `updates.close()` and release its watch with
`await watch.close()`. Watch close does not close independently owned subscriptions.
A watch has at most 32 independent cursors and one pending read per cursor.
See the [bounded React roster example](../../docs/react/guides.md#remote-discovery-and-video-receive-policy)
for explicit Next/First-page navigation and user-visible states.

## Publish caller-owned capture

```ts
import { VideoSource } from "@pulsebeam/web";

const capture = await navigator.mediaDevices.getUserMedia({ video: true });
const source = VideoSource.fromTrack(capture.getVideoTracks()[0]!);
const publication = camera.publish({ enabled: true });
const pipe = source.pipeTo(publication);
publication.configure({ enabled: false });
```

Use `AudioSource.fromTrack(track)` with an audio publication. To change input,
close the prior pipe before piping the replacement source into the same
publication. Configuring `enabled` changes mute without releasing the publication.

Sources and publications have separate lifetimes. Closing a publication releases
that binding, not its source. Closing a source makes that producer terminal;
it cannot be reused. The SDK owns its publication-track clones, not the browser
capture tracks passed to `fromTrack`. Stop those caller-owned tracks yourself
when capture is no longer needed:

```ts
await pipe.close();
await publication.close();
await source.close();
for (const track of capture.getTracks()) track.stop();
```

## Receive and present video

Subscribe to the exact remote track before attaching playback:

```ts
import { PlayoutDelay } from "@pulsebeam/web";

const subscription = remoteCamera.subscribe({
  visible: true,
  height: 720,
  minHeight: 360,
  minFps: 15,
  priority: 100,
  playoutDelay: PlayoutDelay.Fixed.new({ minMs: 100, maxMs: 200 }),
});
const output = await subscription.playbackOutput();
const receivedTrack = output.track();
const element = document.querySelector("video")!;
if (receivedTrack) {
  element.srcObject = new MediaStream([receivedTrack]);
  await element.play();
}
```

An output follows one physical receiver incarnation, not every future track
for that name. Reacquire an output after source retirement. Closing it does not
close its subscription. Await `output.close()` and `subscription.close()` when
their respective owners end.
Clear the element’s `srcObject` when detaching playback. If the browser blocks
`play()`, offer a user gesture to retry.

## Send and receive bytes

Applications own codecs, schema validation, versioning, and malformed-payload
policy. Local data handles create owned publications; room-pinned participant
data handles create independent subscriptions.

```ts
import { DataEvent } from "@pulsebeam/web";

const publication = chat.publish();
const messages = remoteChat.subscribe();
const reading = (async () => {
  while (true) {
    const event = await messages.next();
    if (event === undefined) return;
    if (DataEvent.Message.instanceOf(event)) handleBytes(event.inner.data);
  }
})();
await publication.send(new TextEncoder().encode("hello"));
```

`handleBytes` is an application handler. Handle processing failures
on `reading`. Close the publication with `await publication.close()` when sending
ends. To stop reception, call `await messages.close()`, then await the
reading task. Closing one receiver does not close other receivers or their Agent.
There is no implicit JSON codec, automatic iterator, or callback subscription.

`publish()` and `subscribe()` default to `DataDelivery.Reliable`. Use
`{ delivery: DataDelivery.Unreliable }` for available live messages or
`{ delivery: DataDelivery.Latest }` for current values. The subscription mode
must match the exact source. Each message identifies its publication and
participant, and has a `bigint` sequence and binary data. Source replacement may
produce a discontinuity; an unrecoverable Reliable gap rejects `next()` and
ends only that subscription. Read one event at a time.

Messages go to current authorized subscribers. A successful `send(bytes)`
confirms local admission, not receipt. Transport is live and finite, not
durable storage.

## Observe state and release ownership

`agent.state()` returns the current immutable state. Each call to
`agent.updates()` opens an independent cursor. Await `cursor.next()` for the
current state and then for changes. Intermediate revisions may coalesce; this
is not an event history. Use one pending read per cursor.

Call `cursor.close()` to end observation and wake a pending read with
`undefined`. Closing a cursor leaves the Agent running. When the Agent closes,
open cursors return its final closed state, then `undefined`.

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

The JavaScript unit test exercises the media-handle registry without WASM.
Pinned-Node artifact tests instantiate the shipped bindings and check validation,
state cursors, resource ownership, and close. Node is a test host, not a supported
application platform. Deterministic native/SFU simulation owns delivery,
identity, publication replacement, and Reliable continuity loss.

The browser adapter prepares receivers through the existing Peer/executor before
Core requests server admission. Successful description completion, correlated to
the current generation, operation, and owner, establishes local configuration.
Actual track events provide track observations; packets, frames, unmuting, and
exhaustive parameter readback are not configuration gates. Server acceptance is
still required for application-visible subscription readiness. See
[Core's receive flow](../pulsebeam-agent-core/README.md#prepared-reception).

Run the browser owner for shipped Web-to-Web media, SCTP incarnation reuse,
trusted-gesture audio playback, and the Native Python producer to packed React
receiver workflow. It does not build Meet. See the
[client QA boundaries](../../docs/build.md#client-qa-boundaries),
[testing contract](../../docs/build.md#testing-contract), and
[editor setup](../../docs/ide.md).
`./bazel run //:web_dev` serves test fixtures. To try a session in a browser, use
[the React quickstart](../../docs/react/quickstart.md).
