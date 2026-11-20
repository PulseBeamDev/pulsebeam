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
| Endpoint | `http://localhost:7070/api/v1/whip?room=demo` locally, or your server's HTTPS base URL plus `/api/v1/whip?room=demo` |
| Authorization | `Bearer <participant-token>` |
| Sources | Camera and microphone |

For a browser publisher, open the page over HTTPS or localhost, then start
capture from a user gesture. Give the client the endpoint and participant token,
and allow camera/microphone access.

Set `room` to an application room name such as `studio-a`. Room, participant,
and track names use 1 to 36 ASCII letters, digits, underscores, or hyphens.
Names are case-sensitive. The participant token supplies the publisher's
identity and must grant Join and Publish for the room.

If receivers need stable track names, set them in the endpoint URL. For
example, use `/api/v1/whip?room=demo&track=video.camera&track=audio.mic` for a
camera and microphone. You can set one `track=kind.name` per sending media
kind. Without an override, the publication uses that media section's sender
MID as its name, so a change in MID changes the name.

The publishing flow is:

1. Create a WebRTC offer with the media to send.
2. POST the SDP offer to `/api/v1/whip?room=demo` with `Content-Type: application/sdp`
   and the `Authorization: Bearer <participant-token>` header.
3. Apply the SDP answer from the response to the peer connection.
4. Retain the response's resource URL for teardown.

## Confirm media reaches a receiver

A local preview only confirms capture. To check delivery in an application,
use the [React SDK](/react/quickstart) to explicitly subscribe to the publisher's
tracks. Joining the room does not start playback.

Confirm that the receiver shows moving publisher video and plays microphone
audio. Browser autoplay policy may require a receiver-side user gesture.

### Receive an exact source with WHEP

With a WHEP client, supply one `track=room.participant.kind.name` selector per
active receiving media kind. For example, to receive only video from the
`camera` publication that `alice` publishes in `demo`, POST a video-only
receive offer to `/api/v1/whep?track=demo.alice.video.camera`. For audio and
video, repeat the `track` parameter with a selector for each kind.

The receiver token must grant Subscribe to each requested source. It does not
need Join or Discover. WHEP does not join a room or select other publications
if a source is unavailable. If the publisher stops and republishes, create a
new WHEP session. Use the opaque `Location` from the response to tear down the
session.

## Stop publishing

Stop capture to release camera and microphone tracks. DELETE the session's
resource URL using the participant token to tear down the WHIP session. A WHIP
client normally performs this protocol cleanup for you.

::: warning Development credentials
The local server's known development signing key is not a production credential.
Production tokens must come from a trusted backend and be scoped to the intended
room and participant.
:::
