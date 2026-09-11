---
title: Quickstart
description: Run a PulseBeam SFU and stream your first video in a couple of minutes.
---

# Quickstart

PulseBeam is a single binary. There's no signaling cluster to stand up, no STUN
or TURN to configure, and no database. Run the server, point a WebRTC client at
it, and you have a room. This page gets you from nothing to a live video stream.

::: info
You'll need a Linux host to run the server. If you just want to see it work
without installing anything, use the hosted demo at
[pulsebeam.dev](https://pulsebeam.dev/#quickstart).
:::

## Step 1 — Run the server

The easiest way is Docker (or Podman) with host networking:

```bash
docker run --rm --net=host ghcr.io/pulsebeamdev/pulsebeam:pulsebeam-v0.4.6 --dev
```

That's it — the server is now serving its HTTP signaling API on
`http://localhost:7070`.

::: info
The `--dev` flag runs WebRTC on port **3478** so you don't need root. In
production (without `--dev`), WebRTC uses the standard port **443**. See
[Deployment](/deployment) for the full port and firewall list.
:::

Prefer not to use Docker? Grab a prebuilt binary from the
[releases page](https://github.com/pulsebeamdev/pulsebeam/releases/latest), or
build from source with `cargo run --release -p pulsebeam`.

## Step 2 — Publish a stream

A room is just a name in the URL — you never create it ahead of time. The first
participant to POST an SDP offer to `rooms/{room}/participants` brings the room
into existence.

You don't need an SDK to try this. Paste the following into your browser's
console to publish your webcam to a room called `demo`:

```javascript
const pc = new RTCPeerConnection();
const stream = await navigator.mediaDevices.getUserMedia({ video: true });

// Simulcast: send three quality layers so the SFU can forward the right one
// to each viewer based on their available bandwidth.
const transceiver = pc.addTransceiver("video", {
  direction: "sendonly",
  sendEncodings: [
    { rid: "q", scaleResolutionDownBy: 4, maxBitrate: 150_000 },
    { rid: "h", scaleResolutionDownBy: 2, maxBitrate: 400_000 },
    { rid: "f", scaleResolutionDownBy: 1, maxBitrate: 1_250_000 },
  ],
});
transceiver.sender.replaceTrack(stream.getVideoTracks()[0]);

const offer = await pc.createOffer();
await pc.setLocalDescription(offer);

// The entire join handshake is a single HTTP request — no WebSocket.
const res = await fetch("http://localhost:7070/api/v1/rooms/demo/participants", {
  method: "POST",
  headers: { "Content-Type": "application/sdp" },
  body: offer.sdp,
});

await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
```

You just used the whole connection protocol: `POST` an offer, get an answer
back, done. Everything after this — which streams you want, at what resolution —
flows over the WebRTC data channel, not more HTTP calls.

## Step 3 — Watch it

Open the [viewer on CodePen](https://codepen.io/lherman-cs/pen/pvgVZar) and join
the room `demo`. Your webcam feed should appear.

## Where to go next

- **[Deployment](/deployment)** — put a domain, TLS, and a reverse proxy
  in front of the server for production.
- **[Introduction](/)** — the design choices behind PulseBeam and why it
  drops STUN, TURN, and WebSockets.
