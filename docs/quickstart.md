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

Room and participant identity come from the bearer token. Mint a development
token for the server running with `--dev`:

```bash
cargo run -q -p pulsebeam-cli -- token --room demo --participant publisher
```

This example uses WHIP, the HTTP/SDP publishing boundary. Paste it into a
browser console on localhost or an HTTPS page that permits access to the server:

```javascript
const token = "paste the development token here";
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
if (pc.iceGatheringState !== "complete") {
  await new Promise((resolve) => {
    pc.addEventListener("icegatheringstatechange", function gathered() {
      if (pc.iceGatheringState === "complete") {
        pc.removeEventListener("icegatheringstatechange", gathered);
        resolve();
      }
    });
  });
}

const res = await fetch("http://localhost:7070/api/v1/whip", {
  method: "POST",
  headers: {
    Authorization: `Bearer ${token}`,
    "Content-Type": "application/sdp",
  },
  body: pc.localDescription.sdp,
});
if (res.status !== 201) throw new Error(await res.text());
await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
```

WHIP publishes this video under the label `video`. It does not use native
Catalog, Intent or Mapping messages. Native SDK clients instead create a
connection through `/api/v1/native` and reconcile media over the reliable ordered
`v1/sys/signaling` data channel.

## Step 3 — Watch it

Connect a current PulseBeam SDK or Meet client to this server with a token for
room `demo` and a distinct participant identity. Select the publisher's `video`
track from its remote catalog. Old clients using the previous signaling schema
are not compatible.

## Where to go next

- **[Deployment](/deployment)** — put a domain, TLS, and a reverse proxy
  in front of the server for production.
- **[Introduction](/)** — the design choices behind PulseBeam and why it
  drops STUN, TURN, and WebSockets.
