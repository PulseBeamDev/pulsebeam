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

Getting started is a simple, step-by-step process. First, you'll run the server on your machine, and then you'll connect to it using the browser-based demos.

### Step 1: Run the PulseBeam Server

You must have the server running before the demo clients can connect. The easiest way to start it is with Docker.

Open your terminal and run the following command:

```bash
docker run --rm --net=host ghcr.io/pulsebeamdev/pulsebeam:pulsebeam-v0.1.12
```

This command starts the PulseBeam server, which is now listening for connections on your machine. Keep this terminal window running.

> **Other ways to run:**
>
> *   **Binary:** download from [Releases](https://github.com/pulsebeamdev/pulsebeam/releases/latest)
> *   **Source:** `cargo run --release -p pulsebeam`

### Step 2: Run the Browser Demo

With the server running, you can use the demo clients. The snippets below show the core JavaScript logic.

**Note:** The linked JSFiddles are configured to connect to `http://localhost:3000` and are intended to be run on the **same machine** as the server. See the section below for instructions on testing with other devices.

#### A. Start the Publisher

The publisher page accesses your webcam and sends the video stream to your local PulseBeam server.

*   **[Open Publisher Demo in JSFiddle](https://jsfiddle.net/lherman/0bqe6xnv/)**

Once the page loads, click **"Start Publishing"** to begin the stream.

```javascript
// Core publisher logic
const pc = new RTCPeerConnection();

// 1. Get user's video and add it to the connection
const stream = await navigator.mediaDevices.getUserMedia({ video: true });
const trans = pc.addTransceiver("video", { direction: "sendonly" });
trans.sender.replaceTrack(stream.getVideoTracks()[0]);

// 2. Create an SDP offer and send it to your local PulseBeam server
const offer = await pc.createOffer();
await pc.setLocalDescription(offer);
const res = await fetch("http://localhost:3000/api/v1/rooms/demo", {
  method: "POST",
  headers: { "Content-Type": "application/sdp" },
  body: offer.sdp
});

// 3. Set the remote description with PulseBeam's answer
const answer = await res.text();
await pc.setRemoteDescription({ type: "answer", sdp: answer });
```

---

#### B. Start the Viewer

The viewer page subscribes to the video stream. Open the link in a **new browser tab on the same machine** to test.

*   **[Open Viewer Demo in JSFiddle](https://jsfiddle.net/lherman/xotv9h6m)**

Click **"Start Viewing,"** and you should see the video from the publisher tab.

```javascript
// Core viewer logic
const pc = new RTCPeerConnection();

// 1. Set up the connection to receive video
pc.addTransceiver("video", { direction: "recvonly" });
pc.ontrack = e => remoteVideo.srcObject = e.streams[0]; // 'remoteVideo' is a <video> element

// 2. Create an SDP offer to signal intent to receive
const offer = await pc.createOffer();
await pc.setLocalDescription(offer);
const res = await fetch("http://localhost:3000/api/v1/rooms/demo", {
  method: "POST",
  headers: { "Content-Type": "application/sdp" },
  body: offer.sdp
});

// 3. Set the remote description with PulseBeam's answer
const answer = await res.text();
await pc.setRemoteDescription({ type: "answer", sdp: answer });
```

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
