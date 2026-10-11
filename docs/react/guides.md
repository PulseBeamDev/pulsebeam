---
title: React integration guides
---

# Integration guides

These recipes extend the complete [quickstart](/react/quickstart). Use
`useAgent` for React ownership, and check its initial null before accessing it.
Contracts and related types are grouped in the [reference](/react/reference).

## Pre-join preview

Call `useUserMedia({video: true, audio: true})` in a mounted component.
A user gesture calls `capture.request()`; render
`<Video source={capture.videoTrack} mirror />` without creating or connecting
an Agent. On join, lend the capture to local handles with `setSource`.
Unmounting that capture component stops the devices, so keep it mounted if
you want the same capture to survive the transition into the room.

A captured-source preview shows the raw capture even if a local publication is
disabled. Use `<Video source={cameraHandle} />` when the preview should blank
with `cameraHandle.setEnabled(false)`.

## Device selection and replacement

`useMediaDevices()` returns reactive cameras, microphones, and speakers.
Each has `id` and `label`; browser privacy can leave these empty until
permission. Check `state` and `error` before presenting choices.

Pass the selected IDs in `useUserMedia` options, for example
`{video: {deviceId: {exact: cameraId}}, audio: true}`.
After an initial explicit request, changing options requests replacement.
The old capture remains available until success, then is stopped.
A failed replacement retains the old active tracks and reports `error`;
do not equate every error with absent capture. Bind source changes using an
effect as in the quickstart. Relax an exact device constraint or let the user
select again if a device disappears. Device enumeration does not switch audio
output: there is no React speaker-selection API.

The quickstart's source-binding effect needs no teardown: new capture replaces
its source, stopped capture updates it to null, and `useAgent` closes all local
handles on unmount or Agent replacement. Do not call `setSource` on a handle
in cleanup after its owning Agent has closed.

## Enablement, withdrawal, and release

Keep stable labels rather than inventing a new one for each capture:
`agent.local.video("camera")` and `agent.local.audio("microphone")`.

| User action | API | Observable result |
| --- | --- | --- |
| Disable camera/microphone | `handle.setEnabled(false)` | Black video/silent audio, same publication; capture stays active |
| Re-enable | `handle.setEnabled(true)` | Current borrowed source sends again |
| Withdraw publication | `handle.setSource(null)` | Catalog publication removed; capture still active |
| Release devices | `capture.stop()` and update/clear sources | Native tracks stop; hook returns idle |
| Leave | `agent.disconnect()` plus capture stop if intended | Connection intent stops; borrowed capture otherwise survives |

Enablement can be set before capture arrives and persists across replacement
and detachment. Labels reserve capacity for the Agent lifetime; withdrawing a
track does not free its slot. Catch `LocalTrackCapacityError` if user-driven
labels might exceed topology rather than continually allocating new labels.

## Screen sharing

Use a separate `useDisplayMedia({video: true, audio: false})` hook.
Call `request()` directly from a Share screen gesture and catch its rejection
(for example `void share.request().catch(reportError)`), then bind its
`videoTrack` to `agent.local.video("screen")`. Reserve two local video slots
if camera and screen coexist. Render screen preview without mirroring.

Display-media option changes do not automatically open the chooser. Explicitly
request again from a gesture. Cancellation is not necessarily a fatal error;
inspect `MediaCaptureError.code`. Browser UI ending the shared video returns
capture to idle and stops the session. Display audio availability depends on
the browser and selected surface; `audio: true` does not guarantee an audio
track. Stop or unmount the display hook to release it, and clear its publication.

## Remote discovery and video receive policy

Use `agent.remote.participants`, `videoTracks`, and `audioTracks` for
current discovery; they exclude self. Key video elements by external
participant ID plus label, not a transient publication ID.

For a known identity, `agent.remote.participant("alice").video("camera")`
works before discovery and survives departure/republication.
Lookup alone neither discovers media nor activates demand.

`track.setReceiveOptions({minHeight: 180, minFps: 15, priority: 100})`
replaces its receive policy. Mount a visibly sized `Video` to request media.
Visible element size sets demand in physical pixels, with the maximum shared
across presentations of one track. Unmount releases demand; hidden/off-screen
elements do not request receivers even with `minHeight`.
If visible tracks exceed `remoteVideos`, excess tracks remain unmapped and
warn rather than growing capacity. Increase topology intentionally (replacing
the Agent) or show fewer tracks.

`autoPlay={false}` with native `controls` disables automatic video play,
not attachment or receive demand. For automatic play, retain
`onPlaybackError`'s `retry` and call it from a button gesture.
Changing receive policy is not an autoplay recovery mechanism.

## Remote audio and autoplay

Connected Agents select and play remote audio automatically without an
`Audio` component, audio element, or waveform read.
Use `agent.remote.resumeAudio()` directly inside a click handler to recover
a blocked AudioContext. It can unlock before media arrives, resolves only when
the context is running, and rejects on recovery failure or close.
Automatic playback failure is logged but does not terminate the Agent.
Surface rejected recovery promises to the user; do not install a timer loop.

A `RemoteAudioTrack` exposes `receiving`, meaning it is bound by Mapping,
not that the user can hear it. Subscribe for an isolated indicator with
React's `useSyncExternalStore((listener) => track.subscribe(listener), () => track.receiving, () => false)`.
Waveform and spectrum reads are imperative animation/DSP operations and do not
trigger renders.

## Connection, renewal, and errors

`connect()` and `disconnect()` synchronously change desired state.
Render `getSnapshot().connection`, `failure`, and admitted identities.
A transient retry may pass through `reconnecting` and
`retry-waiting:N`; the runtime owns retries while intent is connected.
`reconnect()` preserves publication, receive, and topic intent.
Disconnect clears admission identity and remote output, but does not stop
borrowed capture. Close is terminal.

Fetch renewed credentials from your authenticated backend before the
snapshot's `authorizationExpiresAt`, allowing for request latency.
Update the config token or call `renewAuthorization(token)` for low-level
ownership. Renewal does not replace endpoint or session identity. The browser
never holds the signing secret. The SDK does not schedule a backend refresh.
Use a new Agent to change room/participant. Diagnose authorization,
configuration, protocol, initialization, and retry-exhaustion failures instead
of assuming reconnect will repair them.

Use capture's `error` and rejected `request()` promises for acquisition;
video's `onPlaybackError` for play failures; rejected `resumeAudio()` for
audio recovery; snapshots and `subscribeEvents` for Agent failures and topic
diagnostics. Subscription cleanup returns an idempotent unsubscribe function.
See [troubleshooting](/react/troubleshooting).

## Typed topics

`agent.topic<{text: string}>("chat", {mode: "reliable"})` gives JSON
`publish(value): Promise<void>` and `subscribe({signal})` async iteration.
Constructing a topic does not subscribe. A React effect can start iteration and
abort it on cleanup:

```tsx
useEffect(() => {
  if (!agent) return;
  const controller = new AbortController();
  const chat = agent.topic<{ text: string }>("chat", { mode: "reliable" });
  void (async () => {
    for await (const message of chat.subscribe({ signal: controller.signal })) {
      setMessages((previous) => [...previous, message.text]);
    }
  })().catch(reportError);
  return () => controller.abort();
}, [agent]);
```

Publish from an application action and catch its rejection:
`void chat.publish({text: "hello"}).catch(reportError)`.
The generic type is a compile-time contract, not schema validation or
authorization. Validate untrusted application payloads at consumption.

- **reliable** maps to ordered delivery, with a bounded per-subscription queue.
- **unreliable** maps to latest-value delivery; queued values can be replaced.
- UTF-8 JSON payloads above 65,536 bytes reject before sending. Non-JSON values
  can fail serialization; this is not an arbitrary binary data-channel API.
- Abort, iterator return, or breaking iteration releases that subscription.
  Each subscriber has its own queue.
- Send admission is not a delivery acknowledgment. Reconnection is not durable
  replay: gaps, dropped sends, resynchronization, and channel failures remain
  possible. Inspect `snapshot.topics` and `AgentEvent` diagnostics and rebuild
  authoritative application state through your backend when necessary.
- Desired interest survives connection recovery, but neither mode guarantees
  application-level exactly-once processing, permanent history, or delivery to
  offline participants.

Do not mix low-level whole-state topic registrations with hook-oriented logic
without understanding [complete desired state](/react/reference#low-level-agent-state).
