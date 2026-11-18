---
title: React API reference
description: Public API reference for the PulseBeam React SDK.
---

# API reference

`@pulsebeam/react` owns Agent, capture, publication, receive, playback, and data
lifetimes through React. Non-React browser applications use `@pulsebeam/web`.

## `useAgent`

```ts
useAgent(config: AgentConfig): Agent | null
```

The hook returns null until a committed effect creates the actual Web SDK Agent.
It never joins implicitly. React observes that handle, renews authorization when
`token` changes, and closes it on unmount. Key the owning component when endpoint,
identity, or topology changes.

```ts
interface AgentConfig {
  endpoint: string;
  token: string;
  topology?: {
    localVideos?: number;
    localAudios?: number;
    remoteVideos?: number;
    remoteAudios?: number;
  };
  logging?: { level?: LogLevel };
}
```

The returned type is `Agent` from `@pulsebeam/web`, including `join(room)`,
`state()`, `updates()`, and typed local and room-pinned remote lookups. Rendering
reads the same cached-by-revision `state()` object as the internal observation
bridge. Hook-owned Agents close on unmount; token prop changes call `renew()`.
Do not add a competing imperative token-renewal owner.

## `usePublish` and `useSubscribe`

```ts
const publication = usePublish(agent.video("camera"), capture.video);
const subscription = useSubscribe(
  agent.room("robots").participant("robot-42").video("front"),
);
```

Call dependent hooks in a mounted child after the Agent exists. Both hooks also
accept null references and return null until effect-owned creation. Their results
are the SDK's actual typed publication/subscription handles, not wrapper objects.
They observe `state()` internally; no separate snapshot hook is needed. Ongoing
admission and execution failures remain state values. Creation or observation
failures reach the nearest Error Boundary after cleanup.

`usePublish(track, source?, options?)` accepts local video, audio, or data refs.
Media sources may be matching SDK or capture sources; null or omitted source
retains a publication without input. Media options are `{ enabled: boolean }`,
with active defaults when omitted. Source changes replace only the pipe; option
changes configure the existing publication. Borrowed sources survive cleanup.
Data publication takes no source and returns `DataPublication`.

`useSubscribe(remoteTrack, options?)` accepts room-pinned video, audio, or data
refs. Media options are the SDK's complete `VideoObservation` or
`AudioSubscriptionConfig`, not partial playback hints. Omit options to use SDK
defaults. Changes configure the existing subscription; omitting options restores its SDK
defaults. Data returns `DataSubscription<D>` with its typed `next()` event API.
Changed target scope closes the old handle and returns null until its replacement
exists. Unmount and Strict Mode replay close owned resources and cursors.

## Logical track lookup

```ts
const Camera = agent.video("camera");
const Microphone = agent.audio("microphone");
const Chat = agent.data("chat", "ordered");
```

Labels are application-defined identities. Lookup returns a stable logical
handle but does not reserve capacity, publish, subscribe, or play media. Local
media refs expose `kind`, `label`, and `publish()`. Local data refs also expose
`delivery`; their publications send bytes. Remote refs expose `subscribe()`.

Equivalent SDK-issued handles within an Agent identify the same logical target.
Agent scope, local versus participant selection, participant ID, kind, label,
and data delivery distinguish targets. Re-rendering with an equivalent handle
preserves its owner. A different target releases the old owner and acquires a
new one after commit. Fabricated handles are unsupported.

This is a source-breaking replacement of the React `track` export,
`TrackSpec`/`VideoTrackSpec`/`AudioTrackSpec`/`DataTrackSpec` exports, `.track(spec)`
methods, and local media setters. There are no deprecated aliases. The
caller-owned Web SDK also uses typed lookups, synchronous factories, and explicit
resource lifetimes.

Core validates portable label rules when an owner acquires the resource. Media
and data labels are nonempty UTF-8 strings of at most 256 bytes. Limits count
encoded bytes, not JavaScript characters.

Data delivery is `"latest" | "lossy" | "ordered"`:

- `latest` provides current-value semantics and does not report gaps.
- `lossy` emits available live messages without recovery gaps.
- `ordered` preserves ordered delivery and may emit gaps when history is no
  longer recoverable.

## `useCapture`

```ts
useCapture("device", options: DeviceCapture): CaptureResult
useCapture("display", options: DisplayCapture): CaptureResult
```

Device options accept `video` and `audio` booleans or settings objects. Video
settings are `device`, `width`, `height`, and `frameRate`. Audio settings are
`device`, `echoCancellation`, `noiseSuppression`, and `autoGainControl`.
Display options require video as `true` or video settings, plus an audio
boolean.

```ts
interface CaptureResult {
  readonly video: CapturedVideo | null;
  readonly audio: CapturedAudio | null;
  readonly state: "idle" | "requesting" | "active" | "error";
  readonly error: MediaCaptureError | null;
  request(): Promise<void>;
  stop(): void;
}
```

Call `request()` from a user gesture. `stop()` ends all browser tracks owned by
that capture. Changed device options reconcile active or acquiring capture;
changed display options apply on the next explicit request. Unmount releases
capture.

`MediaCaptureError` has a stable `code`, optional media `kind`, message, and
cause. Codes distinguish unavailable APIs, invalid constraints, permission,
device, cancellation, and browser failures.

## `useDevices`

```ts
useDevices(): DevicesResult
```

The result has readonly `cameras`, `microphones`, `state`, and `error`. Devices
are typed `{ kind, id, label }` objects suitable for `useCapture`. Device labels
may be empty before permission is granted.

## `Video` and `Audio`

```tsx
const video = useSubscribe(remoteVideo, {
  visible: true, height: 360, minHeight: 180, minFps: 15, priority: 100,
  playoutDelay: { mode: "adaptive" },
});
const audio = useSubscribe(remoteAudio);

<Video source={video} mirror={false} onPlaybackError={({ error, retry }) => {}} />
<Audio source={audio} onPlaybackError={(failure) => setFailure(failure)} />
```

`source` accepts matching captured media, a Web SDK media source, an already-owned
video or audio subscription, a `MediaObservation`, or null. A logical remote track
is not a playback source: call `useSubscribe` first. Null renders nothing.

Playback never creates or closes reception. Each view of a subscription acquires
its own observation clone and releases only that clone and its update cursor on
unmount or source replacement. Pending acquisition is cancelled; late output
cannot attach to a retired view. An explicitly supplied `MediaObservation` or
capture source is borrowed, not closed. Independent views do not create extra
subscription owners.

Both components forward normal media element props except `src` and `children`.
`autoPlay` defaults to true; Video also defaults `muted` and `playsInline` to true.
`mirror` composes with Video's provided style. Presentation changes do not recreate
the subscription. Configure reception through `useSubscribe`, not renderer props.

`onPlaybackError` receives a `PlaybackError` containing the original browser
`error` and `retry()`. Call retry from a user gesture when autoplay is blocked.
A retired view's retry does nothing; callback changes use the latest committed
callback. Resource setup failures reach the nearest React error boundary.

## Discover one bounded page with `useWatch`

```tsx
const room = useMemo(() => agent.room(roomName), [agent, roomName]);
const watch = useWatch(room, { limit: 100, cursor });
const state = watch?.state();
```

`useWatch(roomRef | null, config?)` returns the actual owned Web `Watch`, or
`null` until committed ownership. It observes updates internally, so reading
`watch.state()` in render rerenders when the state changes. A null room acquires
nothing. Unmount closes this watch, not independently owned subscriptions.

Room, limit and cursor are immutable scope. Changing the room or config values
replaces the watch; recreating an equivalent config object does not. Cursor bytes
are copied and compared by value. Omit the cursor for the first page.

| Value | Contract |
| --- | --- |
| `WatchStatus.Loading` | No complete page yet; not a successful empty result. |
| `WatchStatus.Live` | `state.page` is one complete immutable page. An empty participant array is valid. |
| `WatchStatus.Failed` | Inspect `state.error`; correct the cause and create a new watch. |
| `WatchStatus.Closed` | Final healthy close state; its page is cleared. A prior failure remains a failure on close. |
| `page.participants` | Ordered participant metadata: `id`, application `externalId`, and `publications`. Presence-only participants have no publications. |
| `publication.kind.tag` | `Video`, `Audio`, or `Data`; data also carries `kind.inner.delivery`. |
| `page.hasMore`, `page.nextCursor` | Explicit continuation. Replace the watch to request the next page; do not append every page automatically. |
| `page.cursorExpiresAtUnixSeconds` | Continuation expiry. Invalid, expired or wrong-scope cursors fail explicitly; offer a first-page restart. |

The default limit is 100 participants, with a maximum of 500. An Agent owns at
most eight pending/live watches. Each watch retains one current page and allows
32 independent update cursors, with one pending `next()` per cursor. The server's
opaque continuation expires after 60 seconds and is bound to the room and limit;
it does not confer permission. Retained application snapshots remain stable when
a newer page replaces the current page.

Non-React callers use `agent.room(name).watch(config)`, `state()`, `updates()` and
`close()`. Update cursors are current-first and coalesced, not an event log.
Closing a cursor releases only that cursor; closing the watch releases its page
and reservation. Discovery requires Discover permission independently of Subscribe.

See [bounded roster navigation](/react/guides#remote-discovery-and-video-receive-policy).

## Look up pinned participants and tracks

```ts
agent.room(room).participant(externalId): RemoteParticipant
participant.video(label): RemoteVideoTrack
participant.audio(label): RemoteAudioTrack
participant.data(label, delivery): RemoteDataTrack
```

Lookup is inert. Use an application-known address or a publication from the
current Watch page, passing the participant's `externalId` and publication label.
These logical handles survive temporary absence and replacement but stay pinned
to their literal room. They do not enumerate participants or publications:
`agent.participants`, `participant.tracks` and global snapshot catalog/mapping/media
fields are absent. Session snapshots are not a discovery API.

Create receive demand with `useSubscribe`. Known-address Subscribe does not
require Discover, and a watch never creates media reception. Skip your own
application participant ID when turning a roster into receive owners.

## `Data`

```tsx
<Data
  track={agent.room("meeting").participant("alice").data("chat", "ordered")}
  onEvent={handleEvent}
  onError={report}
/>
```

Each committed mount owns one event subscription to its pinned participant.
Receiving does not require watching the room catalog. To send, retain an owned
publication created with `usePublish`:

```tsx
const publication = usePublish(agent.data("chat", "ordered"));

async function send(payload: Uint8Array) {
  if (!publication) return;
  await publication.send(payload);
}
```

The hook returns null until its effect creates the publication, then closes it
on unmount. Handle send failures at the application action.

Events are discriminated by `type`:

```ts
type DataMessage = {
  readonly type: "message";
  readonly trackId: string;
  readonly participantId: string;
  readonly sequence: bigint;
  readonly data: Uint8Array;
};

type DataGap = {
  readonly type: "gap";
  readonly trackId: string;
  readonly participantId: string;
  readonly from: bigint;
  readonly to: bigint;
};
```

`DataEvent<"latest" | "lossy">` is message-only. `DataEvent<"ordered">` may
also be a gap. PulseBeam does not apply JSON or text codecs. Applications must
encode, decode, validate, version, and decide how to handle malformed payloads.
Catch malformed-payload errors inside `onEvent` if reception should continue.

Payloads may be empty or arbitrary binary bytes, up to 65,536 bytes. Admission
copies bytes so later caller mutation does not alter the accepted send.
Resolution means local transmission acceptance, not remote delivery. Pending
sends reject on disconnect, invalid binding, terminal failure, timeout, or host
rejection rather than migrating to replacement publications.

Each receive owner buffers at most 256 events. Latest coalesces by publisher
track; lossy may drop queued events; ordered overflow exposes skipped ranges
before later messages. Unrepresentable overflow ends only the affected owner.

`onEvent` has type `(event: DataEvent<D>) => void | Promise<void>`. Each mount
runs one serial loop and awaits the handler before taking its next event. Other
mounts, including mounts of the same track, continue independently. Awaiting is
not transport backpressure, persistence, exactly-once processing, or protection
against overload loss.

The next event uses the latest committed callback. Callback changes do not
resubscribe or replace a running handler. A handler throw/rejection or receive
failure stops that loop, releases its receive owner, and reports once to the
current `onError`, or the diagnostic fallback when absent. Reporting failures
cannot prevent cleanup. Callback changes do not restart a failed loop; remount
`Data` to restart it.

Unmount or target replacement promptly cancels receive ownership, even if an
application promise is still pending. No further handlers or late error
callbacks run for that retired lifetime. An already-running application promise
cannot be forcibly cancelled, and its side effects cannot be undone.

## Connection and errors

`connection` reports the detailed lifecycle, including `disconnected`, setup,
connected, reconnecting, retry-waiting, closing, and terminal-failure states.
`connect()` records desired connection; `disconnect()` clears it. Surviving
component declarations and desired state remain through disconnect and transient
loss. Unmount and session replacement release their owners. Final Agent
disposal cannot be reversed. `AgentError`
contains a stable failure `class` and `message`. Present errors to users without
logging credentials.

## Capacity

Topology counts reserve fixed media sender and receiver resources when the Agent
is created. They are track slot counts, not participant limits. Publication and
receive owners arbitrate those resources; logical lookup itself consumes no
capacity. Replace the Agent to change topology.
