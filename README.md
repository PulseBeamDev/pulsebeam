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

---

## Quickstart

The easiest way to run PulseBeam is with Docker:

```bash
docker run --rm --net=host ghcr.io/pulsebeamdev/pulsebeam:pulsebeam-v0.1.8
````

Other ways to run:

* **Binary:** download from [Releases](https://github.com/pulsebeamdev/pulsebeam/releases)
* **Source:** `cargo run --release -p pulsebeam`


### Demo: Broadcast

Use browser-native APIs — no SDK lock-in:

#### Publisher (sends video)

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@picocss/pico@2/css/pico.min.css">
</head>
<body>
  <h3>Publisher</h3>
  <video id="localVideo" autoplay playsinline style="width:320px;height:240px;border:1px solid #ccc;"></video>
  <button id="startBtn">Start Publishing</button>
  <span id="status">Not connected</span>

  <script type="module">
    const status = document.getElementById("status");
    const localVideo = document.getElementById("localVideo");
    document.getElementById('startBtn').onclick = async () => {
      const pc = new RTCPeerConnection();

      const stream = await navigator.mediaDevices.getUserMedia({ video: true, audio: false });
      localVideo.srcObject = stream;

      pc.addTransceiver("video", { direction: "sendonly" }).sender.replaceTrack(stream.getVideoTracks()[0]);
      pc.onconnectionstatechange = () => status.textContent = pc.connectionState;

      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);

      const res = await fetch("http://localhost:3000/api/v1/rooms/demo", {
        method: "POST",
        headers: { "Content-Type": "application/sdp" },
        body: offer.sdp
      });

      await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
      console.log("Publishing started");
    };
  </script>
</body>
</html>
````

**Run it immediately:** [Open Publisher JSFiddle](https://jsfiddle.net/lherman/0bqe6xnv/)

---

#### Viewer (receives video)

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@picocss/pico@2/css/pico.min.css">
</head>
<body>
  <h3>Viewer</h3>
  <video id="remoteVideo" autoplay playsinline style="width:320px;height:240px;border:1px solid #ccc;"></video>
  <button id="viewBtn">Start Viewing</button>
  <span id="status">disconnected</span>

  <script type="module">
    const status = document.getElementById("status");
    const remoteVideo = document.getElementById("remoteVideo");
    document.getElementById('viewBtn').onclick = async () => {
      const pc = new RTCPeerConnection();
      pc.addTransceiver("video", { direction: "recvonly" });
      pc.ontrack = e => remoteVideo.srcObject = e.streams[0];
      pc.onconnectionstatechange = () => status.textContent = pc.connectionState;

      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);

      const res = await fetch("http://localhost:3000/api/v1/rooms/demo", {
        method: "POST",
        headers: { "Content-Type": "application/sdp" },
        body: offer.sdp
      });

      await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
      console.log("Viewing started");
    };
  </script>
</body>
</html>
```

**Run it immediately:** [Open Viewer JSFiddle](https://jsfiddle.net/lherman/xotv9h6m)

## Roadmap

* ✅ Prototype: working video-only Rust SFU + demo
* 🚧 Core stability: simulation testing, end-to-end tests (current focus)
* 📅 Bandwidth estimator, audio and data handling, video simulcast
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
