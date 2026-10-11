---
title: React SDK concepts
description: Build browser media integrations with React-owned Agents and capture.
---

# React SDK concepts

Start with the [quickstart](/react/quickstart), then use the
[integration guides](/react/guides), [API reference](/react/reference), and
[troubleshooting](/react/troubleshooting). All application-facing imports on
these pages come from `@pulsebeam/react`.

## Four roles

- **React application:** renders controls and media, requests capture after a
  gesture, and expresses whether the user wants to be connected.
- **Browser SDK runtime:** owns an Agent's connection, reconciles publications
  and receive demand, and plays selected remote audio. Its JavaScript and WASM
  run in the browser, not on an SSR server.
- **PulseBeam server:** admits a participant to a room and forwards media and
  topic traffic among admitted participants. It does not capture your camera.
- **Application token backend:** authenticates your users and issues scoped,
  expiring participant tokens. Signing secrets stay here, never in React code.

For local development the server's `--dev` profile and CLI replace the token
backend. This is not a production authentication design.

## Rooms and identity

A credential chooses a room and participant identity. Two simultaneous Agents
must use **distinct participant credentials in the same room** to exchange
media. Calling `connect()` only expresses intent. Inspect
`getSnapshot().connection === "connected"` and the admitted
`participantExternalId` and `roomExternalId` before displaying successful entry.

Those external IDs are your application identities. They are null before
admission, retained during reconnection to the same session, and cleared on
disconnect. The snapshot's `participantId` is an opaque internal canonical ID,
not your user ID. Logical remote handles use the external ID in their
`participantId` property; raw Catalog publications use canonical IDs.
Do not persist transport generations, receiver indices, or publication IDs as
application identities.

## Agent ownership and intent

`useAgent(config)` creates one independent Agent after mount, can initially
return null, and closes it on unmount. It observes snapshots so discovery and
connection changes rerender its component. Agents start disconnected.
`connect()` requests connection and retry; `disconnect()` clears that intent.
Temporary transport loss can recover while intent remains connected.
`reconnect()` preserves desired state. A terminal failure needs diagnosis,
not a loop of blind retries.

Token changes renew authorization on the existing Agent. Endpoint, topology,
or logging changes replace it. Topology reserves fixed sender and receiver
capacity; it does not grow when more video elements mount. Use
`createAgent()` only if you will explicitly own subscriptions and `close()`.

## Capture, publications, and stable handles

Capture is separate from connection. `useUserMedia({video: true, audio: true})`
does nothing until `request()`. Its hook owns native tracks: `stop()` or
unmount releases the devices. You can [preview before joining](/react/guides#pre-join-preview).

`agent.local.video("camera")` reserves a stable logical video handle.
`setSource(capture.videoTrack)` lends capture to that handle and expresses a
publication. Labels such as `camera`, `microphone`, and `screen` are your
application's per-kind names. Repeated lookup returns the same handle.
Labels and slots stay reserved for the Agent lifetime, even after withdrawal.

- `setEnabled(false)` retains the publication and sends black video or silent
  audio. It does not release the device.
- `setSource(null)` withdraws the publication. It does not stop borrowed capture.
- `capture.stop()` releases the native tracks. Clear or update the handle's
  source too, as the quickstart does.
- Agent disconnect or close does not stop capture borrowed from a hook.

Each sender uses SDK-owned clones, so lending one capture source to two Agents
does not share their mute state.

## Discovery, presentation, and audio

Remote publications appear in `agent.remote.videoTracks` and
`audioTracks`, excluding self. Discovery is not proof that media is mapped
or playing. `remote.participant(externalId).video(label)` returns a logical
handle even before discovery; the handle survives departure and republication.

Mounting `<Video source={remoteTrack} />` contributes receive demand based on
visible physical-pixel size. Hidden/off-screen elements do not reserve receivers.
Unmounting releases demand. Receive policy can constrain quality without
activating an unpresented track.

Remote audio is automatically selected and played by the runtime, subject to
browser autoplay restrictions. There is no React audio component to mount.
`remoteAudio.receiving` reports Mapping, not audibility; recover blocked audio
with `agent.remote.resumeAudio()` directly from a gesture.

## Typed topics

`agent.topic<T>("chat", {mode: "reliable"})` exchanges typed JSON, not arbitrary
data-channel handles. Construction alone does not subscribe. Publishing and
async subscription express their own interest; abort or end iteration to
release it. Ordered and latest-value delivery are bounded, not durable messaging.
See [topics and recovery](/react/guides#typed-topics) before using them for
application state.
