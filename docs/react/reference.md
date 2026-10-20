---
title: React SDK API reference
---

# API reference

Import React ownership, capture, and presentation APIs from `@pulsebeam/react`.
For a working application, start with the [quickstart](/react/quickstart).

## Construction and package boundaries

| Package | Agent ownership |
| --- | --- |
| `@pulsebeam/react` | `useAgent(config)` owns activation and cleanup |
| `@pulsebeam/web` | `createAgent(config)` creates a direct caller-owned imperative Agent; the caller closes it |

React does not export `createAgent`. Managed activation is an internal SDK
mechanism, not a public Web constructor or lifecycle API. The hook-owned Agent
has no manual close, raw desired-state setter, subscription loop, or low-level
snapshot API. There is no `agent.advanced` API.

## useAgent

```ts
useAgent(config: AgentConfig): Agent
```

Returns a stable handle immediately. Render and lookup are inert. React
activates the runtime after commit and releases it on unmount. Activation does
not connect or capture automatically. `connection`, `error`, admitted identity,
authorization expiry, and remote collections are reactive without application
subscriptions.

| Change | Result |
| --- | --- |
| Token | Renew authorization on the same Agent; obtain tokens from your backend |
| Logging level | Update diagnostics without replacing the Agent |
| Endpoint or topology | Replace the Agent and release the previous runtime |
| Different React key | Start a new logical lifetime; use this for a different room or participant |
| Unmount | Release runtime and declarations, not borrowed capture |

### Configuration types

`AgentConfig` requires `endpoint` (absolute HTTP(S) base URL) and `token`
(opaque participant bearer token). The runtime adds `/api/v1/native` to the
endpoint. Optional `topology: MediaTopology` fixes track capacities, not
participant limits, for this lifetime:

| Field | Default | Maximum |
| --- | --- | --- |
| `localVideos` | 2 | 32 |
| `localAudios` | 2 | 32 |
| `remoteVideos` | 16 | 32 |
| `remoteAudios` | 8 | 32 |

Omitted fields retain defaults. Local labels bind slots only when committed,
and keep that binding for the Agent lifetime even after withdrawal. Inert
lookups do not consume capacity. Increasing capacity requires a new Agent and
connection. Excess visible remote tracks remain unmapped and warn.

`logging: {level}` defaults to `warn`. Levels are `off`, `error`, `warn`, `info`,
`debug`, and `trace`. Chrome requires Verbose enabled for debug/trace output.

## Agent interface

| Member | Meaning |
| --- | --- |
| `connection`, `error` | Observed connection progress and nullable typed failure |
| `participantId`, `roomId` | Nullable authoritative application identities from admission |
| `authorizationExpiresAt` | Nullable deadline in Unix seconds |
| `local.video(label)`, `local.audio(label)` | Stable resource components, keyed by kind and label |
| `remote` | Stable logical remote identities and automatic audio recovery |
| `topic<T>(name, {mode})` | Stable typed topic component with a `publish` action |
| `connect()` | Request connection; not an admission promise |
| `disconnect()` | Withdraw connection intent and cancel recovery; capture remains owned by its hook |
| `reconnect()` | Explicitly retry/recover while preserving logical relationships |

Transient failures use core recovery. Terminal failure and its typed reason
persist across media, topic, token, logging, UI changes, and late transport
events. Correct the cause, then explicitly reconnect or disconnect/connect.
A new token alone does not clear a terminal failure.

## Local media and captured sources

```tsx
const Camera = agent.local.video("camera");
const Microphone = agent.local.audio("microphone");
// In the committed tree:
<Camera source={capture.video} enabled={cameraOn} />;
<Microphone source={capture.audio} enabled={micOn} />;
```

Each component renders no DOM. Props:

| Prop | Contract |
| --- | --- |
| `source` | Required `PublishableVideoSource` or `PublishableAudioSource` of the matching kind, or null |
| `enabled` | Default true; false sends black/silent media without withdrawing publication or stopping capture |
| `preferences.content` | Video: `motion` (default), `detail`, `text`; audio: `speech` (default), `music` |
| `preferences.degradation` | Optional `maintain-framerate`, `maintain-resolution`, or `balanced` |

Committed mounts exclusively own `(Agent, kind, label)`, even with null source.
Duplicate mounts throw `RelationshipOwnershipError` without replacing the
first owner. Capacity exhaustion throws `LocalTrackCapacityError`. Use an error
boundary where declarations can exceed known capacity. Equal semantic props
are no-ops. Null or ended source withdraws effective publication but retains
ownership; unmount releases only that declaration. Neither stops caller capture.
Replacement retains logical identity and committed enabled/preferences state.
Tuning is best effort, not a codec guarantee or a substitute for enablement.

`capture.video` and `capture.audio` are publishable sources with acquisition
metadata. Pass them directly to publications or playback; no track extraction is
needed. Capture owns originals; each Agent borrows them and owns its sender
resources. One Agent's mute does not mute another Agent or stop a device.

### Source types

`VideoSource` and `AudioSource` are opaque, kind-specific SDK source contracts.
Captured media, local publication handles, and remote handles satisfy the matching
contract while exposing their own additional properties. A `{kind: "video"}` or
browser track alone is not a source.

`PublishableVideoSource` and `PublishableAudioSource` are narrower capabilities
implemented by captured sources. Local and remote handles are not publishable
inputs: accepting a source for playback does not enable remote forwarding or
publication-to-publication routing.

## Video

`Video.source` accepts `VideoSource` or null: pass `capture.video`, a local video
component handle, or a remote video handle directly. Null renders no DOM. It supports HTML video attributes except
`src` and `children`, plus `mirror` (false), `onPlaybackError`, and `preferences`.
`autoPlay`, `muted`, and `playsInline` default to true.

`onPlaybackError` receives `{error, retry(): Promise<void>}`. Store the retry and
invoke it from a user gesture, catching rejection. Retired presentations cannot
invoke a callback or restart playback. A local-handle preview follows publication
enablement; a captured-source preview shows the device independently.

### Video preferences

```tsx
<Video source={remote} preferences={{
  minHeight: 360, minFps: 15, priority: 100,
  latency: {minMs: 100, maxMs: 200},
}} />
```

All numeric fields must be finite unsigned 32-bit integers. Latency requires
`minMs <= maxMs`; invalid values throw without coercion. Minimum height, frame
rate, and priority default to zero. Latency omission requests receiver defaults,
which is distinct from explicit `{minMs: 0, maxMs: 0}`.

Only visible, attached remote presentations contribute demand. Local previews
create none. For one remote track, contributions aggregate independent of order:
maximum physical-pixel measured height, `minHeight`, `minFps`, and `priority`;
latency encloses specified ranges using the lowest minimum and highest maximum.
This can relax an individual preference. Hidden, offscreen, detached, and
unmounted consumers contribute nothing and cannot clear another's contribution.

Removing the last latency preference requests default receiver policy. A
receiver stamped with fixed bounds cannot clear them in its existing transport.
The server may remap media to a compatible receiver or request transport
recovery. Logical handles remain stable. A new compatible receiver/transport
permits defaults again, not a guarantee of a browser's buffering algorithm.

Minimums are best effort. Allocation protects minimum height and frame rate
jointly before target upgrades; priority wins within those stages. Insufficient
bandwidth or layers may reduce quality or pause forwarding without removing the
receiver assignment. There is no competing normal React receive setter.

## Capture hooks

```ts
useUserMedia(options: UserMediaOptions): CaptureResult
useDisplayMedia(options: DisplayMediaOptions): CaptureResult
```

Both require explicit `video` and `audio` fields. Construction is inert;
`request()` begins acquisition. Use a user gesture, especially for display capture.

| Kind | `useUserMedia` option |
| --- | --- |
| Video | false, true, or `{device?: CameraDevice, preferences?: {width?, height?, frameRate?}}` |
| Audio | false, true, or `{device?: MicrophoneDevice, preferences?: {echoCancellation?, noiseSuppression?, autoGainControl?}}` |

True or an options object without a device selects the platform default; no
inventory is required first. Explicit device selection is exact or fails, never
retries another/default device. Dimensions/frame rate must be positive finite
numbers; processing preferences must be booleans. Preferences map to best-effort
requests, not exact/minimum constraints. Browser constraint dictionaries are not
part of this API.

Display options accept the same per-kind preferences but no device selector.
The platform chooser owns selection. Video is required; display audio may be
absent even when requested. Both user-media kinds false stops/returns idle.

**CaptureResult** exposes readonly `video: CapturedVideo | null`,
`audio: CapturedAudio | null`, `state` (`idle`, `requesting`, `active`, `error`),
`error`, plus `request()` and `stop()`. Each captured object is both the usable
source and its acquisition metadata; it is null without that live source:

- `kind`: `video` or `audio`.
- `device`: actual typed camera/microphone, or null when identity is unknown.
  Display selection is not fabricated as a camera/microphone.
- `settings`: actual track-reported `VideoCaptureSettings` (width, height,
  frameRate) or `AudioCaptureSettings` (echoCancellation, noiseSuppression,
  autoGainControl). Unknown fields are absent, never copied from the requested
  `VideoCapturePreferences` or `AudioCapturePreferences`. Settings are acquisition
  observations, not adaptive telemetry or raw browser settings dictionaries.

User-media option changes reconcile after an explicit request while acquiring
or active. Equal options are no-ops. Replacement acquires and validates a full
candidate before swapping, then stops superseded originals. Failure preserves
working sources/settings and exposes an error without retrying unchanged failed
options. A failed first acquisition needs another explicit request.

`request()` resolves after committing the winning result. Failure rejects with
`MediaCaptureError`; superseded requests reject with `request-cancelled` and late
tracks are stopped. `stop()` is idempotent, invalidates pending work, releases
originals, clears observations/error to idle, and disables option-driven restart
until the next request. Unmount provides the same cleanup. Retained actions
cannot acquire after unmount.

Ended user-media tracks disappear from sources/observations; all ended means
idle. Display-video ending stops the whole sharing session. Ending never
silently restarts capture. Display options apply only to the next explicit
request; replacement may prompt, and failure retains working sharing.

## useMediaDevices

`useMediaDevices(): MediaDevicesResult` has its own lifetime independent of
capture. It returns readonly `cameras`, `microphones`, `speakers`, `state`
(`loading`, `ready`, `error`), and nullable `error`.

`CameraDevice`, `MicrophoneDevice`, and `SpeakerDevice` have `id`, `label`, and
literal `kind` values `camera`, `microphone`, and `speaker`. IDs are opaque and
platform-local, may change with permission, and must not be interpreted. Labels
may initially be empty. Speaker inventory does not imply output routing.

Mounted inventories refresh after successful acquisition, `devicechange`, and
observable camera/microphone permission changes. Browser support for permission
observation varies; unsupported changes may only become visible on device change,
acquisition, or remount. Enumeration is revision-fenced and listeners are removed
on unmount. Capture does not own these observers.

## Remote media

`remote.participants`, `remote.videoTracks`, and `remote.audioTracks` are readonly,
exclude self, and preserve collection identity until membership changes.
`remote.participant(participantId)` returns a stable participant with `participantId`,
`video(label)`, `audio(label)`, `videoTracks`, and `audioTracks`. Lookup is inert
and works before discovery. Handles survive absence, remapping, replacement,
and reconnect.

Remote video exposes `kind`, external `participantId`, `label`, `active`, and
`subscribe`. `active` means catalog presence, not decoded playback. Preferences
belong to `Video`, not a handle-wide setter.

Remote audio exposes `kind`, external `participantId`, `label`, `receiving`,
`subscribe`, `readWaveform(Float32Array)`, and `readSpectrum(Float32Array)`.
`receiving` follows receiver assignment, not sound or autoplay. Reads use the
default 2048-point FFT: leading amplitudes or decibel bins fill smaller buffers;
unavailable data/oversized tails are zero or negative infinity respectively.
Reads do not cause rerenders.

Automatic remote audio requires no presentation component. Call
`remote.resumeAudio()` from a gesture for exceptional autoplay recovery, even
before arrival; catch rejection. Disconnect removes routes; reconnect restores
receivers without duplicate output.

## Snapshots, status, and errors

`ConnectionState` includes `disconnected`, `initializing`, `creating-offer`,
`joining`, `applying-answer`, `waiting-for-transport`, `waiting-for-signaling`,
`connected`, `reconnecting`, `retry-waiting:N`, `closing`, and `terminal-failure`.
Do not infer admission from intent or intermediate transport progress.

### Errors

`AgentError` has `class` and `message`. Classes include initialization,
invalid-configuration, authorization, protocol, transient, resource-expired,
retry-exhausted, validation, and runtime. Use class and connection state together.

`MediaCaptureError` has machine-readable `code`, message, and optional `kind`
and `cause` where known. Codes: `permission-denied`, `device-not-found`,
`device-busy`, `constraint-unsatisfied`, `request-cancelled`, `not-supported`,
`unknown`. Diagnostic strings are not used to infer a more specific category.

`LocalTrackCapacityError` exposes `kind`, `label`, and `capacity`.
`RelationshipOwnershipError` identifies duplicate committed publication ownership.

## Topics

Every lookup requires an explicit mode and preserves identity by `(name, mode)`.
Different modes do not cross-deliver, even with the same name.

```tsx
const Chat = agent.topic<{text: string}>("chat", {mode: "ordered"});
// In the committed tree; accept and reportError are application UI callbacks.
<Chat onEvent={accept} onError={reportError} />;
// In a user-triggered send handler:
void Chat.publish({text: "hello"}).catch(reportError);
```

Names contain 1 to 85 ASCII letters, digits, underscores, or hyphens.
Lookup is inert: it creates neither publisher nor subscriber interest.
Generic `T` does not validate untrusted JSON or authorize its contents.

### Delivery modes

| Mode | Meaning |
| --- | --- |
| `latest` | Replaceable state per publisher data track; a newer value may supersede pending values of that track, never another publisher's value |
| `lossy` | Distinct best-effort events; no intentional latest-value replacement, ordering, or recovery |
| `ordered` | Distinct messages in increasing sequence per track; recovery copies are deduplicated, with finite retained history and reorder state |

Latest never delivers an older or duplicate sequence after a newer one from the
same track. Nonrecovering modes do not replay history or synthesize ordered gaps.
Ordered subscriptions start at a **live frontier**, without replaying
pre-subscription messages or calling historical traffic a gap. Once established,
a missing range is recovered when retained state permits; otherwise a gap precedes
continuation. There is no global order across tracks.

None of the modes provides an initial state snapshot, offline replay, durable
history, end-to-end acknowledgment, or an exactly-once application contract.
Sending does not loop back to the publisher.

### Inline events and identity

`onEvent` receives the same discriminated `TopicEvent<T>` as a direct Web
subscription. There is no separate continuity callback:

```ts
type TopicEvent<T> =
  | {
      readonly type: "message";
      readonly dataTrackId: string;
      readonly sequence: bigint;
      readonly participantId: string;
      readonly topic: string;
      readonly value: T;
    }
  | {
      readonly type: "gap";
      readonly dataTrackId: string;
      readonly participantId: string;
      readonly topic: string;
      readonly expectedSequence: bigint;
      readonly availableSequence: bigint;
    };
```

A gap skips `[expectedSequence, availableSequence)` before any later message
from that track. Render it visibly or reload authoritative application state;
do not present a discontinuous stream as complete.

`participantId` is the server-authored application participant identity, not a
payload field, display name, or canonical protocol ID. `dataTrackId` is the
opaque Catalog data-publication ID. Use `(dataTrackId, sequence)` to identify
received messages. Topic names and JSON cannot impersonate another publisher.

Sequences and gap boundaries are unsigned-64-bit-backed **TypeScript `bigint`**
through the SDK and generated JavaScript bindings. Do not convert them to
`number`. If storing metadata in JSON, encode them as decimal strings;
`JSON.stringify` does not serialize `bigint` directly.

Every newly opened publication, including reopening the same topic in one
connection, gets a fresh `dataTrackId` and sequence space. Reconnect restores
desired registrations but creates new publications. Queued frames, history, and
controls from a retired publication cannot become messages of its replacement.
Route changes without publication replacement do not change identity.
Recovery is not promised across these lifetimes.

### Ownership and errors

Each committed component owns an independent consumer. Callback changes do not
resubscribe and only committed callbacks run. Unmount releases that consumer.
Publishing works without mounted consumers; publishing interest lasts for the
Agent lifetime independently of subscription interest.

Optional `onError` receives `{code: "decode-failed", topic, cause}` for invalid
UTF-8/JSON, or `{code: "channel-failed", event}` with structured core
direction/mode attribution. Missing error callbacks do not suppress diagnostics.
A malformed payload releases only the affected consumer; remount retries.
Continuity gaps are ordinary inline events, not decode/channel failures.
Core owns transport restoration and recovery policy.

### Limits and send admission

| Bound | Behavior |
| --- | --- |
| Application payload | At most 65,536 UTF-8 JSON bytes, excluding the envelope |
| Topic channels | At most 64 registrations per Agent, counting publisher/subscriber channels |
| Core publication send queue | At most 256 messages; latest may supersede its own pending value |
| Ordered retained history | At most 256 accepted messages per publication |
| Ordered reorder state | At most 256 messages per data track |
| Pending typed publishes | At most 256 per topic handle |
| Initial binding/admission wait | At most 10 seconds in the Web/React typed API |
| Direct Web iterator | At most 256 buffered events and 256 pending reads |

Web iterator latest coalescing is per data track. Lossy may discard buffered
events under pressure. Ordered overflow preserves explicit skipped intervals
before continuation, merging ranges when necessary. If finite latest/ordered
state cannot represent all tracks safely, the affected iterator fails explicitly
instead of silently evicting another track or its continuity marker.

`Chat.publish(value): Promise<void>` resolves only after local transmission
acceptance. It does **not** certify remote delivery, persistence, an application
acknowledgment, or survival of supersession, queue pressure, or reconnect.
Invalid/oversized JSON, unavailable/closed publications, capacity rejection, and
sequence exhaustion are explicit admission failures; sequences never wrap.

The publisher waits for its server-authoritative Catalog binding before emitting
an application envelope. This initial wait is bounded and fenced to its selected
generation/publication. Disconnect, close, a failed channel, or replacement of
that selected generation/binding rejects pending work rather than sending it on
a replacement publication. A new application action may retry after recovery.

### Coordinated upgrade

Use matching server and SDK versions for this breaking topic contract. There is
no default mode, legacy alias, mixed-version wire negotiation, or dual-stack path.
Replace `reliable` with `ordered`; choose `latest` for replaceable state or
`lossy` for transient events instead of `unreliable`. Replace split message/gap
callbacks with `onEvent` and consume tagged events rather than bare values.

Normal Agent, snapshot, remote-resource, and generated app-facing identity fields
use `participantId` and `roomId` for application identities. Remove
`participantExternalId` / `roomExternalId` and remote `externalId` aliases.
Canonical participant/room IDs remain protocol-internal. Remove payload sender
IDs or message UUIDs used only to reconstruct transport identity; the event
already supplies sender and publication-scoped message identity.
