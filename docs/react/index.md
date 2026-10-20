---
title: React SDK
description: Add real-time media and live room messages with the PulseBeam React SDK.
---

# React SDK

Use `@pulsebeam/react` to connect a browser application to a PulseBeam room,
publish camera, microphone, or screen capture, and present other participants'
media. Your application provides the server endpoint and participant token.

**[Install the SDK and add your first room →](/react/quickstart)**

You build your application's backend and React frontend. PulseBeam supplies
the media server, which you run with Docker or Podman. The
[quickstart](/react/quickstart) shows the ownership and connection flow:
your backend signs participant tokens using `@pulsebeam/server`; your React
frontend uses `@pulsebeam/react` to connect directly to PulseBeam.
Signing credentials never enter the browser.

## Find your next task

| You need to… | Open |
| --- | --- |
| Join, capture, play remote media, and leave | [Quickstart](/react/quickstart) |
| Add screen sharing, device selection, renewal, or live messages | [Guides](/react/guides) |
| Find a hook, component, option, or error type | [API reference](/react/reference) |
| Diagnose a failed join, capture, or playback | [Troubleshooting](/react/troubleshooting) |

## How the room works

These concepts explain the quickstart after you have tried it:

1. **Agent:** one room session handle for one participant. `useAgent` returns
   a handle immediately and owns its runtime while the component is mounted.
2. **Capture:** `useUserMedia` and `useDisplayMedia` own local capture tracks.
   Start capture from a user gesture. Connect alone does not open devices.
3. **Publication:** `agent.local.video("camera")` and
   `agent.local.audio("microphone")` return stable labeled component handles.
   Render `<Camera source={capture.video} />` to declare a committed
   publication. Lookup alone does not publish or reserve a slot.
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
component with a `publish` action. Mounting it owns a consumer; unmounting
releases that consumer. It is not durable message storage. See
[typed topics](/react/guides#typed-topics) for publishing, subscription cleanup,
and recovery limits.

## What your application owns

- **Authorization:** authenticate users and issue scoped participant tokens on
  your backend. Never expose signing keys to the browser.
- **Session identity:** use one Agent per room/participant identity. Renew the
  token for the same session; replace the Agent when identity changes.
- **Media lifecycle:** disconnecting stops sending and receiving, but capture
  can remain active. Stop capture on leave or unmount its owning hooks.
- **Capacity:** topology is optional; the default reserves 2 local videos,
  2 local audios, 16 remote videos and 8 remote audios. These are fixed media
  slots, not participant limits. Changing capacity requires a new Agent;
  automatic growth is not supported yet.

Use the [guides](/react/guides) when adding controls, and the
[reference](/react/reference) for exact contracts. React owns Agents through
`useAgent` only. Non-React applications use caller-owned `createAgent` from
`@pulsebeam/web`; internal managed lifecycle functions are not public APIs.
