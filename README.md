<p align="center">
  <a href="https://pulsebeam.dev/">
    <img src="https://pulsebeam.dev/favicon.svg" width="140px" alt="PulseBeam" />
  </a>
</p>

<h1 align="center">PulseBeam</h1>
<p align="center">
  <strong>A lightweight, open-source SFU for real-time communication.</strong>
</p>

---

## Overview

PulseBeam is a **lightweight, open-source SFU (Selective Forwarding Unit)** built in Rust. It helps you relay audio, video, and data between participants in real time—without the complexity of traditional solutions.  

At its core, PulseBeam is **minimal and reliable**, designed to do one thing well: efficient media relay. Everything else lives outside the core, powered by **agents**—server-side clients that connect in real time to extend functionality.  

Agents can do things like **record sessions, moderate content, run analytics, or even host AI models** that process streams in real time (transcription, translation, summarization, moderation, and more).  

PulseBeam speaks **WHIP/WHEP by default**, so existing clients just work, and extensions let you go further when you need them.  

---

## Quickstart

Requirements: Rust and Cargo.

```bash
git clone https://github.com/pulsebeamdev/pulsebeam.git
cd pulsebeam
cargo run
````

Open `http://localhost:7880/demo` in two browser tabs to test a basic audio/video relay.

*(Early version—your feedback and contributions are welcome!)*

---

## High-Level Architecture

PulseBeam follows a **minimal core + agents** model:

```
 Participants <--> PulseBeam Core SFU <--> Agents
 (Browsers,           (Media relay)       (Recording, Moderation,
  Mobile,                                  Analytics, AI, Integrations)
  Embedded)                              
```

Agents are just **clients with special jobs**. They connect and communicate in real time, but instead of sending a webcam feed, they can capture streams, generate them, analyze them, or feed them into other systems.

---

## Planned API

PulseBeam works with WHIP/WHEP clients as-is, while supporting extensions for more advanced workflows. Cross-platform SDKs are planned for handling advanced features like dynamic stream prioritization, reconnection, etc.

## Example: 1:1 Call with WHIP/WHEP

With PulseBeam, you can set up a working 1:1 call in just a few lines of JavaScript:

```javascript
const pc = new RTCPeerConnection();

// Local: send-only transceivers
const stream = await navigator.mediaDevices.getUserMedia({ video: true, audio: true });
pc.addTransceiver("video", { direction: "sendonly" })
  .sender.replaceTrack(stream.getVideoTracks()[0]);
pc.addTransceiver("audio", { direction: "sendonly" })
  .sender.replaceTrack(stream.getAudioTracks()[0]);
document.getElementById("local").srcObject = stream;

// Remote: recv-only transceivers
pc.addTransceiver("video", { direction: "recvonly" });
pc.addTransceiver("audio", { direction: "recvonly" });
const remote = new MediaStream();
pc.ontrack = e => remote.addTrack(e.track);
document.getElementById("remote").srcObject = remote;

// Negotiate with PulseBeam
const offer = await pc.createOffer();
await pc.setLocalDescription(offer);
const res = await fetch("http://localhost:7880?room=test&participant=alice", {
  method: "POST", headers: { "Content-Type": "application/sdp" }, body: offer.sdp,
});
await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
```

👉 See [demo](./demo) for a complete HTML + TypeScript demo with controls.

---

## Roadmap

| Stage                      | Status & Focus                                                 |
| -------------------------- | -------------------------------------------------------------- |
| **Prototype**              | ✅ Minimal core relay running, demo available                   |
| **Core Relay**             | 🚧 Improving stability & robustness                            |
| **Agent Modules**          | Planned — recording, moderation, analytics, AI                 |
| **SDKs & Integrations**    | Planned — JavaScript, embedded, and native SDKs                |
| **Scaling & Optimization** | Planned — cascading SFUs, performance tuning, deployment tools |

---

## Contributing

You can help shape PulseBeam in many ways:

* Improve the Rust core
* Prototype new agent modules
* Build SDKs (JavaScript, embedded, native)
* Write docs, tests, or tutorials

---

## Join the Community

PulseBeam is for developers who want real-time communication that’s **lightweight, open, and extensible.**

By contributing early, you’re not just adding code—you’re shaping the foundation of a project that makes programmable video/audio simpler for everyone.

✨ Try it, share feedback, or contribute. Every bit helps.

💬 Join us on [Discord](https://discord.gg/Bhd3t9afuB) to connect with other developers and contributors.

---

## License

PulseBeam server is open source under the GNU Affero General Public License Version 3 (AGPLv3).