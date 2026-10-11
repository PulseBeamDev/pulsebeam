---
title: React SDK quickstart
description: Share camera and microphone between two browser tabs with the checked React example.
---

# React SDK quickstart

Run a complete React room in two browser tabs. Each participant will join,
share camera and microphone, see the other's video, and leave without keeping
the devices active.

The example uses plain React, not a specific framework. You can run it before
reading the [concepts](/react/).

## Before you start

| You need | Check |
| --- | --- |
| Repository checkout on a supported Linux host | Install the [Bazel build prerequisites](/build). Run all commands from the repository root. |
| Camera and microphone | Allow browser access. Use headphones to avoid feedback between tabs. |
| Browser media support | WebRTC, WebAssembly, Web Audio, and `navigator.mediaDevices.getUserMedia` must be available. |
| Secure capture page | Use HTTPS or localhost. An HTTPS page cannot connect to an insecure remote endpoint. |

The checked example uses React and React DOM 18.3.1 with pinned Chromium.
The package declares React 18 or newer as its peer range; this is not a tested
compatibility promise for every browser or React version.

## 1. Start the development server

In a terminal:

```bash
./bazel run //:server -- --dev
```

Keep it running. The HTTP endpoint is `http://localhost:7070`, and the
development media port is 3478. Use the base endpoint, not `/api/v1/native`.
The `--dev` authentication profile is for local testing, not production.

## 2. Open the example in two tabs

In another terminal:

```bash
./bazel run //agents/react:quickstart_dev
```

Open the localhost URL it prints in **tab A** and **tab B**. You should see
the endpoint and participant-token form.

The runner builds the local SDK packages, uses locked dependencies, and serves
the complete example below. It does not fetch `@pulsebeam/react` from npm.
Package metadata alone does not establish public registry availability.

## 3. Create a token for each participant

```bash
./bazel run //:cli -- token --room react-demo --participant alice
./bazel run //:cli -- token --room react-demo --participant bob
```

| Tab | Endpoint | Token |
| --- | --- | --- |
| A | `http://localhost:7070` | Output for `alice` |
| B | `http://localhost:7070` | Output for `bob` |

Paste the tokens into their tabs and click **Open room**. Both tokens name
the same room, but each Agent needs a different participant credential.
The CLI's default token lifetime is one hour.

Do not commit tokens or put signing keys in browser code. These pasted tokens
stand in for a production backend response.

## 4. Join and share media

Do each action in both tabs.

| Action | Expected result |
| --- | --- |
| Click **Join** | `Connection: connected` and the expected participant name |
| Click **Enable camera and microphone** and allow access | `Capture: active` and a local preview |
| Watch the other tab's publication | The other's `camera` appears and its remote video moves |
| Speak into the microphone | The other tab receives audio; the remote audio publication count becomes one |

A connected status confirms admission. A local preview confirms capture.
Neither proves that the other participant received media.

If audio is blocked, click **Enable remote audio**. If video playback is
blocked, use **Retry video playback**. A publication count alone does not
prove sound; listen in the other tab.

## 5. Leave and release devices

1. Click **Leave and stop capture** in tab A. It disconnects and stops capture.
   Its publications should disappear from tab B.
2. Click **Unmount room** in each tab. The hook-owned Agent closes and capture stops.
3. Check that the camera and microphone are no longer active.

Disconnect alone does not release borrowed capture. This example stops it
explicitly. It reports capture and connection failures rather than retrying
them blindly.

## Common blockers

| Symptom | First check |
| --- | --- |
| Capture fails | Browser/site permission and HTTPS or localhost |
| Join fails | Base endpoint, token expiry, and distinct participant credentials |
| Connected, but no remote video | Both tabs published; remote video is visible and playback is allowed |
| Remote audio is silent | Click **Enable remote audio**, then speak; check browser/OS output |
| SDK does not initialize | JavaScript and WASM asset URLs; a WASM request must not return HTML |

Follow [troubleshooting](/react/troubleshooting) for detailed checks.
For a remote server, also verify its reachable media ports, TLS, and firewall
settings in [deployment](/deployment).

## Complete source

These are the files served by the runner. The site includes the actual source,
and `//agents/react:quickstart_types` checks it against the built public package.

### Room.tsx

<<< ./Room.tsx

### main.tsx

<<< ./main.tsx

The HTML entry supplies `<div id="root"></div>` and loads the bundled
`main.js`. In an SSR framework, mount the room in a client-only boundary.
Keep capture, Web Audio, and WASM/media initialization out of server execution.
Show the preparing state while `useAgent` returns null.

## Use the SDK in your app

The runner is the recommended first test. For an existing bundled browser app,
use local packages from the same checkout:

```bash
./bazel build //agents/react:package //agents/pulsebeam-agent-web:package
```

With pnpm 10.12.4, add this application-level override to your
`package.json`. Replace `/absolute/checkout` with your checkout's absolute path.

```json
{
  "pnpm": {
    "overrides": {
      "@pulsebeam/web": "file:/absolute/checkout/bazel-bin/agents/pulsebeam-agent-web/package"
    }
  }
}
```

Then install the React package and compatible React dependencies:

```bash
pnpm add file:/absolute/checkout/bazel-bin/agents/react/package react@18.3.1 react-dom@18.3.1
```

The override supplies the internal Web dependency. App imports still come
from `@pulsebeam/react`. Rebuild and reinstall local packages when changing
SDK versions; do not edit generated Bazel output.

::: warning Serve the WASM asset
Your bundler must serve `dist/wasm/pulsebeam_agent_web_bg.wasm` from the Web
package at the URL resolved by its generated JavaScript. The example copies
it next to the bundled module. Do not assume another bundler does this.
See [package and WASM checks](/react/troubleshooting#package-and-wasm).
:::

## Production handoff

Before mounting the room, fetch a scoped token from your authenticated backend.
The backend chooses the room and participant identity and keeps the signing key.

Renew before `authorizationExpiresAt` (Unix seconds). Update only the config's
token to keep the same Agent. The SDK does not fetch new tokens for you.
Use a new Agent for a different room or participant. See
[connection and renewal](/react/guides#connection-renewal-and-errors).

## Next step

Choose a task in the [integration guides](/react/guides), or look up an exact
contract in the [API reference](/react/reference).
