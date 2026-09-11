# PulseBeam React

`@pulsebeam/react` is Meet's complete app-facing PulseBeam SDK. It includes the
agent factory, all public types, the browser runtime, and the React adapter.

Install the adapter with React:

```sh
pnpm add @pulsebeam/react react
```

Create and close an agent for each effect lifetime, then place `AgentProvider`
above components that call `useAgent`. The provider never initializes,
reconnects, replaces, or closes its caller-owned agent. Its preparation recipe
builds the internal web runtime and WASM assets; applications do not need a web
SDK dependency, runtime override, or private asset path.

```tsx
import { AgentProvider, createAgent, useAgent, useRemoteMedia, type Agent } from "@pulsebeam/react";
import { useEffect, useRef, useState } from "react";

function Status({ agent }: { agent: Agent }) {
  const { connection, tracks, topics, subscribeEvents } = useAgent();
  const remoteVideo = useRef<HTMLVideoElement>(null);
  useRemoteMedia(agent, remoteVideo, {
    publicationIds: Object.values(tracks)
      .filter((track) => track.kind === "video")
      .map((track) => track.publicationId),
    onPlaybackBlocked: (failure) => console.warn(failure.message),
  });
  useEffect(() => subscribeEvents((event) => {
    if (event.type === "topic-message") console.log(event.topic, event.payload);
  }), [subscribeEvents]);
  return (
    <>{connection} <video ref={remoteVideo} autoPlay /> {topics.deliveredMessages}</>
  );
}

function App() {
  const [agent, setAgent] = useState<Agent | null>(null);
  useEffect(() => {
    const current = createAgent({ endpoint: "https://pulsebeam.example", token: "opaque-token", topology: { localVideo: ["camera", "screen"], localAudio: ["mic"] } });
    current.setState({ connected: true, publications: [{ slot: "camera", active: true }, { slot: "mic", active: true }, { slot: "screen", active: true }], video: [{ slot: 0, trackId: "remote-camera", height: 720, minHeight: 360, minFps: 24, priority: 1 }], audio: { automatic: true }, playoutDelay: { mode: "fixed", minMs: 50, maxMs: 100 }, topics: [{ name: "chat", mode: "ordered", publish: true, subscribe: true }, { name: "reaction", mode: "latest", publish: true, subscribe: true }] });
    setAgent(current);
    return () => { setAgent(null); current.close(); };
  }, []);
  return agent && <AgentProvider agent={agent}><Status agent={agent} /></AgentProvider>;
}

```

The web agent owns browser attachment and playback. `useRemoteMedia` binds
selected remote publication IDs to a caller-owned audio or video element; the
hook retains the attachment across ordinary renders and returns a stable retry
operation for blocked-playback UI. The application still chooses the element
and visible tracks.
