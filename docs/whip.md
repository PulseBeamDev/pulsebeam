---
title: WHIP publishing
description: Publish camera and microphone media with a WHIP-compatible client.
---

# Publish with WHIP

Use this path for a client that already supports the WHIP HTTP/SDP publishing
protocol. To add an interactive room to a React app, use the
[React SDK quickstart](/react/quickstart) instead; it handles signaling for you.

## Before you start

You need a running PulseBeam server that supports WHIP and a participant token
for the room you want to publish to. Your backend issues production tokens;
never place signing secrets in a browser.

For a local test setup, follow the
[React quickstart's media-server setup](/react/quickstart#1-run-the-pulsebeam-media-server). Its
`http://localhost:7070` server and development participant tokens can be reused
here. Do not start another server or switch rooms between publishing and playback.
For production operation, see [deployment](/deployment).

## Publish camera and microphone

With a WHIP-compatible client, configure:

| Setting | Value |
| --- | --- |
| Endpoint | `http://localhost:7070/api/v1/whip` locally, or your server's HTTPS base URL plus `/api/v1/whip` |
| Authorization | `Bearer <participant-token>` |
| Sources | Camera and microphone |

For a browser publisher, open the page over HTTPS or localhost, then start
capture from a user gesture. Give the client the endpoint and participant token,
and allow camera/microphone access.

The publishing flow is:

1. Create a WebRTC offer with the media to send.
2. POST the SDP offer to `/api/v1/whip` with `Content-Type: application/sdp`
   and the `Authorization: Bearer <participant-token>` header.
3. Apply the SDP answer from the response to the peer connection.
4. Retain the response's resource URL for teardown.

## Confirm media reaches a receiver

A local preview only confirms capture. To check delivery, join **the same server
and room** with a different participant token in a receiving application. The
[React quickstart](/react/quickstart#4-try-it) provides one:
use your existing endpoint and a token for another participant rather than
creating a second sandbox.

Confirm that the receiver shows moving publisher video and plays microphone
audio. Browser autoplay policy may require a receiver-side user gesture.

## Stop publishing

Stop capture to release camera and microphone tracks. DELETE the session's
resource URL using the participant token to tear down the WHIP session. A WHIP
client normally performs this protocol cleanup for you.

::: warning Development credentials
The local server's known development signing key is not a production credential.
Production tokens must come from a trusted backend and be scoped to the intended
room and participant.
:::
