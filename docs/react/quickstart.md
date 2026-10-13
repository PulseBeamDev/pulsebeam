---
title: React quickstart
description: Add a live video room to a Next.js app.
---

# Add a video room to React

Build a room where two people can see and hear each other. We'll use Next.js
to issue participant tokens and `@pulsebeam/react` to handle media.
The snippets below are building blocks for your own components, not a UI template.

You need Node.js 22+ and a local PulseBeam development server.

## 1. Create an app

```bash
npx create-next-app@latest pulsebeam-room --ts --app --no-src-dir --use-npm --yes
cd pulsebeam-room
npm install @pulsebeam/react @pulsebeam/server
```

## 2. Create a participant token

Start a PulseBeam server in `--dev` mode at `http://localhost:7070`
([local server setup](/build#local-development)). On the Next.js server:

```tsx
import { signParticipantToken, INSECURE_DEV_CREDENTIALS } from "@pulsebeam/server";

function issueToken() {
  return signParticipantToken({
    ...INSECURE_DEV_CREDENTIALS,
    room: "demo",
    participant: crypto.randomUUID(),
    expiration: Math.floor(Date.now() / 1000) + 3600,
  });
}
```

Call `issueToken()` inside a Server Component or request handler for each visitor,
then pass that token to your React room component. Each participant needs a
unique token for the same room; don't create it at module scope. For a Next.js
page, use `export const dynamic = "force-dynamic"` to avoid sharing a cached page.

::: warning Local development only
The built-in credentials are public and work with PulseBeam's `--dev` server.
**Never expose a dev server publicly or use these credentials for production.**
In production, authenticate and authorize participants and protect signing keys
on your server.
:::

The [shared JSON fixture](https://github.com/PulseBeamDev/pulsebeam/blob/main/server/auth/development.json)
contains the same credentials for other server SDKs.

## 3. Connect

In a React Client Component (`"use client"`), import `useAgent` from
`@pulsebeam/react`. Pass it the token issued by your server:

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

Open [localhost:3000](http://localhost:3000) in two tabs and click **Join** in
both. You should see and hear the other participant. Use headphones to avoid
feedback; if audio is blocked, click **Enable audio**. On one device, you may
need two browsers or devices if camera sharing is unsupported.

Next: [screen sharing and device controls](/react/guides) ·
[API reference](/react/reference) · [Troubleshooting](/react/troubleshooting).
