---
title: Build with PulseBeam
description: Add real-time video, audio, and live messages to your application.
---

# Build with PulseBeam

PulseBeam connects participants in a room so your application can share video,
audio, and live messages. Your app controls capture and presentation; the server
forwards media between participants.

**Building a React app? [Add your first video room →](/react/quickstart)**

Install the React SDK, connect to a PulseBeam server, and see and hear another
participant in two browser tabs. You do not need to write WebRTC signaling code.

## Choose your path

| You want to… | Start here |
| --- | --- |
| Add a room to a React application | [React quickstart](/react/quickstart) |
| Add screen sharing, device controls, or live messages | [React guides](/react/guides) |
| Publish from a WHIP-compatible client | [WHIP publishing](/whip) |
| Operate a PulseBeam server | [Deployment](/deployment) |
| Work on PulseBeam itself | [Contributor setup](/build) |

## What you need for an integration

- A PulseBeam server endpoint reachable from your application.
- A participant token issued for the room and identity you want to join.
- A browser application that requests camera and microphone access from a user action.

Your backend decides who can join and issues scoped participant tokens. Keep
signing keys on the backend, never in browser code. The SDK connects with those
tokens; it does not provide an application login or fetch credentials for you.

The [quickstart](/react/quickstart) includes an installation path, complete React
code, two-participant checks, and a separately labeled local development sandbox.
For the ownership model behind the code, see [React concepts](/react/).
