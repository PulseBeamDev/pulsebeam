---
title: React SDK API reference
---

# API reference

Import from `@pulsebeam/react`. Pick an API below.
Need a working app first? [Install the SDK and add a room](/react/quickstart).

## Runtime export index

| Export | Contract |
| --- | --- |
| `useAgent` | [React-owned Agent](#useagent) |
| `createAgent` | [Caller-owned Agent](#createagent) |
| `Video` | [Video presentation](#video) |
| `useUserMedia` | [Camera/microphone capture](#capture-hooks) |
| `useDisplayMedia` | [Display capture](#capture-hooks) |
| `useMediaDevices` | [Device enumeration](#usemediadevices) |
| `MediaCaptureError` | [Acquisition errors](#errors) |
| `LocalTrackCapacityError` | [Local slot exhaustion](#errors) |

## useAgent

```ts
useAgent(config: AgentConfig): Agent
```

Returns an Agent handle from the first render and subscribes the component to
its snapshots. The handle is inert during rendering; React activates its runtime
after commit. It does not connect or start capture automatically.

| Lifecycle | Result |
| --- | --- |
| First render | Returns a handle (no loading-time `null`) |
| Token-only change | Renews authorization on the existing Agent |
| Endpoint/topology change | Replaces the Agent with a new inert handle |
| Logging change | Updates the existing Agent without replacement |
| React key changes | Remounts the component and creates a new session |
| Unmount | Releases Agent runtime resources; does not stop caller-owned capture |

### Configuration types

**AgentConfig**

| Field | Input |
| --- | --- |
| `endpoint` | Required absolute HTTP(S) base URL; the runtime adds `/api/v1/native` |
| `token` | Required opaque bearer token |
| `topology` | Optional `MediaTopology`; omitted values use the defaults below |
| `logging` | Optional `AgentLogging` |

**MediaTopology** fields are optional. The SDK reserves these outgoing and
incoming **track slots** when an Agent is created; they are not participant
limits. Capacity stays fixed for that Agent's lifetime; each individual
field is capped at 32 slots (local video, local audio, remote video, and
remote audio).

| Field | Default |
| --- | --- |
| `localVideos` | 2 |
| `localAudios` | 2 |
| `remoteVideos` | 16 |
| `remoteAudios` | 8 |

Local publication labels permanently claim a slot for the lifetime of the
Agent, even after withdrawal. Creating more local labels than the reserved
slots throws `LocalTrackCapacityError`; remote media beyond reserved receiver
slots cannot be assigned. Set only the fields you need, for example
`topology: { remoteVideos: 4 }`; unspecified fields retain their defaults.
**Today, increasing any capacity means replacing the Agent and reconnecting.**
Automatic resizing is planned, not implemented.

**AgentLogging / LogLevel:** optional `level`, default `warn`.
Supported levels: `off`, `error`, `warn`, `info`, `debug`, `trace`.
Console logging is scoped to each Agent and can be changed in place via
`agent.setLogging(level)`. Chrome needs Verbose enabled for debug/trace.

Configuration and runtime initialization failures appear through Agent failure
snapshots/events. Browser assets and APIs must be available. Connection intent
is not admission. See [connection diagnostics](/react/troubleshooting#admission-and-connection).

## createAgent

```ts
createAgent(config: AgentConfig): Agent
```

Returns a caller-owned Agent synchronously. It starts disconnected and retains
the latest desired state while initializing. Prefer `useAgent` for React ownership.

The caller owns `subscribe`/`subscribeEvents` unsubscribe functions and
`close()`. Unsubscribe and close are idempotent; close is terminal, fences
pending work, detaches callbacks and releases runtime/browser resources.
It does not stop borrowed capture. Do not create Agents on every render or
execute browser runtime initialization in SSR.

## Agent interface

| Member | Result and ownership |
| --- | --- |
| `local: LocalMedia` | Stable local audio/video handles by label |
| `remote: RemoteMedia` | Logical discovery, presentation handles, audio recovery |
| `topic<T>(name, {mode})` | Typed JSON facade, see [topics](#topics) |
| `connect(): void` | Synchronously sets desired connection true, does not await admission |
| `disconnect(): void` | Sets intent false and stops retrying; capture remains caller-owned |
| `reconnect(): void` | Requests runtime reconnection, retains desired state |
| `renewAuthorization(token): void` | Updates bearer authorization for this session, does not fetch a token |
| `getSnapshot(): AgentSnapshot` | Current immutable observable snapshot |
| `subscribe(listener): () => void` | Snapshot notifications; caller cleans up unless using `useAgent` |
| `subscribeEvents(listener): () => void` | Diagnostics and topic events; caller cleans up |
| `setLogging(level): void` | Update diagnostic verbosity without restarting the session |
| `close(): void` | Terminal, idempotent teardown |

Admission supplies authoritative external IDs and expiry. Renew before expiry
with backend-issued credentials for the existing session. A different room or
participant needs a new Agent. See [renewal](/react/guides#connection-renewal-and-errors).
The additional low-level methods are documented [below](#low-level-agent-state).

## Local media and captured sources

**LocalMedia:** `video(label): LocalVideoTrack`, `audio(label): LocalAudioTrack`.
Repeated per-kind label lookup returns the same handle and reserved slot.
Withdrawal does not free that slot.

**LocalVideoTrack / LocalAudioTrack**

| Member | Contract |
| --- | --- |
| readonly `kind`, `label`, `source`, `enabled` | Current logical handle; enabled starts true |
| `setSource(source \| null)` | Borrow matching capture or withdraw; never stops original tracks |
| `setEnabled(boolean)` | False sends black/silent media and blanks local-handle video; publication remains |
| `subscribe(listener): unsubscribe` | Observe changes; caller cleans up |

Enablement survives replacement/withdrawal. Kind/capture mismatches fail validation.
Agent teardown does not stop capture; sender clones isolate mute state.

**CapturedVideoTrack / CapturedAudioTrack:** opaque hook-owned sources with `kind`,
not a public native-track accessor. Pass them to matching local handles or a
captured video to `Video`; sources from another hook can be borrowed.

## Video

`Video(props: VideoProps)` renders a video element, or null when `source` is null.

| Prop | Input / default |
| --- | --- |
| `source` | Required captured video source, local video handle, remote video handle, or null |
| `autoPlay` | `true` |
| `muted` | `true` |
| `playsInline` | `true` |
| `mirror` | `false`; visual transform only |
| `onPlaybackError` | Optional callback receiving `PlaybackError` |

Other native video attributes are supported except `src` and `children`.
Use `className`, `style`, or `controls` as needed.

For remote video it attaches presentation-driven receive demand. Visible size
in physical pixels determines demand, shared by maximum size when multiple
elements present one handle. Unmount/source changes detach; hidden/off-screen
presentation does not reserve receivers. `autoPlay=false` still attaches and
requests media, but native controls/gesture must start playback.

For captured/local sources it binds a preview without taking capture ownership.
`mirror` is a visual transform only. A local-handle source follows enablement;
a captured-source preview does not reflect publication enablement.

**PlaybackError:** `onPlaybackError` receives `{error: unknown, retry:
() => Promise<void>}` when automatic play fails. Store the retry and call it
from a user gesture; catch rejection. It is not a general connection retry.

## Capture hooks

```ts
useUserMedia(options: UserMediaOptions): CaptureResult
useDisplayMedia(options: DisplayMediaOptions): CaptureResult
```

Both option types require `video` and `audio`, each a boolean or browser
`MediaTrackConstraints`. For camera and microphone, use
`useUserMedia({video: true, audio: true})`.

No capture starts until `request()` is called. Invoke it from a gesture;
display choosers especially require transient activation.

**CaptureResult**

| Member | Contract |
| --- | --- |
| `videoTrack` / `audioTrack` | Readonly typed sources or null |
| `state` | Readonly `idle`, `requesting`, `active`, or `error` |
| `error` | Readonly `MediaCaptureError` or null |
| `request(): Promise<void>` | Rejects on failure or supersession; catch rejection |
| `stop(): void` | Stops native tracks and returns the hook to idle |

Stopping or unmounting also invalidates pending acquisitions; late tracks are stopped.

| Hook | Replacement and ending behavior |
| --- | --- |
| `useUserMedia` | Option changes after requesting trigger replacement. Old capture stays until success; failure retains it with `active` and an error. |
| `useDisplayMedia` | Option changes never open a chooser automatically. Ending the shared video stops the session. Display audio is not guaranteed. |

## useMediaDevices

`useMediaDevices(): MediaDevicesResult` enumerates after mount, refreshes after
capture and device changes, and removes its listeners on unmount.
**MediaDevicesResult** contains readonly `cameras`, `microphones`, `speakers`,
`state` (`loading`, `ready`, `error`), and nullable `error`.
**MediaDevice** has readonly `id` and `label`, possibly empty until permission.
This is enumeration, not a speaker-output routing API.

## Remote media

**RemoteMedia** exposes readonly `participants: RemoteParticipant[]`,
`videoTracks: RemoteVideoTrack[]`, `audioTracks: RemoteAudioTrack[]`,
`participant(externalId)`, and `resumeAudio(): Promise<void>`.
Collections are readonly and preserve identity until membership changes; they
exclude self. **RemoteParticipant** has `externalId`, per-participant collections,
and stable `video(label)` / `audio(label)` lookups. Lookup works before discovery.
Handles survive temporary absence, unpublication, departure/rejoin, remapping,
and publication-ID replacement. Lookup after close fails.

**RemoteVideoTrack:** `kind`, `participantId` (external application identity),
`label`, `active` (catalog publication presence, not playback),
`setReceiveOptions(ReceiveOptions)`, and `subscribe(listener): unsubscribe`.
Policy replacement alone never requests hidden/unmounted video.
**ReceiveOptions:** optional `minHeight`, `minFps`, `priority`, and
`playoutDelay: {minMs, maxMs}`. Defaults are 0, 0, 0, and no explicit delay;
numeric values must be finite uint32 integers or the setter throws `TypeError`.
Use a maximum delay not below the minimum; invalid runtime policy is reported
through Agent diagnostics. Policy survives temporary absence.
Excess visible tracks beyond topology stay unmapped and warn.

**RemoteAudioTrack:** `kind`, external `participantId`, `label`, `receiving`,
`subscribe`, `readWaveform(Float32Array)`, `readSpectrum(Float32Array)`.
`receiving` follows receiver assignment (Mapping), not sound or autoplay.

::: details Waveform and spectrum reads
Reads synchronously fill normalized amplitudes or decibel frequency bins using
the default 2048-point FFT and smoothing. Smaller buffers get leading samples/bins,
not a new FFT size. Unmapped/unavailable signal and oversized tails are zero waveform
or negative infinity spectrum. Empty buffers are valid; reads do not notify subscribers.
:::

Automatic audio selection/playback is enabled while connected. It does not
require UI or analysis reads. `resumeAudio()` is exceptional autoplay recovery:
call from a gesture, even before media arrives; it resolves when the context is
running and rejects on recovery failure or close. Automatic failures are logged
without changing Mapping or terminating the Agent. Disconnect removes routes;
reconnect restores receivers without duplicate output. Close makes existing
handles inactive/nonreceiving and analysis silent.

## Snapshots, status, and errors

**AgentSnapshot** separates requested state from observed results.
Admission means the server accepted the participant into the room.

| Field | Meaning |
| --- | --- |
| `connection: ConnectionState` | Current connection progress/status |
| `participantExternalId`, `roomExternalId` | Authoritative admitted app IDs; null before admission, retained during same-session reconnection, cleared on disconnect |
| `participantId` | Opaque internal (canonical) participant ID, not the app's identity |
| `authorizationExpiresAt` | Nullable authorization deadline in Unix seconds |
| `catalog: CatalogSnapshot` | Discovered participants and publications |
| `mapping: MappingSnapshot` | Publications assigned to receiver slots |
| `tracks` | Publication-ID-keyed low-level media observations |
| `topics: TopicSnapshot` | Topic transport status and counters |
| `failure: AgentFailure \| null` | Failure class and message |
| `version`, `desiredRevision`, `generation` | Snapshot version, desired-state revision, and runtime generation for diagnostics |

**ConnectionState** includes `disconnected`, `initializing`, `creating-offer`,
`joining`, `applying-answer`, `waiting-for-transport`, `waiting-for-signaling`,
`connected`, `reconnecting`, `retry-waiting:N`, `closing`, `terminal-failure`.
Do not infer admission from intent or an intermediate transport state.

::: details Catalog, Mapping, and native-track fields
**CatalogSnapshot** has revision, participants, publications. **Participant**
has internal `id` and application `externalId`; **Publication** has `id`,
canonical `participantId`, `kind: MediaKind` (`audio`/`video`), and `label`.
**MappingSnapshot** has `acceptedIntentRevision` (the accepted desired-state
revision) and audio/video **TrackMapping** entries. Each entry pairs a
`receiverIndex` (a reserved receiver slot) with a `publicationId`.
Discovery does not imply receiver assignment, called Mapping.
`tracks` is a publication-ID-keyed record of **RemoteTrack**, a union of
**MappedRemoteVideoTrack / MappedRemoteAudioTrack** with `kind`, `publicationId`,
canonical `participantId`, and native `media: MediaStreamTrack`. These are
low-level observations, not stable logical handles. Snapshot containers are
immutable/stable until observable changes; platform tracks are not frozen.
:::

### Errors

**AgentFailure** has `class: FailureClass` and `message`. Classes:
`initialization`, `invalid-configuration`, `authorization`, `protocol`,
`transient`, `resource-expired`, `retry-exhausted`, `validation`, `runtime`.
Use connection status and class together; not every failure is terminal or
repairable by retry. [Troubleshooting](/react/troubleshooting) describes diagnosis.

**MediaCaptureError extends Error:** `code: MediaCaptureErrorCode`, message,
optional `kind` and `cause`. Codes are `permission-denied`, `device-not-found`,
`device-busy`, `constraint-unsatisfied`, `request-cancelled`, `not-supported`,
`unknown`. Inspect these with capture status, especially retained replacement.

**LocalTrackCapacityError extends Error:** readonly `kind`, `label`, `capacity`;
thrown synchronously when allocating a new label exceeds reserved local slots.
Choose stable labels or a sufficient topology rather than repeatedly withdrawing
and reallocating.

## Topics

`topic<T>(name, {mode: "reliable" | "unreliable"}): Topic<T>` creates a cached
facade. Names use 1 to 85 ASCII letters, digits, underscores, or hyphens.
Invalid names/modes and lookup after close throw. Construction does not register
interest. `publish(value): Promise<void>` registers publishing and encodes UTF-8
JSON; serialization, size above 65,536 bytes, closed Agent, or send failure rejects.
Publishing interest remains for the Agent lifetime once activated.
`subscribe({signal?}): AsyncIterable<T>` owns independent bounded buffering.
Abort, iterator return, or ending iteration releases interest; an already-aborted
signal yields no values. Malformed incoming JSON rejects iteration.

| Facade mode | Internal `TopicMode` | Per-subscription queue |
| --- | --- | --- |
| `reliable` | `ordered` | At most 256 values; drops oldest on overflow |
| `unreliable` | `latest` | New queued values replace old ones |

Send admission is not delivery acknowledgment. Neither mode provides arbitrary
channels, persistent history, exactly-once semantics, or guaranteed offline
replay. Desired interest survives reconnection; messages are not durable. See [topic recovery](/react/guides#typed-topics).

::: details Topic status and events
**TopicSnapshot** contains publishers/subscribers and counters for accepted
and dropped sends, delivered messages, resynchronizations, channel failures.
**TopicPublisherStatus** includes name/mode, connected, streamId/nextSequence,
queued, sendPending. **TopicSubscriberStatus** includes name/mode, publisherId,
connected, publisher count and buffered count. Use these for transport diagnostics,
not application delivery confirmation.

**AgentEvent** covers state-change, failure, topic-message (bytes and publisher;
ordered messages also stream/sequence), topic-send-admitted (operation),
topic-send-dropped, topic-recovery-gap, topic-resynchronized, and
topic-channel-failed. **TopicDropReason** includes invalid-payload, not-registered,
channel-unavailable, queue-full, superseded, host-rejected, channel-closed,
transport-replaced, sequence-exhausted. Clean up event subscriptions explicitly.
:::

## Low-level Agent state

Prefer logical handles and capture hooks. Open this only to manage complete
Agent state or native slots yourself.

:::: details Native state, slots, and sender configuration
A slot is a sender or receiver position reserved by topology.

::: warning Complete-state replacement
Omitted collections in `setState` retract previous low-level values. Do not
mix native slot ownership with logical source ownership for the same slot.
:::

- `setState(state: AgentState): void` replaces complete desired state.
  `connected` is required. Omitted publication, video, audio-pinning, and topic
  collections are empty and retract previous low-level values. Logical handle
  publications and mounted presentation remain managed by the facade.
- **PublicationIntent:** slot, label, active. Labels bind to slots on first use.
- **VideoDemand:** receiver slot, height, minHeight, minFps, priority, optional
  `FixedPlayoutDelay`, and either internal trackId or **TrackSelector**
  (`participantExternalId`, label). Missing selectors remain desired until
  discovered.
- **AudioDemand:** pinned IDs, selected selectors, automatic selection flag,
  per-track playoutDelays or selectorDelays. `automatic: false` opts out of
  automatic selection, not playback of explicitly selected/pinned mapped audio.
- **FixedPlayoutDelay:** mode `fixed`, minMs/maxMs. It belongs to an explicitly
  requested track, not connection-wide automatic audio.
- **TopicRegistration:** name, `TopicMode` (`latest`/`ordered`), optional publish,
  subscribe, publisherId. `sendTopic(name, mode, Uint8Array): void` is lower-level
  byte sending with event diagnostics, not a delivery acknowledgment.
- `replaceLocalTrack(slot, MediaStreamTrack | null, SenderConfig): Promise<void>`
  attaches/detaches caller-owned native tracks for reserved slots. Validate kind
  and settings and catch rejection; detaching ignores sender settings. Replacement
  and close never stop the borrowed original track. Do not race it with logical
  source ownership for the same slot.
- `setLocalMuted(slot, boolean): Promise<void>` controls the reserved sender;
  catch runtime/validation failure. Prefer logical `setEnabled` for React capture.

**SenderConfig** is **VideoSenderConfig | AudioSenderConfig**. Video contentHint
is motion/detail/text; audio is speech/music. Optional degradationPreference is
maintain-framerate/maintain-resolution/balanced. Omitted or empty encodings use
runtime defaults (three video layers, one audio layer); explicit arrays contain
exactly three or one **SenderEncoding** entries respectively.
Encoding fields include required active, optional rid, scaleResolutionDownBy,
maxBitrate, maxFramerate, scalabilityMode (L1T1/L1T2/L1T3), and dtx
(enabled/disabled). Invalid kinds/configuration fail validation. These settings
are not browser-independent codec guarantees.
::::
