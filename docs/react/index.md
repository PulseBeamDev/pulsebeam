---
title: React SDK
description: Add real-time media and live room messages with the PulseBeam React SDK.
---

# React SDK

Use `@pulsebeam/react` to connect a browser application to a PulseBeam room,
publish camera, microphone, or screen capture, and present other participants'
media. Your application provides the server endpoint and participant token.

**[Install the SDK and add your first room →](/react/quickstart)**

These pages describe the track-centric API in the current source tree. Use
matching Web and React packages that export this API; older released packages
may expose a different interface.

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
   null until its committed effect creates the actual Web SDK Agent, then owns
   that handle and observation while mounted.
2. **Capture:** `useCapture("device", options)` and
   `useCapture("display", options)` own local capture tracks. Start capture
   from a user gesture. Connect alone does not open devices.
3. **Publication:** look up a local handle with `agent.video(trackId)` or
   `agent.audio(trackId)` and pass it to `usePublish` to own publication. Lookup alone
   does not publish or reserve a slot.
4. **Remote media:** look up a known source with
   `agent.room(room).participant(id).video(trackId)` or `.audio(trackId)`. Lookup is
   inert and room-pinned. `useSubscribe` owns exact reception independently of
   discovery and returns the SDK subscription; its `state()` reports execution.

A room groups participants authorized to join it. Room and participant IDs are
your application's names, not opaque connection IDs. The `camera` and `microphone`
strings are track IDs chosen by your application. Keep IDs stable so logical
track handles can follow media across temporary absence or replacement.

For live application messages, pass `agent.data(trackId, delivery)` to `usePublish`.
To receive from one participant, pass
`agent.room(room).participant(id).data(trackId, delivery)` to `useSubscribe` and read
its typed events. Data payloads are bytes; your application owns encoding,
decoding, validation, and malformed-payload policy. It is not
durable message storage. See [data tracks](/react/guides#data-tracks) for
publication, subscription cleanup, and recovery limits.

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
`useAgent` only. Non-React applications use caller-owned `new Agent(config)` from
`@pulsebeam/web`; internal managed lifecycle functions are not public APIs.
