# PulseBeam

<img align="right" src="https://pulsebeam.dev/favicon.svg" height="48px" alt="PulseBeam Logo: Open Source WebRTC SFU for browsers, mobile, and IoT">

PulseBeam is an open source, general-purpose WebRTC SFU server for connecting browsers, mobile, and IoT clients. We believe real-time application development shouldn't be complicated, nor should it rely on heavy architectures with many moving parts. PulseBeam reduces this friction by adhering to these core design goals:

* Support all WebRTC clients.
* Keep the architecture simple, but not simpler.
* Natively support vertical and horizontal scaling.
* Provide client SDKs strictly for convenience, not necessity.
* Require minimal to zero configuration.

If your client device speaks WebRTC, it can communicate with PulseBeam.

## Compatibility

PulseBeam is opinionated about media handling to prioritize battery efficiency, hardware support, and predictable performance:

- **Video:** H.264 Baseline profile up to Level 4.1  
- **Audio:** Opus  
- **Data Channel:** Planned  

## Architecture

The architecture is highly inspired by [SDN](https://en.wikipedia.org/wiki/Software-defined_networking) architecture.

![architecture](./docs/architecture.svg)

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
* ✅ Bandwidth estimator, simulcast support
* 🚧 Top-N audio selection, Data channel, Web Client SDK
* 📅 HTTP API & Webhooks (events)
* 📅 Multi-node / cascading SFU support
* 📅 Extensions: recording, SIP, AI

---

## License

* Server → AGPL-3.0
* Client libraries/tooling → Apache-2.0

---

## Community

* 💬 [Discord](https://discord.gg/Bhd3t9afuB)
* 🐛 [Issues](https://github.com/pulsebeamdev/pulsebeam/issues)

PRs welcome.
