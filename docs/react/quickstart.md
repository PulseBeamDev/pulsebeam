---
title: React quickstart
description: Add a live video room to a Next.js app.
---

# Add a video room to React

Build a room where two people can see and hear each other. We'll use Next.js
to issue participant tokens and `@pulsebeam/react` to handle media.

You need Node.js 22+, a PulseBeam server URL, and signing credentials.

## 1. Create an app

```bash
npx create-next-app@latest pulsebeam-room --ts --app --no-src-dir --use-npm --yes
cd pulsebeam-room
npm install @pulsebeam/react @pulsebeam/server
```

## 2. Issue a participant token

Replace `app/page.tsx`:

```tsx
import { randomUUID } from "node:crypto";
import { signParticipantToken, INSECURE_DEV_CREDENTIALS } from "@pulsebeam/server";
import Room from "./room";

export const dynamic = "force-dynamic";

export default function Page() {
  const token = signParticipantToken({
    ...INSECURE_DEV_CREDENTIALS,
    room: "demo",
    participant: randomUUID(),
    expiration: Math.floor(Date.now() / 1000) + 3600,
  });

  return <Room token={token} endpoint="http://localhost:7070" />;
}
```

Replace the placeholders and endpoint with your PulseBeam credentials and server
URL. Each page load gets a new participant identity in the same room.

**For local development only:** Don't commit real signing secrets. In production,
authenticate users, authorize their room access, and load signing credentials from
your server's secret store.

## 3. Add the room

Create `app/room.tsx`:

```tsx
"use client";

import { useEffect } from "react";
import { Video, useAgent, useUserMedia } from "@pulsebeam/react";

export default function Room({ token, endpoint }: { token: string; endpoint: string }) {
  const agent = useAgent({
    token, endpoint,
    topology: { localVideos: 1, localAudios: 1, remoteVideos: 2, remoteAudios: 2 },
  });
  const media = useUserMedia({ video: true, audio: true });

  useEffect(() => {
    if (!agent) return;
    agent.local.video("camera").setSource(media.videoTrack);
    agent.local.audio("microphone").setSource(media.audioTrack);
  }, [agent, media.videoTrack, media.audioTrack]);

  return (
    <main>
      <p>Connection: {agent?.getSnapshot().connection ?? "preparing"}</p>
      <button disabled={!agent} onClick={() => {
        agent?.connect();
        void media.request().catch(console.error);
      }}>Join</button>
      <button onClick={() => { media.stop(); agent?.disconnect(); }}>Leave</button>
      <button onClick={() => void agent?.remote.resumeAudio().catch(console.error)}>
        Enable audio
      </button>
      <Video source={media.videoTrack} mirror style={{ width: 320, height: 240 }} />
      {agent?.remote.videoTracks.map((track) => (
        <Video key={track.participantId + ":" + track.label}
          source={track} style={{ width: 320, height: 240 }} />
      ))}
    </main>
  );
}
```

`useAgent` connects to the room. `useUserMedia` requests camera and microphone
access when you click **Join**. Local tracks are published as `camera` and
`microphone`; rendering a remote `Video` requests that participant's video.
Remote audio plays automatically when the browser allows it.

## 4. Try it

```bash
npm run dev
```

Open [localhost:3000](http://localhost:3000) in two tabs and click **Join** in
both. You should see and hear the other participant. Use headphones to avoid
feedback; if audio is blocked, click **Enable audio**. On one device, you may
need two browsers or devices if camera sharing is unsupported.

Next: [screen sharing and device controls](/react/guides) ·
[API reference](/react/reference) · [Troubleshooting](/react/troubleshooting).
