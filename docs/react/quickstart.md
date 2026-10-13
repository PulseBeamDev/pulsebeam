---
title: React quickstart
description: Add live video and audio to any React app.
---

# Add a video room to React

Build a room where two people can see and hear each other. The same React SDK
works with Next.js, Vite, Astro, and other React setups. These short snippets
focus on PulseBeam, not your app's UI.

You need a local PulseBeam development server and Node.js 22+ to issue tokens.

## 1. Install the SDK

In your React project:

```bash
npm install @pulsebeam/react
```

On your Node.js backend, install `@pulsebeam/server` to sign participant tokens:

```bash
npm install @pulsebeam/server
```

Starting from scratch? Use your usual scaffolder—for example,
`npx create-next-app@latest pulsebeam-room` for Next.js.
The remaining steps are the same regardless of framework.

## 2. Create a participant token

Start a PulseBeam server in `--dev` mode at `http://localhost:7070`
([local server setup](/build#local-development)). On your **backend**:

```tsx
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

Call `issueToken()` for each participant after deciding who can join the room,
then send that token to your React app. Each participant needs a distinct
identity in the same room. Never sign tokens in the browser or cache one
participant's token for other visitors.

::: warning Local development only
The built-in credentials are public and work with PulseBeam's `--dev` server.
**Never expose a dev server publicly or use these credentials for production.**
In production, authenticate and authorize participants and protect signing keys
on your server.
:::

These development credentials are shared by the other server SDKs.

## 3. Connect

In your browser-rendered React component, import `useAgent` from
`@pulsebeam/react`. The `token` comes from your backend:

```tsx
const agent = useAgent({
  endpoint: "http://localhost:7070",
  token,
  topology: { localVideos: 1, localAudios: 1, remoteVideos: 2, remoteAudios: 2 },
});
```

One Agent represents one participant in one room. Call `agent.connect()`
from your **Join** button.

## 4. Publish media

Import `useUserMedia` from `@pulsebeam/react` and `useEffect` from React:

```tsx
const media = useUserMedia({ video: true, audio: true });

useEffect(() => {
  if (!agent) return;
  agent.local.video("camera").setSource(media.videoTrack);
  agent.local.audio("microphone").setSource(media.audioTrack);
}, [agent, media.videoTrack, media.audioTrack]);
```

Connect and request capture directly from the Join button:

```tsx
<button onClick={() => {
  agent?.connect();
  void media.request().catch(console.error);
}}>Join</button>
```

The labels `camera` and `microphone` identify publications that
other participants can receive.

## 5. Display remote video

Import `Video` from `@pulsebeam/react` and render the tracks you want to receive:

```tsx
{agent?.remote.videoTracks.map((track) => (
  <Video key={track.participantId + ':' + track.label}
    source={track} style={{ width: 320, height: 240 }} />
))}
```

Visible videos request incoming media. Remote audio plays automatically when
the browser permits; if blocked, call `agent.remote.resumeAudio()` directly
from a button click. On leave, call `agent.disconnect()` and `media.stop()`.

## 6. Try it

```bash
npm run dev
```

Open the local URL printed by your dev server in two tabs. Issue a distinct token
for each tab in the same `demo` room, then click **Join** in both. You should
see and hear the other participant. Use headphones to avoid feedback; if
audio is blocked, call `agent.remote.resumeAudio()` from a button click. On one
device, you may need two browsers or devices if camera sharing is unsupported.

Next: [screen sharing and device controls](/react/guides) ·
[API reference](/react/reference) · [Troubleshooting](/react/troubleshooting).
