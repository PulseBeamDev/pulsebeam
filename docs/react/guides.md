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

Look up a local handle, then mount `Publish` to own publication.

```tsx
<Publish
  track={agent.video("camera")}
  source={capture.video}
  enabled={cameraOn}
/>
```

`enabled={false}` disables media while retaining ownership. A null source keeps
the declaration without a source; unmounting `Publish` releases it. The capture
remains owned by `useCapture` until `stop()` or unmount. Lookup alone has no
publication side effect.

## Screen sharing

Display capture uses the same hook with a different source kind.

```tsx
const display = useCapture("display", {
  video: { width: 1920, height: 1080, frameRate: 30 },
  audio: false,
});

<button onClick={() => void display.request()}>Share screen</button>
<Publish
  track={agent.video("screen")}
  source={display.video}
  content="detail"
/>
```

Call `display.stop()` to end sharing. Browser-native cancellation is exposed
through `display.error`.

## Remote discovery and video receive policy

Participants and their immutable track arrays are the discovery surface. Narrow
by `kind` when collecting video tracks.

```tsx
const videos = agent.remote.participants.flatMap((participant) =>
  participant.tracks.filter(
    (value): value is RemoteVideoTrack => value.kind === "video",
  ),
);
```

A mounted, visible, nonzero-sized `Video` owns reception of a remote video.
Use `receive` for policy hints.

```tsx
<Video
  source={remote}
  receive={{
    priority: pinned ? 200 : 10,
    minFps: 15,
    playoutDelay: { minMs: 100, maxMs: 200 },
  }}
  onPlaybackError={(failure) => setVideoFailure(failure)}
/>
```

Track handles follow the participant and label across temporary absence and
replacement. Use participant ID plus label for React keys.

## Remote audio and autoplay

Mount one `Audio` owner for the Agent. It follows discovered remote audio tracks.
When autoplay is blocked, retain the failure and invoke `retry()` from a click or
keyboard gesture.

```tsx
const [audioFailure, setAudioFailure] = useState<PlaybackFailure | null>(null);

<Audio agent={agent} onPlaybackError={setAudioFailure} />
{audioFailure && (
  <button onClick={() => void audioFailure.retry()}>Enable remote audio</button>
)}
```

## Connection, renewal, and errors

`connect()` records connection intent. `disconnect()` clears it. Transient
transport failures retry while connection is desired. After correcting a
terminal failure, call `connect()` again.

`useAgent` renews authorization when only `config.token` changes. Key the owning
component when room, participant identity, endpoint, or topology changes so
React replaces the Agent.

```tsx
<Room key={`${roomId}:${participantId}`} token={token} />
```

Render `agent.connection` and `agent.error` for user-visible status. Do not put
credentials in URLs, storage, or logs.

## Data tracks

PulseBeam data tracks carry bytes. The application owns its wire format,
validation, versioning, and malformed-payload policy.

```tsx
const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

function Chat({ agent }: { agent: Agent }) {
  const ChatTrack = agent.data("chat", "ordered");

  async function send(message: string) {
    await ChatTrack.send(encoder.encode(JSON.stringify(message)));
  }

  return (
    <>
      <button onClick={() => void send("hello").catch(report)}>Send hello</button>
      <Data
      track={ChatTrack}
      onEvent={(event) => {
        if (event.type === "gap") {
          showGap(event.from, event.to);
          return;
        }
        try {
          const value: unknown = JSON.parse(decoder.decode(event.data));
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

Mounting `Data` owns one subscription and unmounting releases it. `lossy` emits
messages only. `ordered` may also emit a `gap` when unavailable history prevents
continuous recovery. Consumers must preserve the gap before later messages.
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
