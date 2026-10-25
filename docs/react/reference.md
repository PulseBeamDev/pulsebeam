---
title: React API reference
description: Public API reference for the PulseBeam React SDK.
---

# API reference

`@pulsebeam/react` owns Agent, capture, publication, receive, playback, and data
lifetimes through React. Non-React browser applications use `@pulsebeam/web`.

## `useAgent`

```ts
useAgent(config: AgentConfig): Agent
```

The hook returns a stable facade immediately. React activates it while mounted,
renews authorization when `token` changes, and closes the managed runtime after
unmount. Key the owning component when endpoint, identity, or topology changes.

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

`Agent` exposes readonly `connection`, `error`, `participantId`, `roomId`,
`authorizationExpiresAt`, and `remote`; plus `connect()`, `disconnect()`, and
`video(label)`, `audio(label)`, and `data(label, delivery)` lookup. React does not expose runtime lifecycle,
logging mutation, snapshot, subscription, or token-renewal methods on this
facade.

## Logical track lookup

```ts
const Camera = agent.video("camera");
const Microphone = agent.audio("microphone");
const Chat = agent.data("chat", "ordered");
```

Labels are application-defined identities. Lookup returns a stable logical
handle but does not reserve capacity, publish, subscribe, or play media. Local
media handles expose only readonly `kind` and `label`; local data handles add
`delivery` and binary `send`. Remote data handles are receive-only.

Equivalent SDK-issued handles within an Agent identify the same logical target.
Agent scope, local versus participant selection, participant ID, kind, label,
and data delivery distinguish targets. Re-rendering with an equivalent handle
preserves its owner. A different target releases the old owner and acquires a
new one after commit. Fabricated handles are unsupported.

This is a source-breaking replacement of the React `track` export,
`TrackSpec`/`VideoTrackSpec`/`AudioTrackSpec`/`DataTrackSpec` exports, `.track(spec)`
methods, and local media setters. There are no deprecated aliases. The public
Web descriptor API is unchanged.

Core validates portable label rules when an owner acquires the resource. Media
labels are nonempty UTF-8 strings of at most 64 bytes, without control characters
or `/`. Data labels are 1 to 85 ASCII characters from `[A-Za-z0-9_-]`.

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

## `Publish`

```tsx
<Publish
  track={agent.video("camera")}
  source={capture.video}
  enabled={true}
  content="motion"
/>
```

`track` must be a local audio or video handle. `source` is the matching captured
source or null. `enabled` defaults to true. Video content defaults to `motion`;
audio content defaults to `speech`. Mount owns publication, prop changes update
it, and unmount releases it. A null source retains the declaration without a
source, while `enabled={false}` retains ownership with media disabled. Neither
stops borrowed capture. Independent mounts own independent resources; duplicate
exclusive publication or audio playback claims conflict. Allocation or runtime
failures reach the nearest React error boundary.

## `Video`

```tsx
<Video
  source={remoteVideo}
  receive={{ priority: 100, minFps: 15 }}
  mirror={false}
  onPlaybackError={({ error, retry }) => {}}
/>
```

`source` accepts a captured video source, remote video track, or null. The
component forwards normal video element props except `src` and `children`.
`muted`, `autoPlay`, and `playsInline` default to true. `mirror` composes with the
provided style. Presentation and `receive` changes update the existing owner.
Captured video is preview-only and creates no remote receive demand or audible
playback.

For a remote source, mounted visibility and nonzero size own receive demand.
`receive` supports `priority`, `minFps`, `minHeight`,
and `playoutDelay: { minMs, maxMs }`. `onPlaybackError` provides the original
error and a gesture-safe retry function.

## `Audio`

```tsx
<Audio agent={agent} onPlaybackError={(failure) => setFailure(failure)} />
```

Mount one owner to follow and play the Agent's remote audio tracks. A
`PlaybackFailure` contains `message`, optional `cause`, and `retry()`. Call retry
from a user gesture when browser autoplay policy blocks playback. Unmount stops
and releases playback ownership.

## Remote participants and tracks

```ts
agent.remote.participants: readonly RemoteParticipant[]
agent.remote.participant(participantId): RemoteParticipant
participant.video(label): RemoteVideoTrack
participant.audio(label): RemoteAudioTrack
participant.data(label, delivery): RemoteDataTrack
```

A participant has `participantId`, `active`, and immutable `tracks`. A track has
`kind`, `participantId`, `label`, `id`, `active`, and `subscribe`. Data tracks
also expose `delivery`. Participant and track lookup return stable logical
handles that survive temporary absence and replacement. Discovery arrays are
immutable current snapshots.

## `Data`

```tsx
<Data track={agent.data("chat", "ordered")} onEvent={handleEvent} onError={report} />
```

Each committed mount owns one event subscription. A local data handle receives
from all matching publishers in the room; a participant data handle receives
only from that participant. A local handle also sends bytes:

```ts
const payload = Uint8Array.of(0, 255);
await agent.data("chat", "ordered").send(payload);
```

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
