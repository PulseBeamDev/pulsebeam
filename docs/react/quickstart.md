---
title: React quickstart
description: Add live video and audio to your React app.
---

# Add video and audio to React

Use PulseBeam with Next.js, Vite, Astro + React, or your existing React app.
You'll run a development server, issue participant tokens, and connect two tabs.

## 1. Start PulseBeam

On Linux x86_64, run the published server image with Docker or Podman:

```sh
docker run --pull=always --rm --network=host ghcr.io/pulsebeamdev/pulsebeam:main --dev
```

Using Podman? Replace `docker` with `podman`. Your server is now at
`http://localhost:7070`. Host networking lets WebRTC use UDP port 3478
without container port mapping.

The `:main` image follows the main branch. For a reproducible setup, use
[a published commit-SHA image tag](https://github.com/PulseBeamDev/pulsebeam/pkgs/container/pulsebeam)
instead. Building PulseBeam [from source](/build#local-development) is optional,
not a prerequisite for using the SDK.

::: warning Development only
`--dev` uses **public, insecure signing keys** and does not restrict network
listeners to localhost. Host networking exposes the server's listeners on your
machine's network interfaces. Keep them behind your local firewall; never
expose them to the internet or use these credentials in production.
:::

## 2. Install the SDK

Use your usual React scaffolder if you're starting a new project, then install:

```sh
npm install @pulsebeam/react
```

On your **Node.js backend** (or the server side of a full-stack application):

```sh
npm install @pulsebeam/server
```

For other backends, use the Rust, Go, or Python server SDK.

## 3. Let a participant join

On your backend, issue a token for each visitor:

```ts
import { signParticipantToken, INSECURE_DEV_CREDENTIALS } from "@pulsebeam/server";

function issueToken() {
  return signParticipantToken({
    projectId: INSECURE_DEV_CREDENTIALS.projectId,
    keyId: INSECURE_DEV_CREDENTIALS.keyId,
    secret: INSECURE_DEV_CREDENTIALS.secret,
    room: "demo",
    participant: crypto.randomUUID(),
    expiration: Math.floor(Date.now() / 1000) + 3600,
  });
}
```

Your app delivers the token to React using its existing backend-to-client flow.
Each tab must get its **own participant identity** in the same `demo` room.
Keep signing credentials on the backend. In production, authenticate the user
and authorize room access before signing.

In your browser-rendered React component:

```tsx
const agent = useAgent({ endpoint: "http://localhost:7070", token });
```

Import `useAgent` from `@pulsebeam/react`; call `agent.connect()` on **Join**.

## 4. Share camera and microphone

```tsx
const media = useUserMedia({ video: true, audio: true });

useEffect(() => {
  if (!agent) return;
  agent.local.video("camera").setSource(media.videoTrack);
  agent.local.audio("microphone").setSource(media.audioTrack);
}, [agent, media.videoTrack, media.audioTrack]);
```

Import `useUserMedia` from `@pulsebeam/react` and `useEffect` from React.
Request capture in the Join click handler (not on mount):

```tsx
<button onClick={() => {
  agent?.connect();
  void media.request().catch(console.error);
}}>Join</button>
```

The labels `camera` and `microphone` name your publications.

## 5. See the other participant

```tsx
{agent?.remote.videoTracks.map((track) => (
  <Video key={track.participantId + ":" + track.label} source={track} />
))}
```

Import `Video` from `@pulsebeam/react` and give it a visible, nonzero size.
Remote audio plays automatically when allowed; if blocked, call
`agent.remote.resumeAudio()` from a user click. On leave, call
`agent.disconnect()` and `media.stop()`.

Run your app and join from two tabs with distinct tokens. **Success means each
tab receives moving video and audio from the other**, not just a local preview.
Use headphones to avoid feedback.

## Default capacity

You can omit `topology`, but capacity is **fixed for each Agent**:

| Direction | Video tracks | Audio tracks |
| --- | ---: | ---: |
| Publish locally | 2 | 2 |
| Receive remotely | 16 | 8 |

These are reserved **track slots**, not room participant limits. Each of the
four capacity fields can be set to at most **32**. A label keeps its outgoing
slot even after withdrawal. Exceeding local slots throws
`LocalTrackCapacityError`; excess remote tracks cannot be assigned receivers.
To override defaults, pass `topology: { remoteVideos: 4 }` (other fields keep
their defaults). Today changing capacity requires **recreating the Agent and
reconnecting**; automatic expansion is planned but not available yet.
See [capacity and limits](/react/reference#configuration-types).

## Complete example

Want to inspect a working application instead of assembling snippets?
[PulseBeam Meet](https://github.com/PulseBeamDev/pulsebeam/tree/main/apps/meet)
is the full React example. Its [local run instructions](https://github.com/PulseBeamDev/pulsebeam/blob/main/apps/meet/README.md)
use the source repository's Bazel development workflow; that is **not**
required to integrate the published SDK into your own app.

Next: [screen sharing and controls](/react/guides) ·
[API reference](/react/reference) · [Troubleshooting](/react/troubleshooting).
