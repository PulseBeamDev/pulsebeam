<p align="center">
  <a href="https://pulsebeam.dev/">
    <img src="https://pulsebeam.dev/favicon.svg" width="140px" alt="PulseBeam" />
  </a>
</p>

<h1 align="center">PulseBeam</h1>

<p align="center">
  An open-source stack for real-time audio and video, with a focus on reliability and simplicity.
</p>

<p align="center">
  <a href="https://github.com/pulsebeamdev/pulsebeam/issues">Report a Bug</a>
  ·
  <a href="https://github.com/pulsebeamdev/pulsebeam/issues">Request a Feature</a>
  ·
  <a href="https://discord.gg/Bhd3t9afuB">Join our Discord</a>
</p>

## What is PulseBeam?

PulseBeam is an open-source media stack for building real-time applications. Our approach is guided by a focus on creating a robust, simple, and scalable foundation for developers.

At its heart is a minimal **Selective Forwarding Unit (SFU) written in Rust**. We've intentionally kept the core small, dedicated only to the essential task of routing real-time media. This design choice aims to create a stable and predictable system that is easy to reason about.

For all other features—like recording, analytics, or AI integrations—our design encourages building them as independent, server-side services that connect to the core, keeping the central media server stable and uncluttered.

### Our Design Philosophy

*   **A Minimal Core:** We believe a small, stable core is the best foundation for a reliable system. It's easier to maintain, debug, and build upon.
*   **Modular Features:** Our goal is to enable powerful functionality without creating a monolith. We favor a design where features are deployed as decoupled services.
*   **Simple, HTTP-Based Signaling:** We use a simple, request/response signaling protocol that is a superset of WHIP/WHEP. This provides out-of-the-box compatibility while allowing for extensions to handle advanced use cases—all without requiring WebSockets for signaling.
*   **Open and Transparent:** The project is open source, and building in Rust helps us create efficient and memory-safe code by design. We aim to be transparent in our work as we build the project with the community.

## A Simple Mental Model

The architecture of PulseBeam is straightforward. The core SFU sits in the middle and relays media. On either side, different kinds of clients connect to it.

```
+-----------------+      +--------------------+      +----------------------+
|                 |      |                    |      |                      |
| End-User Clients|      |                    |      | Server-Side Services |
| (Browsers,      |<---->| PulseBeam Core SFU |<---->| (Recording,          |
|  Mobile Apps,   |      |  (Media Relay)     |      |  AI, Analytics)      |
|  Embedded)      |      |                    |      |                      |
+-----------------+      +--------------------+      +----------------------+
```

Conceptually, we find it helpful to think of every client that connects to the core as an **"agent"** with a specific job.

From this perspective, a participant in a browser is simply an agent responsible for user interaction and media capture. A recording bot is an agent whose job is to persist media streams to disk.

This mental model helps us keep the core SFU incredibly simple. The core doesn't need to know about different "types" of clients; it just routes media between connected agents. This is the key to the simplicity and robustness we're striving for.


## Quickstart

You can get a PulseBeam server running locally in under a minute.

**Requirements:** Rust and Cargo.

```bash
# 1. Clone the repository
git clone https://github.com/pulsebeamdev/pulsebeam.git
cd pulsebeam

# 2. Run the server
cargo run --release
```

Next, open `http://localhost:7880/demo` in two browser tabs to see the media relay in action.

*(This is an early version of PulseBeam. Your feedback and contributions are incredibly valuable as we work towards building a truly reliable platform.)*


## API & Usage

PulseBeam's signaling is designed for both simplicity and flexibility. For basic media exchange, it's compatible with the WHIP/WHEP standard.

### Example: Connecting with WHIP/WHEP

This example uses browser-native JavaScript—no proprietary SDKs are required for basic media publishing and viewing.

```javascript
const pc = new RTCPeerConnection();

// Configure local media
const stream = await navigator.mediaDevices.getUserMedia({ video: true, audio: true });
pc.addTransceiver("video", { direction: "sendonly" }).sender.replaceTrack(stream.getVideoTracks()[0]);
pc.addTransceiver("audio", { direction: "sendonly" }).sender.replaceTrack(stream.getAudioTracks()[0]);

// Negotiate with the PulseBeam server
const offer = await pc.createOffer();
await pc.setLocalDescription(offer);

const res = await fetch("http://localhost:3000?room=test&participant=alice", {
  method: "POST",
  headers: { "Content-Type": "application/sdp" },
  body: offer.sdp,
});
await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
```

This simple, HTTP-based interaction model is the foundation for how all clients, including server-side services, will communicate with the core.

👉 For a complete implementation with UI controls, see the [demo](./demo) directory.

## Roadmap

Our roadmap reflects our focus on building a stable foundation first.

| Stage                      | Status & Focus                                                                                                              |
| :------------------------- | :-------------------------------------------------------------------------------------------------------------------------- |
| **Prototype**              | ✅ Minimal core SFU is functional with a working demo.                                                                      |
| **Core Relay Stability**   | 🚧 **Current Focus:** Hardening the core SFU. Our main effort is on improving stability, connection recovery, and resilience.    |
| **Signaling & Agent API**  | 📅 **Next:** Defining and documenting our full HTTP-based signaling API, ensuring it's robust for all types of clients.     |
| **Essential Services & SDKs**| 📅 **Planned:** Releasing official server-side services (e.g., recording) and a lightweight JavaScript SDK.                 |
| **Scaling & Deployment**   | 📅 **Future:** Developing guides and tools for multi-node deployments and high-availability configurations.                   |


## Join the Community

If our approach to building simpler, more robust real-time tools resonates with you, we'd love for you to get involved. By contributing, you can help shape a foundational piece of open infrastructure.

💬 **[Join us on Discord](https://discord.gg/Bhd3t9afuB)** to connect with the team and other developers.


## Licensing

Pulsebeam applies different licenses to different parts of the project:

- **Server components** are licensed under **AGPL-3.0**.  
  This license ensures that the central technology remains open and that improvements made to the server are contributed back to the community.

- **Client libraries and tooling** are licensed under **Apache-2.0**.  
  This license provides a permissive and widely adopted framework that makes it straightforward to integrate Pulsebeam into applications and infrastructure.

Each workspace member includes its own `LICENSE` file and declares its license in `Cargo.toml`.  
Those files are the authoritative source for the license of each component.

## Frequently Asked Questions

Can I use the Pulsebeam SFU for an internal project at my company?

> Yes, absolutely. The AGPL license does not restrict internal use.

I have more questions or need a different license for the server core. Who should I contact?

> We're happy to help. Please get in touch by emailing [lukas@pulsebeam.dev](mailto:lukas@pulsebeam.dev).

This dual-license model is our way of creating a sustainable open-source project that is both protected from corporate exploitation and friendly to our developer community.
