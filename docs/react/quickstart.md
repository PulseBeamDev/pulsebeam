---
title: React quickstart
description: Add live video and audio to your React app.
---

# Add video and audio to React

You own **your application's backend** and **React frontend**. PulseBeam
provides the separate media server, which you run.

```mermaid
flowchart LR
    subgraph app["Your application — code you own"]
        backend["Backend: sign tokens"] -->|"participant token"| frontend["React frontend: runs in the browser"]
    end
    frontend <-->|"connection + media"| server["PulseBeam media server: you run"]
```

Works with Next.js, Vite, Astro + React, or your existing React app.

## 1. Run the PulseBeam media server

With Docker (or Podman):

```sh
docker run --rm --network host ghcr.io/pulsebeamdev/pulsebeam:08eb663 --dev
```

The PulseBeam endpoint is `http://localhost:7070`.

::: warning Local development only
`--dev` trusts a fixed test key whose signing secret is publicly known.
Anyone can mint valid tokens for it. Never expose a `--dev` server publicly.
:::

## 2. Your app backend: issue a participant token

In **your application's backend** (Node.js), install the server SDK:

```sh
npm install @pulsebeam/server
```

`INSECURE_DEV_API_KEY` is a single publicly known `key_0` credential matching
the server's `--dev` identity. Production backends use their own
`PULSEBEAM_API_KEY` secret; see the [V0 format](../../server/auth/api-key.md).

```ts
import { signParticipantToken, INSECURE_DEV_API_KEY } from "@pulsebeam/server";

function issueToken(participant: string) {
  return signParticipantToken({
    apiKey: INSECURE_DEV_API_KEY,
    room: "demo",
    participant,
    expiration: Math.floor(Date.now() / 1000) + 3600,
  });
}
```

Call `issueToken(participantId)` for each stable application participant ID and send the resulting token to
your React frontend through your application's API or server-rendered props.
Never send signing credentials to the browser. **In production:** run PulseBeam
without `--dev`, register your own public key with `--project-registry`, and
keep the corresponding `PULSEBEAM_API_KEY` on your application backend. Authenticate
users and authorize room access before issuing tokens.

For Rust, Go, or Python backends, use the corresponding server SDK.

## 3. Your React frontend: capture and join

Install the React SDK in your application:

```sh
npm install @pulsebeam/react
```

**Capture and the Agent are independent.** The parent owns the camera and
microphone; the keyed room owns the PulseBeam session. Pass in a token from
your application's backend.

```tsx
import { useEffect } from "react";
import {
  Video, useAgent, useUserMedia, type CaptureResult,
} from "@pulsebeam/react";

export function Call({ sessionId, token }: { sessionId: string; token: string }) {
  const media = useUserMedia({ video: true, audio: true });

  async function enableDevices() {
    try {
      await media.request();
    } catch {
      // useUserMedia exposes the failure through media.error.
    }
  }

  return (
    <>
      <button onClick={enableDevices}>Enable camera & mic</button>
      {media.error && <p role="alert">{media.error.message}</p>}
      <Video source={media.videoTrack} mirror style={{ width: 320, height: 240 }} />
      <Room key={sessionId} token={token} media={media} />
    </>
  );
}

function Room({ token, media }: { token: string; media: CaptureResult }) {
  const agent = useAgent({ endpoint: "http://localhost:7070", token });

  useEffect(() => {
    agent.local.video("camera").setSource(media.videoTrack);
    agent.local.audio("microphone").setSource(media.audioTrack);
  }, [agent, media.videoTrack, media.audioTrack]);

  const { connection, failure } = agent.getSnapshot();

  return (
    <>
      <button onClick={() => agent.connect()}>Join room</button>
      <button onClick={() => agent.disconnect()}>Leave room</button>
      <p>Connection: {connection}</p>
      {connection === "terminal-failure" && failure && (
        <p role="alert">{failure.message}</p>
      )}
      {agent.remote.videoTracks.map((track) => (
        <Video
          key={`${track.participantId}:${track.label}`}
          source={track}
          style={{ width: 320, height: 240 }}
        />
      ))}
    </>
  );
}
```

The effect keeps publications synchronized with captured sources, including
device changes. `connect()` records connection intent; the Agent handles
retries. The browser sends media directly to PulseBeam, not through your
application backend. To switch rooms or participants, change `sessionId`
(the React key); refreshing a token for the same session preserves its Agent.

## 4. Try it

Open your app in two tabs with different participant tokens for the same
`demo` room. Enable devices and join in both tabs. Each should display the
other's video and play remote audio. Use headphones to avoid feedback.

## Complete example

Explore the [complete React example](https://github.com/PulseBeamDev/pulsebeam/tree/main/apps/meet)
or see [capacity and configuration](/react/reference#configuration-types).

Next: [screen sharing and controls](/react/guides) ·
[API reference](/react/reference) · [Troubleshooting](/react/troubleshooting).
