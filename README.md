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

To add a room to your application, start with the
[React SDK quickstart](docs/react/quickstart.md). It uses the published SDK and
shows how to connect two participants in your own app. For other browser UIs,
use the [Web SDK](agents/pulsebeam-agent-web/README.md).

## Technical Targets

- CPU induced p99 jitter: <2ms

## Compatibility

PulseBeam is opinionated about media handling to prioritize battery efficiency, hardware support, and predictable performance:

- **Video:** H.264 Baseline profile up to Level 4.1
- **Audio:** Opus
- **Data channels:** Native SDK signaling and reliable/unreliable topics are implemented. WHIP publishing is media-only.

## Development

Bazel owns compilation, dependency/tool provisioning, generated bindings, checks,
and tests. Normal development and browser acceptance support Linux x86_64.
Install the [baseline OS prerequisites](docs/build.md#host-prerequisites), then:

```bash
./bazel test //:fast                  # Static checks and fast tests
./bazel test //:test                  # Complete non-privileged acceptance
./bazel build //:server //:native //:web //:react //:meet //:docs
```

The checksum-verified launcher bootstraps the pinned Bazel version. Bazel supplies
Rust 1.99.0, native compilation tools, Node/pnpm, matching WASM generators/runtime,
protobuf, Go, Python, and the pinned Chrome/Firefox executables and drivers.
Initial downloads require network access; tests use local application fixtures.
No pre-existing language/browser cache or independent tool installation is required.

See [build and development](docs/build.md), [editor-neutral LSP setup](docs/ide.md),
and [distribution](docs/releasing.md). Cargo/pnpm metadata remains authoritative
for dependencies and editor maintenance, not a supported parallel build/test workflow.

## Local development demo

See video forwarded through a local server using [PulseBeam Meet](apps/meet/README.md).
Both browser tabs publish their camera and receive the other participant's video.

Use a Linux x86_64 checkout with the [host prerequisites](docs/build.md#host-prerequisites)
installed. Run commands from the repository root. You need a browser and working
camera and microphone capture; Meet requires both even when joining to view media.

### 1. Start the server

Keep this command running in its own terminal:

```bash
./bazel run --config=dev //:server -- --dev
```

Development mode uses fixed, public development credentials and unprivileged
WebRTC ports. Use it only for local testing, not an internet-facing deployment.

| Port | Purpose | Access |
| --- | --- | --- |
| TCP/7070 | HTTP signaling | Local clients in this quickstart |
| UDP/3478 | WebRTC traffic | Local clients in this quickstart |
| TCP/3478 | WebRTC over TCP fallback | Local clients in this quickstart |
| TCP/6060 | Internal metrics, health checks, and CPU profiling | Private only; never add to the public firewall allowlist |

Without `--dev`, WebRTC uses UDP/TCP 443 and the server requires a public `key-registry.json` for its single project. Development mode does not restrict listener access to loopback; keep
these services protected by your host firewall.

### 2. Start Meet and mint two tokens

In a second terminal, keep the Meet development server running:

```bash
./bazel run //:meet_dev -- --port 3000
```

When Next.js reports that it is ready, open <http://localhost:3000>. Use this
localhost origin so the browser can capture media without a separate HTTPS setup.

In a third terminal, mint tokens for two distinct participants in the same room:

```bash
./bazel run //:cli -- token --room demo --participant alice
./bazel run //:cli -- token --room demo --participant bob
```

These tokens expire after one hour. Generate new ones if they expire; do not use
the CLI's development tokens for production.

### 3. Publish and view

1. In the first Meet tab, allow camera and microphone access and confirm that
   the camera preview appears.
2. Paste Alice's token into **Bearer token**, set **Server URL** to
   `http://localhost:7070`, and select **Join room**.
3. Open <http://localhost:3000> in a second tab. Repeat with Bob's token and the
   same server URL. Mute the microphones in both tabs to avoid audio feedback.
4. Each tab should show its own participant identity, `Room demo`, a `connected`
   status, and a remote video tile in addition to its local camera. Select the
   remote tile to view the other participant's video. Send a message through
   **Chat** to try a reliable topic.

If **Join room** stays disabled, enable camera and microphone permissions and
select **Retry** after a capture error. If admission fails, check that the server
is running in `--dev` mode, the server URL is correct, and both tokens are fresh.

### 4. Clean up

Select **Leave** in each tab, then close the tabs. Leave disconnects the client
and stops its captured media, but returning to the lobby requests capture again.
Close the tabs to release the devices, then stop Meet and the server with Ctrl+C
in their terminals.

For your own client, start with the [Web SDK](agents/pulsebeam-agent-web/README.md)
or [React SDK](agents/react/README.md). For packaged server artifacts, see
[distribution](docs/releasing.md).

## Profiling & Metrics

PulseBeam exposes an internal debug HTTP server on **`http://localhost:6060`**.
Keep port 6060 private; the server does not restrict its listener to loopback.
Use local or trusted administrative access, never a public endpoint.

- **Metrics (Prometheus):** `http://localhost:6060/metrics`
- **CPU profile (pprof):** `http://localhost:6060/debug/pprof/profile?seconds=30`
- **CPU flamegraph:** `http://localhost:6060/debug/pprof/profile?seconds=30&flamegraph=true`

Memory profiling is currently disabled; `/debug/pprof/allocs` is not available.

> CPU profiling measures **CPU usage**, not wall time. For meaningful results, profile while the server is under load.

View CPU profiles with:

```bash
./bazel run @rules_go//go -- tool pprof -http=:8080 cpu.pprof
```

Or view as a flamegraph on a browser by specifying `flamegraph=true` to the URL query.

## Roadmap

You can view the full roadmap [here](https://github.com/orgs/PulseBeamDev/projects/2/views/4).
