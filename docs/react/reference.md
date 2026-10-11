---
title: React SDK API reference
---

# API reference

Import runtime values and types from `@pulsebeam/react`. This reference groups
all public exports by contract. Follow the [quickstart](/react/quickstart) for a
complete application, [guides](/react/guides) for recipes, and
[troubleshooting](/react/troubleshooting) for recovery.

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

There is no provider, `Audio` component, raw attachment hook, or React export
for constructing capture from a native track. Use hook capture and source
handles with `Video`. The internal Web dependency is prepared by the package
build, not imported directly by application examples.

## useAgent

`useAgent(config: AgentConfig): Agent | null` creates an independent Agent after
mount and closes it on cleanup. It returns null until ready and during keyed
replacement. It subscribes the component to snapshot updates. It does not connect
or acquire capture automatically. Token-only changes call `renewAuthorization`
on the existing Agent. Endpoint/topology/logging changes replace it.

### Configuration types

- **AgentConfig:** required absolute HTTP(S) `endpoint`, opaque bearer `token`,
  and `topology`; optional `logging`. Use the server base URL, not a native API
  path: the runtime adds `/api/v1/native`.
- **MediaTopology:** optional counts `localVideos` (default 2), `localAudios`
  (2), `remoteVideos` (16), `remoteAudios` (8). Directions support at most 32
  media sections. Capacity is fixed for the Agent lifetime.
- **AgentLogging / LogLevel:** optional `level`, default `warn`; levels are
  `off`, `error`, `warn`, `info`, `debug`, `trace`. Console logging is scoped to
  each Agent. Chrome needs Verbose enabled for debug/trace.

Configuration and runtime initialization failures appear through Agent failure
snapshots/events. Browser assets and APIs must be available. Connection intent
is not admission. See [connection diagnostics](/react/troubleshooting#admission-and-connection).

## createAgent

`createAgent(config: AgentConfig): Agent` is synchronous and may return while
its private WASM initialization is pending. It starts disconnected, retaining
the latest complete desired state during initialization. This is a supported
low-level ownership path, not the recommended React lifetime binding.

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
| `close(): void` | Terminal, idempotent teardown |

Admission supplies authoritative external IDs and expiry. Renew before expiry
with backend-issued credentials for the existing session. A different room or
participant needs a new Agent. See [renewal](/react/guides#connection-renewal-and-errors).
The additional low-level methods are documented [below](#low-level-agent-state).

## Local media and captured sources

**LocalMedia** exposes `video(label): LocalVideoTrack` and
`audio(label): LocalAudioTrack`. Labels are stable per-kind names and bind
reserved slots (`v0`, `a0`, etc.). Repeated lookup returns the same handle.
Clearing a source does not release labels or capacity.

**LocalVideoTrack / LocalAudioTrack** expose readonly `kind`, `label`, `source`,
`enabled`, plus `setSource(source | null)`, `setEnabled(boolean)`, and
`subscribe(listener): unsubscribe`. Enablement starts true and persists across
source replacement/withdrawal. False retains publication while sending black
video/silent audio. A disabled local-handle video preview blanks.
`setSource(null)` withdraws publication; neither it nor Agent teardown stops
borrowed native capture. Kind/capture mismatches fail validation. Capture sources
from another hook can be borrowed; SDK sender clones isolate mute state.

**CapturedVideoTrack / CapturedAudioTrack** are typed opaque sources returned
by capture hooks. They expose `kind` but not a public native-track accessor in
React. Pass them to matching local handles or a captured video to `Video`.

## Video

`Video(props: VideoProps)` renders a video element, or null for a null source.
`source` is required and accepts a captured video source, local video handle,
remote video handle, or null. Defaults: `autoPlay=true`, `muted=true`,
`playsInline=true`, `mirror=false`. Other native video attributes are supported
except `src` and `children`; `className`, `style`, and `controls` are useful.

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

`useUserMedia(options: UserMediaOptions): CaptureResult` and
`useDisplayMedia(options: DisplayMediaOptions): CaptureResult` require both
`video` and `audio`, each boolean or browser `MediaTrackConstraints`.
No capture happens until `request(): Promise<void>` is explicitly called.
Invoke it from a gesture; display choosers especially require transient activation.

**CaptureResult:** readonly `videoTrack`/`audioTrack` (typed sources or null),
`state` (`idle`, `requesting`, `active`, `error`), `error` (nullable
`MediaCaptureError`), `request()`, and `stop(): void`.
`request` rejects on failure or supersession; observe/catch it. `stop` and unmount
stop native tracks and fence pending acquisitions; late tracks are stopped.
User-media option changes after requesting trigger replacement. The old session
remains until success, including when replacement fails (`active` with an error).
Display option changes never open a chooser automatically. Ending the display
video stops that session. Browser display audio is not guaranteed.

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
`receiving` follows Mapping, not sound level or autoplay. Reads synchronously
fill normalized amplitudes or decibel frequency bins using the default 2048-point
FFT and smoothing; smaller buffers get leading samples/bins, not a new FFT size.
Unmapped/unavailable signal and oversized tails are zero waveform or negative
infinity spectrum. Empty buffers are valid. Reads do not notify subscribers.

Automatic audio selection/playback is enabled while connected. It does not
require UI or analysis reads. `resumeAudio()` is exceptional autoplay recovery:
call from a gesture, even before media arrives; it resolves when the context is
running and rejects on recovery failure or close. Automatic failures are logged
without changing Mapping or terminating the Agent. Disconnect removes routes;
reconnect restores receivers without duplicate output. Close makes existing
handles inactive/nonreceiving and analysis silent.

## Snapshots, status, and errors

**AgentSnapshot** separates desired progress and observed state:

- `version`, `desiredRevision`, `generation`, and `connection: ConnectionState`.
- `participantId` is internal; `participantExternalId` and `roomExternalId`
  are authoritative admitted application IDs, null before admission, retained
  during same-session reconnection, cleared on disconnect.
- `authorizationExpiresAt` is a nullable Unix-seconds authorization deadline.
- `catalog: CatalogSnapshot`, `mapping: MappingSnapshot`, `tracks`, `topics`,
  and nullable `failure: AgentFailure`.

**ConnectionState** includes `disconnected`, `initializing`, `creating-offer`,
`joining`, `applying-answer`, `waiting-for-transport`, `waiting-for-signaling`,
`connected`, `reconnecting`, `retry-waiting:N`, `closing`, `terminal-failure`.
Do not infer admission from intent or an intermediate transport state.

**CatalogSnapshot** has revision, participants, publications. **Participant**
has internal `id` and application `externalId`; **Publication** has `id`,
canonical `participantId`, `kind: MediaKind` (`audio`/`video`), and `label`.
**MappingSnapshot** has `acceptedIntentRevision` and audio/video **TrackMapping**
entries (`receiverIndex`, `publicationId`). Discovery does not imply Mapping.
`tracks` is a publication-ID-keyed record of **RemoteTrack**, a union of
**MappedRemoteVideoTrack / MappedRemoteAudioTrack** with `kind`, `publicationId`,
canonical `participantId`, and native `media: MediaStreamTrack`. These are
low-level observations, not stable logical handles. Snapshot containers are
immutable/stable until observable changes; platform tracks are not frozen.

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

Reliable maps to ordered **TopicMode**; unreliable maps to latest. Latest
replaces queued values; ordered retains at most 256 queued values, dropping
oldest on overflow. Admission is not delivery acknowledgment. Neither provides
arbitrary channels, persistent history, exactly-once semantics, or guaranteed
replay to offline subscribers. Desired interest survives reconnection, not
application message durability. See [topic recovery](/react/guides#typed-topics).

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

## Low-level Agent state

These methods are supported but need explicit ownership and full-state reasoning.
Do not substitute low-level native slots for the recommended React capture flow.

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
