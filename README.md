[Documentation](https://docs.pulsebeam.dev) | [Issues](https://github.com/PulseBeamDev/pulsebeam/issues) | [Discord](https://discord.com/invite/Bhd3t9afuB)

# PulseBeam

<img align="right" src="https://pulsebeam.dev/favicon.svg" height="64px" alt="PulseBeam Logo: Open Source WebRTC SFU for browsers, mobile, and IoT">

PulseBeam is an open source, general-purpose WebRTC SFU server for connecting browsers, mobile, and IoT clients. We believe real-time application development shouldn't be complicated, nor should it rely on heavy architectures with many moving parts. PulseBeam reduces this friction by adhering to these core design goals:

- Support all WebRTC clients.
- Keep the architecture simple, but not simpler.
- Natively support vertical and horizontal scaling.
- Provide client SDKs strictly for convenience, not necessity.
- Require minimal configuration.

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

The following quickstart assumes that you have a Linux machine. As a fallback, you can go to <https://pulsebeam.dev/#quickstart> and check the "fallback" toggle.

### Step 1. Run the PulseBeam Server

**Docker/Podman (recommended):**

```bash
docker run --rm --net=host ghcr.io/pulsebeamdev/pulsebeam:pulsebeam-v0.3.1 --dev
```

**Open Port Requirements:**

- **TCP/3000:** HTTP signaling
- **UDP/3478:** WebRTC traffic (Multiplexed)
- **TCP/3478:** WebRTC over TCP fallback (Multiplexed)

> **Note:** The `--dev` flag used above configures PulseBeam to use port **3478** to avoid requiring root privileges. In production (running without `--dev`), WebRTC traffic defaults to standard port **443** (UDP/TCP).

**Other options:**

- **Binary:** download from [Releases](https://github.com/pulsebeamdev/pulsebeam/releases/latest)
- **Source:** `cargo run --release -p pulsebeam`

### Step 2. Publish a video

Run the following snippet in the browser console:

```javascript
const pc = new RTCPeerConnection();
const stream = await navigator.mediaDevices.getUserMedia({ video: true });
const transceiver = pc.addTransceiver("video", {
  direction: "sendonly",
  // Define scalability layers (low, medium, high)
  sendEncodings: [
    { rid: "q", scaleResolutionDownBy: 4, maxBitrate: 150_000 },
    { rid: "h", scaleResolutionDownBy: 2, maxBitrate: 400_000 },
    { rid: "f", scaleResolutionDownBy: 1, maxBitrate: 1_250_000 },
  ],
});
transceiver.sender.replaceTrack(stream.getVideoTracks()[0]);

const offer = await pc.createOffer();
await pc.setLocalDescription(offer);

const res = await fetch("http://localhost:3000/api/v1/rooms/demo", {
  method: "POST",
  headers: { "Content-Type": "application/sdp" },
  body: offer.sdp,
});

await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
```

### Step 3. View the video stream

Go to <https://codepen.io/lherman-cs/pen/pvgVZar>, then put "demo" as the room to connect to.

## Profiling & Metrics

PulseBeam exposes an internal debug HTTP server on **`http://localhost:6060`**.

- **Metrics (Prometheus):** `http://localhost:6060/metrics`
- **CPU profile (pprof):** `http://localhost:6060/debug/pprof/profile?seconds=30`
- **CPU flamegraph:** `http://localhost:6060/debug/pprof/profile?seconds=30&flamegraph=true`
- **Memory profile:** `http://localhost:6060/debug/pprof/allocs`

> CPU profiling measures **CPU usage**, not wall time. For meaningful results, profile while the server is under load.

View CPU profiles with:

```bash
go tool pprof -http=:8080 cpu.pprof
```

Or view as a flamegraph on a browser by specifying `flamegraph=true` to the URL query.

## Roadmap

- ✅ Prototype: Rust SFU + demo apps
- ✅ Bandwidth estimator, simulcast support
- 🚧 Top-N audio selection, Data channel, Web Client SDK
- 📅 HTTP API & Webhooks (events)
- 📅 Multi-node / cascading SFU support
- 📅 Extensions: recording, SIP, AI agents

You can view the full roadmap [here](https://github.com/orgs/PulseBeamDev/projects/2/views/4).
