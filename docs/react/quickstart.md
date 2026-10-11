---
title: React SDK quickstart
description: Run the checked browser example with two participants in a local room.
---

# React SDK quickstart

This framework-independent browser example captures camera and microphone,
publishes them, renders discovered remote video, and receives automatic audio.
Read [concepts](/react/) for the ownership model.

## Prerequisites

- A repository checkout and the [Bazel build prerequisites](/build).
  Run commands below from the repository root on a supported Linux host.
- A browser with WebRTC, WebAssembly, `navigator.mediaDevices.getUserMedia`,
  and Web Audio. The repository browser gate uses pinned Chromium; this is
  not a compatibility promise for every browser.
- HTTPS or localhost for capture, with camera/microphone permission.
  A secure page cannot use an insecure remote server through mixed content.
  A non-local HTTP page is not a capture environment.
- React 18 or newer is the declared peer range. The checked example uses
  React and React DOM 18.3.1; keep their versions compatible.
- A reachable PulseBeam HTTP(S) endpoint and its media ports. See
  [deployment](/deployment) for production TLS/firewalls. Use the base endpoint,
  such as `http://localhost:7070`, not `/api/v1/native`.

## 1. Build the available packages and example

Do not assume the package version in repository metadata is published to npm.
The reproducible distribution path here is a **local Bazel package build**:

```bash
./bazel build //agents/react:package //agents/pulsebeam-agent-web:package
./bazel run //agents/react:quickstart_dev
```

The second command builds and serves this page's actual example using the
built public packages and the repository's locked dependencies, without
fetching `@pulsebeam/react` from a public registry. Open the localhost URL it
prints in two tabs. This runner is a development convenience, not a required
React framework.

To integrate into an existing bundled browser application, build the same
packages and add them from their absolute local paths. With pnpm 10.12.4,
put an application-level override in your application's package.json
(replace the checkout path with your own):

```json
{
  "pnpm": {
    "overrides": {
      "@pulsebeam/web": "file:/absolute/checkout/bazel-bin/agents/pulsebeam-agent-web/package"
    }
  }
}
```

Then install the local React package alongside compatible React/React DOM:

```bash
pnpm add file:/absolute/checkout/bazel-bin/agents/react/package react@18.3.1 react-dom@18.3.1
```

The override supplies the React package's internal Web dependency from the same
checkout. Application imports still use only `@pulsebeam/react`. Rebuild and
reinstall local packages when changing SDK versions. Do not edit generated
Bazel output.

Your bundler must emit and serve the Web package's
`dist/wasm/pulsebeam_agent_web_bg.wasm` at the URL resolved by its generated
JavaScript. The example bundler copies it next to the bundled module.
A 404, HTML fallback, CSP denial, or blocked WASM initialization is a setup
failure, not a camera-permission problem. See [troubleshooting](/react/troubleshooting#package-and-wasm).

## 2. Start a matching development server

In another terminal:

```bash
./bazel run //:server -- --dev
```

This serves signaling on `http://localhost:7070` and uses unprivileged media
port 3478. Do not use this development authentication profile in production.

## 3. Acquire two distinct participant tokens

```bash
./bazel run //:cli -- token --room react-demo --participant alice
./bazel run //:cli -- token --room react-demo --participant bob
```

Paste Alice's output into tab A and Bob's into tab B. Both use the same room,
but must not reuse a participant credential concurrently. The CLI's default
token lifetime is one hour. Do not commit tokens or put signing keys in browser
code. A pasted development token is only a local stand-in for your authenticated
backend's token response.

## 4. Run the complete browser integration

The runner serves the following two source files. The site includes them
directly, and the example gate type-checks them against built public declarations.

### Room.tsx

<<< ./Room.tsx

### main.tsx

<<< ./main.tsx

The HTML entry contains `<div id="root"></div>` and a module script for the
bundled `main.js`. For SSR frameworks, mount this application in a client-only
boundary. Do not execute capture, Web Audio, or WASM/media initialization on
the server. Render the null/preparing state until `useAgent` creates its Agent.

## 5. Observe media and leave

1. In both tabs, enter the endpoint and respective token, then **Open room**.
2. Click **Join** in each. Wait for **Connection: connected** and the expected
   admitted participant name. Intent alone is not successful admission.
3. Click **Enable camera and microphone** in each and accept permission.
   **Capture: active** and the local preview confirm capture, not remote delivery.
4. Each tab should list the other's `camera` publication and render a moving
   remote video. The remote audio publication count should become one.
   Click **Enable remote audio** if the browser blocked automatic playback.
   Use headphones to avoid feedback between two tabs. A count does not prove
   audible signal; speak into the other tab's microphone.
5. **Leave and stop capture** disconnects and releases devices. The other tab's
   remote publication disappears. **Unmount room** also closes the hook-owned
   Agent and stops capture. No device should remain active after unmount.

The capture hook and source-binding effect perform cleanup independently:
disconnect alone would not release capture. The example does not automatically
retry capture errors or terminal connection failures.

## Production handoff

Your application backend must authenticate the user, choose the room and
participant identity, and return a scoped token. Fetch that token over your
authenticated application API before mounting the room, never mint it using
a browser signing secret.

Observe `authorizationExpiresAt` (Unix seconds) and renew before expiry through
your backend. Update the config's token without changing endpoint/topology/logging
to renew the same Agent. The SDK does not fetch tokens for you.
A new token must authorize the existing session identity; use a new Agent for a
different participant/room. Denied admission or terminal expiry needs explicit
diagnosis and a fresh authorized session, not endless reconnects.

Continue with [guides](/react/guides), [reference](/react/reference), or
[symptom-based troubleshooting](/react/troubleshooting).
