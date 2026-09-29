# PulseBeam React

`@pulsebeam/react` provides browser acquisition, Agent ownership, and playback.
Install it alongside React:

```sh
pnpm add @pulsebeam/react react
```

`useAgent(config)` creates one independent Agent after mount and closes it on
unmount. It returns `null` until the Agent is ready to use. Agents start
**disconnected**: call `connect()` to request a connection and `disconnect()` to
stop retrying. Both methods change desired connection state synchronously;
observe `agent.getSnapshot().connection` for progress. Changing the token
renews authorization on the same Agent; changing endpoint, topology, or logging
replaces the Agent. `agent.getSnapshot().participantExternalId` and
`roomExternalId` are the authoritative application identities supplied by
admission. They are null before admission, retained while reconnecting to the
same session, and cleared on disconnect. `participantId` remains a separate
opaque canonical identifier for SDK internals. Use a distinct participant credential for each simultaneous
Agent. The hook subscribes its component to Agent snapshot changes.

Capture is owned by its acquisition hook, not by any Agent. Request it from a
user gesture, then lend its source to one or more stable logical handles:

```tsx
import { useEffect } from "react";
import { useAgent, useUserMedia, type AgentConfig } from "@pulsebeam/react";

function Camera({ config }: { config: AgentConfig }) {
  const agent = useAgent(config);
  const capture = useUserMedia({ video: true, audio: false });
  useEffect(() => {
    if (!agent) return;
    agent.connect();
    return () => agent.disconnect();
  }, [agent]);
  useEffect(() => {
    if (!agent) return;
    const camera = agent.localVideoTrack("camera");
    camera.setSource(capture.videoTrack);
    return () => camera.setSource(null);
  }, [agent, capture.videoTrack]);
  return (
    <button onClick={() => void capture.request().catch(console.error)}>
      {capture.state === "requesting" ? "Requesting…" : "Choose camera"}
    </button>
  );
}
```

Use the same Agent's catalog-backed remote handles directly, without looking
up canonical publication IDs. Mounted video requests receive bandwidth based
on visible element size; unmount releases demand. Audio playback is explicit:

```tsx
import { Audio, Video, useAgent, type AgentConfig } from "@pulsebeam/react";

function Room({
  config,
  onPlaybackError,
}: {
  config: AgentConfig;
  onPlaybackError: (error: unknown, retry: () => Promise<void>) => void;
}) {
  const agent = useAgent(config);
  if (!agent) return null;
  return (
    <>
      {agent.remoteVideoTracks.map((track) => (
        <Video
          key={`${track.participantId}:${track.label}`}
          source={track}
          mirror={false}
          className="participant"
        />
      ))}
      <Audio
        source={agent.remoteAudio}
        onPlaybackError={({ error, retry }) => onPlaybackError(error, retry)}
      />
    </>
  );
}
```

`<Video>` also accepts a local video handle or a captured video source for
pre-join preview, plus `null`, `mirror`, `muted`, `playsInline`, `className`, and
`style`. Both playback components default to `autoPlay={true}`. Set
`autoPlay={false}` to attach without starting playback automatically; native
`controls` can then start playback. Attachment still contributes receive demand.
Use
`track.setReceiveOptions({minHeight,minFps,priority,playoutDelay})` to replace
remote policy without activating a hidden track. `onPlaybackError` receives a
user-gesture retry for autoplay restrictions. `agent.topic<T>(name, { mode:
"reliable" | "unreliable" })` provides typed JSON `publish(value)` and
`subscribe({signal})` iteration; creating a topic does not subscribe.

`useMediaDevices()` lists reactive cameras, microphones and speakers with
readonly `id` and `label`, which may be empty until permission is granted.
`useDisplayMedia({video,audio})` uses the same explicit `request()` and `stop()`
pattern. User-media options changing while active request a replacement while
retaining the old capture until success; display-media option changes never
open a chooser automatically. `stop()` and unmount stop native tracks. Agent
disconnect, source clearing, replacement and close never stop borrowed tracks.
Local handle labels and slots are reserved for the Agent's lifetime; exhausting
capacity throws `LocalTrackCapacityError` synchronously.

For low-level integration, `createAgent(config)` returns a caller-owned Agent.
React ownership uses only `useAgent(config)`; playback uses `<Video>` and
`<Audio>` with source handles. There is no provider-based ownership adapter or
raw media attachment hook.
The package build prepares the internal web runtime and WASM asset, without an
application dependency on `@pulsebeam/web`.
