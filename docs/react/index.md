---
title: React SDK
description: Add real-time media and live room messages with the PulseBeam React SDK.
---

# React SDK

Use `@pulsebeam/react` to connect a browser application to a PulseBeam room,
publish camera, microphone, or screen capture, and present other participants'
media. Your application provides the server endpoint and participant token.

**[Install the SDK and add your first room →](/react/quickstart)**

The Next.js quickstart generates participant tokens on the server, then connects
two browser participants with the React SDK. You do not need to learn the
transport or write signaling code first.

## Find your next task

| You need to… | Open |
| --- | --- |
| Join, capture, play remote media, and leave | [Quickstart](/react/quickstart) |
| Add screen sharing, device selection, renewal, or live messages | [Guides](/react/guides) |
| Find a hook, component, option, or error type | [API reference](/react/reference) |
| Diagnose a failed join, capture, or playback | [Troubleshooting](/react/troubleshooting) |

## How the room works

These concepts explain the quickstart after you have tried it:

1. **Agent:** one room session for one participant. `useAgent` owns it while
   the component is mounted and closes it on unmount.
2. **Capture:** `useUserMedia` and `useDisplayMedia` own local capture tracks.
   Start capture from a user gesture. Connect alone does not open devices.
3. **Publication:** `agent.local.video("camera")` and
   `agent.local.audio("microphone")` create labeled handles. Bind captured tracks
   to these handles as the quickstart does.
4. **Remote media:** `agent.remote` exposes participants and publications.
   `Video` presents a track and requests reception while it is visible and
   nonzero-sized. Remote audio playback is automatic once connected; a user
   gesture may be needed to unblock it.

A room groups participants authorized to join it. The admitted external IDs are
your application's room and participant identities, not opaque transport IDs.
The `camera` and `microphone` strings are labels chosen by your application,
not protocol constants. Logical track handles use participant identity and label
to follow media across temporary absence or replacement. Keep labels stable.

For live application messages, `agent.topic<T>(name, {mode})` creates a typed JSON
publish/subscribe facade. It is not durable message storage. See
[typed topics](/react/guides#typed-topics) for publishing, subscription cleanup,
and recovery limits.

## What your application owns

- **Authorization:** authenticate users and issue scoped participant tokens on
  your backend. Never expose signing keys to the browser.
- **Session identity:** use one Agent per room/participant identity. Renew the
  token for the same session; replace the Agent when identity changes.
- **Media lifecycle:** disconnecting stops sending and receiving, but capture
  can remain active. Stop capture on leave or unmount its owning hooks.
- **Capacity:** choose topology before creating the Agent. It reserves outgoing
  and incoming media capacity; changing it requires a new Agent.

Use the [guides](/react/guides) when adding controls, and the
[reference](/react/reference) for exact contracts and advanced ownership APIs.
