# PulseBeam

**Lightweight end-to-end stack for real-time video/audio/data.**  
Rust WebRTC SFU core. HTTP signaling (no WebSockets). Modular services.  

[Report a Bug](https://github.com/pulsebeamdev/pulsebeam/issues) · [Request a Feature](https://github.com/pulsebeamdev/pulsebeam/issues) · [Discord](https://discord.gg/Bhd3t9afuB)

---

PulseBeam is an opinionated **real-time media stack** designed to be simple, reliable, and easy to extend.  

- **Rust SFU core** – fast, memory-safe, no garbage collector pauses.  
- **HTTP signaling** – WHIP/WHEP-compatible, extended for multi-party and server-side use cases. No WebSockets required.  
- **SDK optional** – any client that speaks WebRTC (browsers, mobile, embedded, bots) can connect directly; thin SDKs will exist for convenience. 
- **Modular by design** – features like recording, analytics, or AI live outside the core as separate processes.  

If your client can do WebRTC, it can talk to PulseBeam.

```
Clients (browsers, mobile, embedded, bots)
          ↕
      PulseBeam
          ↕
 External agents (recorders, transcoding, AI)
```

## Compatibility

To ensure wide hardware acceleration support, compatibility with embedded devices, and a minimal feature set, PulseBeam is opinionated about its media handling:

* **Video**: H.264 Baseline profile up to Level 4.1.
* **Audio**: Opus.
* **Data Channel**: Not yet supported.

## Quickstart

The easiest way to run PulseBeam is with Docker:

```bash
docker run --rm --net=host ghcr.io/pulsebeamdev/pulsebeam:pulsebeam-v0.1.12
````

Other ways to run:

* **Binary:** download from [Releases](https://github.com/pulsebeamdev/pulsebeam/releases)
* **Source:** `cargo run --release -p pulsebeam`


### Demo: Broadcast

The following snippets demonstrate how to use the browser-native WebRTC API to interact with PulseBeam. The HTML and UI code has been removed for clarity.

Full, runnable examples are available in the JSFiddle links.

#### Publisher (sends video)

This snippet shows the core JavaScript logic for publishing a video stream.

```javascript
const pc = new RTCPeerConnection();

const stream = await navigator.mediaDevices.getUserMedia({ video: true });
pc.addTransceiver("video", { direction: "sendonly" }).sender.replaceTrack(stream.getVideoTracks()[0]);

const offer = await pc.createOffer();
await pc.setLocalDescription(offer);

const res = await fetch("http://localhost:3000/api/v1/rooms/demo", {
  method: "POST",
  headers: { "Content-Type": "application/sdp" },
  body: offer.sdp
});

await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
```

**See the full example:** [Open Publisher JSFiddle](https://jsfiddle.net/lherman/0bqe6xnv/) 

#### Viewer (receives video)

```javascript
const pc = new RTCPeerConnection();

pc.addTransceiver("video", { direction: "recvonly" });
pc.ontrack = e => remoteVideo.srcObject = e.streams[0]; // 'remoteVideo' is a <video> element

const offer = await pc.createOffer();
await pc.setLocalDescription(offer);

const res = await fetch("http://localhost:3000/api/v1/rooms/demo", {
  method: "POST",
  headers: { "Content-Type": "application/sdp" },
  body: offer.sdp
});

await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
```

**See the full example:** [Open Viewer JSFiddle](https://jsfiddle.net/lherman/xotv9h6m) 

## Roadmap

* ✅ Prototype: working basic audio/video Rust SFU + demo
* 🚧 Core stability: simulation testing, end-to-end tests (current focus)
* 📅 Bandwidth estimator, data handling, video simulcast
* 📅 First-party services (recording, etc.) + JS SDK
* 📅 Built-in multi node or cascading SFU.

---

## License

* **Server** → AGPL-3.0
* **Client libraries/tooling** → Apache-2.0

Internal/company use is fine.
Need a different license? → [lukas@pulsebeam.dev](mailto:lukas@pulsebeam.dev)

---

## Community

* 💬 [Discord](https://discord.gg/Bhd3t9afuB)
* 🐛 [Issues](https://github.com/pulsebeamdev/pulsebeam/issues)

PRs welcome.
