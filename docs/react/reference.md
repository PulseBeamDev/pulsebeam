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
| `participantId` | Nullable canonical participant ID |
| `participantExternalId`, `roomExternalId` | Nullable authoritative application identities |
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
<Camera source={capture.videoTrack} enabled={cameraOn} />;
<Microphone source={capture.audioTrack} enabled={micOn} />;
```

Each component renders no DOM. Props:

| Prop | Contract |
| --- | --- |
| `source` | Required matching `CapturedVideoTrack` or `CapturedAudioTrack`, or null |
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

`CapturedVideoTrack` and `CapturedAudioTrack` are opaque source identities,
not browser tracks. Capture owns originals; each Agent borrows them and owns
its sender resources. One Agent's mute does not mute another Agent or stop a device.

## Video

`Video` accepts captured video, a local video component handle, a remote video
handle, or null. Null renders no DOM. It supports HTML video attributes except
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

**CaptureResult** exposes readonly `videoTrack`, `audioTrack`, `video`, `audio`,
`state` (`idle`, `requesting`, `active`, `error`), `error`, plus `request()` and
`stop()`. Observed `video`/`audio` records are null without that live source:

- `device`: actual typed camera/microphone, or null when identity is unknown.
  Display selection is not fabricated as a camera/microphone.
- `settings`: only actual track-reported values of the corresponding preferences.
  Unknown fields are absent, never copied from the request. These are acquisition
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
`remote.participant(externalId)` returns a stable participant with `externalId`,
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

```tsx
const Chat = agent.topic<{text: string}>("chat", {mode: "reliable"});
// Render a consumer; call Chat.publish({text}) for a discrete send.
<Chat onMessage={(value, {publisherId}) => accept(value, publisherId)} />;
```

The snippet's `accept` is an application callback. Names contain 1 to 85 ASCII
letters, digits, underscores, or hyphens. Lookup caches identity but creates no
publisher/subscriber interest. Generic T does not validate runtime data.

Each committed component owns an independent consumer. `onMessage(value,
{publisherId})` carries actual publisher identity. Optional `onGap` receives core
recovery-gap fields including publisher, stream, expected sequence, and available
sequence. Optional `onError` receives `{code: "decode-failed", topic, cause}` or
`{code: "channel-failed", event}` with structured core direction/mode attribution.
Missing optional callbacks do not suppress diagnostics. Callback changes do not
resubscribe and only committed callbacks run.

Invalid UTF-8/JSON terminates and releases the affected consumer; remount retries.
Other owners remain independent. Core gaps/channel failures retain core recovery
semantics rather than starting an application subscription loop.

`Chat.publish(value): Promise<void>` works with no mounted consumers. It retains
JSON serialization and the 65,536-byte UTF-8 payload limit. Publishing interest
lasts for the Agent lifetime; the last unmounted consumer releases subscription
interest without withdrawing publishing. Success means send admission, not delivery
acknowledgment. Reliable means live ordered recovery, not durable history;
unreliable retains latest-value/coalescing behavior. Core owns restoration,
bounds, and retry policy.
