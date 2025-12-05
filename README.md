<h1 align="center">
  <a href="https://pulsebeam.dev"><img src="https://pulsebeam.dev/favicon.svg" alt="PulseBeam" height="250px"></a>
  <br>
  PulseBeam
  <br>
</h1>

# PulseBeam

**Open Source WebRTC SFU for browsers, mobile, and IoT.**  
Rust-based SFU core. HTTP signaling (no WebSockets). Modular, extensible architecture.  

[🐛 Report a Bug](https://github.com/pulsebeamdev/pulsebeam/issues) · [✨ Request a Feature](https://github.com/pulsebeamdev/pulsebeam/issues) · [💬 Discord](https://discord.gg/Bhd3t9afuB)

---

## Overview

PulseBeam is a **developer-first real-time media server** designed to be fast, reliable, and easy to extend:

- **Rust SFU core** — memory-safe, low-latency, and performant.  
- **HTTP signaling** — WHIP/WHEP-compatible, no WebSockets required.  
- **Client-agnostic** — any WebRTC-capable client can connect (browsers, mobile, embedded devices, bots).  
- **Modular architecture** — features like recording, analytics, or AI run as separate processes.  

> If your client speaks WebRTC, it can communicate with PulseBeam.

```

Clients (browsers, mobile, embedded, bots)
↕
PulseBeam SFU
↕
External modules (recorders, transcoding, AI)

````

---

## Compatibility

PulseBeam is opinionated about media handling to ensure **wide hardware support and predictable performance**:

- **Video:** H.264 Baseline profile up to Level 4.1  
- **Audio:** Opus  
- **Data Channel:** Planned  

---

## Quickstart

Getting started is easy: run the server, then connect a client.

### 1️⃣ Run the PulseBeam Server

**Docker (recommended):**

```bash
docker run --rm --net=host ghcr.io/pulsebeamdev/pulsebeam:pulsebeam-v0.1.13
````

Other options:

* **Binary:** download from [Releases](https://github.com/pulsebeamdev/pulsebeam/releases/latest)
* **Source:** `cargo run --release -p pulsebeam`

Keep the server running before connecting clients.

---

### 2️⃣ Connect a Client

PulseBeam works with **any WebRTC-capable client**.

#### Publisher Example

```javascript
const pc = new RTCPeerConnection();
const stream = await navigator.mediaDevices.getUserMedia({ video: true });
const trans = pc.addTransceiver("video", { direction: "sendonly" });
trans.sender.replaceTrack(stream.getVideoTracks()[0]);

const offer = await pc.createOffer();
await pc.setLocalDescription(offer);

const res = await fetch("http://localhost:3000/api/v1/rooms/demo", {
  method: "POST",
  headers: { "Content-Type": "application/sdp" },
  body: offer.sdp
});

await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
```

#### Viewer Example

```javascript
const pc = new RTCPeerConnection();
pc.addTransceiver("video", { direction: "recvonly" });
pc.ontrack = e => remoteVideo.srcObject = e.streams[0];

const offer = await pc.createOffer();
await pc.setLocalDescription(offer);

const res = await fetch("http://localhost:3000/api/v1/rooms/demo", {
  method: "POST",
  headers: { "Content-Type": "application/sdp" },
  body: offer.sdp
});

await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
```

> Tip: Use `<details>` in GitHub to collapse code blocks for cleaner readability.

---

## Roadmap

* ✅ Prototype: Rust SFU + demo apps
* 🚧 Bandwidth estimator, simulcast support
* 📅 Top-N audio selection, Data channel, Web Client SDK
* 📅 HTTP API & Webhooks (events)
* 📅 Multi-node / cascading SFU support
* 📅 Extensions: recording, SIP, AI

---

## License

* Server → AGPL-3.0
* Client libraries/tooling → Apache-2.0

For custom licensing, contact [lukas@pulsebeam.dev](mailto:lukas@pulsebeam.dev).

---

## Community

* 💬 [Discord](https://discord.gg/Bhd3t9afuB)
* 🐛 [Issues](https://github.com/pulsebeamdev/pulsebeam/issues)

PRs welcome.
