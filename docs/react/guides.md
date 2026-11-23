---
title: React guides
description: Add capture, publication, playback, data, renewal, and recovery to a PulseBeam React application.
---

# Add one feature

These guides build on the [quickstart](/react/quickstart). Look up logical
tracks directly from the Agent. Labels should remain stable; repeating a lookup
during rendering does not acquire resources or replace a committed owner.

## Pre-join preview

`useCapture("device", options)` owns browser capture independently of the Agent.
Call `request()` from a user gesture and preview the returned source directly.

```tsx
const capture = useCapture("device", { video: true, audio: true });

<button onClick={() => void capture.request()}>Enable devices</button>
<Video source={capture.video} mirror />
```

`capture.stop()` ends the owned browser tracks. Disconnecting the Agent does not
stop capture.

## Device selection and replacement

Call `useDevices()` to enumerate typed camera and microphone devices. Pass a
selected device into `useCapture`. Changing capture options replaces the owned
capture session; publication follows the new captured source without replacing
the publication owner.

```tsx
const devices = useDevices();
const capture = useCapture("device", {
  video: { device: selectedCamera, width: 1280, height: 720 },
  audio: { device: selectedMicrophone, echoCancellation: true },
});
```

Device labels may be empty until the user grants permission. Treat
`devices.error` and `capture.error` as user-facing failures.

## Enablement, withdrawal, and release

Look up a local handle, then call `usePublish` to own publication.

```tsx
const publication = usePublish(agent.video("camera"), capture.video, {
  enabled: cameraOn,
});
```

`enabled: false` disables media while retaining ownership. A null source keeps
the declaration without a source; unmounting the hook owner releases it. The capture
remains owned by `useCapture` until `stop()` or unmount. Lookup alone has no
publication side effect.

## Screen sharing

Display capture uses the same hook with a different source kind.

```tsx
const display = useCapture("display", {
  video: { width: 1920, height: 1080, frameRate: 30 },
  audio: false,
});

usePublish(agent.video("screen"), display.video);

<button onClick={() => void display.request().catch(report)}>Share screen</button>
```

Call `display.stop()` to end sharing. Browser-native cancellation is exposed
through `display.error`.

## Remote discovery and video receive policy

Own one bounded catalog page. Give the observer Discover permission for the
literal room; that does not grant Subscribe or start media reception. Render
loading, failure and continuation separately. This roster deliberately does not
fetch every page or receive every publication:

<<< ./Roster.tsx

Mount this roster with a key containing `roomName` when changing rooms, so its
continuation resets. A cursor is short-lived and bound to the room and limit.
If a continuation expires, First page creates a fresh first-page watch. For an
initial permission failure, correct authorization and remount the roster rather
than treating failure as an empty page. Old application-retained pages stay
stable; the SDK retains only its current page.

Select publications only from the displayed page. A `Video` metadata entry maps
to `room.participant(info.id()).video(publication.trackId)`; use `.audio` or
`.data` for the other kinds. Skip `info.id() === agent.state().participantId` before
creating receive owners. Presence-only entries still belong in the roster.
Keep each selected handle stable in a mounted child with `useMemo`.

For a selected pinned `remote` video handle, create reception explicitly with
`useSubscribe`. Unmounting `Video` releases only its observation. Unmounting the
component that called `useSubscribe` closes that subscription. Replacing a Watch
page does not close subscriptions held independently outside those UI mounts.

```tsx
const subscription = useSubscribe(remote, {
  visible: true,
  height: 360,
  minHeight: 180,
  priority: pinned ? 200 : 10,
  minFps: 15,
});

<Video
  source={subscription}
  onPlaybackError={(failure) => setVideoFailure(failure)}
/>
```

Track handles follow the participant and track ID across temporary absence and
replacement. Use participant ID and track ID for React keys within the same kind.

## Remote audio and autoplay

Pass an already-owned audio subscription to `Audio`. When autoplay is blocked,
retain the failure and invoke `retry()` from a click or keyboard gesture.

```tsx
const [audioFailure, setAudioFailure] = useState<PlaybackError | null>(null);
const audio = useSubscribe(agent.room(room).participant(peer).audio("microphone"));

<Audio source={audio} onPlaybackError={setAudioFailure} />
{audioFailure && (
  <button onClick={() => void audioFailure.retry()}>Enable remote audio</button>
)}
```

## Connection, renewal, and errors

`join(room)` records the desired room. Read `agent.state()` to distinguish that
intent from the admitted room. Transient transport failures retry while joined
intent remains. After correcting a join denial, retry `join(room)`.

`useAgent` renews authorization when `config.token` changes. Do not also renew
the same token imperatively. Use a new component key for a different participant
identity. To move rooms in the same session, call `join()` with the new room.

```tsx
<Room key={participantId} token={token} />
```

Render `agent.state().status` and `agent.state().error` for user-visible status. Do not put
credentials in URLs, storage, or logs.

## Data tracks

PulseBeam data tracks carry bytes. The application owns its wire format,
validation, versioning, and malformed-payload policy.

```tsx
import { DataEvent } from "@pulsebeam/react";

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

function Chat({ agent }: { agent: Agent }) {
  const publication = usePublish(agent.data("chat"));
  const incoming = agent.room("meeting").participant("alice").data("chat");

  async function send(message: string) {
    if (!publication) return;
    await publication.send(encoder.encode(JSON.stringify(message)));
  }

  return (
    <>
      <button onClick={() => void send("hello").catch(report)}>Send hello</button>
      <Data
      track={incoming}
      onEvent={(event) => {
        if (!DataEvent.Message.instanceOf(event)) return;
        try {
          const value: unknown = JSON.parse(decoder.decode(event.inner.data));
          if (typeof value === "string") showMessage(value);
        } catch {
          // Ignore malformed application payloads without ending the stream.
        }
      }}
      onError={(error) => report(error)}
      />
    </>
  );
}
```

Mounting `Data` owns one subscription and unmounting releases it. The example
uses Reliable delivery. If recovery cannot preserve continuity, only that
subscription ends and `onError` receives the failure.
`onEvent` may return a promise. Each mount awaits its current handler before
taking the next event, using the latest committed callback. Other mounts keep
processing independently. Buffering is bounded, so awaiting does not provide
transport backpressure or guarantee against overload loss. If a handler throws
or rejects, or reception fails, that mount releases its subscription and reports
the error through `onError` (or a diagnostic when absent). Callback changes do
not restart it; remount to restart. Catch processing errors inside `onEvent` to
continue deliberately. Unmount and target changes release receive ownership
without waiting for application work, and suppress late callbacks. Already
running application side effects cannot be cancelled or undone.

Data is live transport, not durable storage. Use application storage when late
joiners need complete history.
