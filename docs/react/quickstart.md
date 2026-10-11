---
title: React SDK quickstart
description: Run two tabs, share your camera, and hear the other participant.
---

# Your first React room

**Open two tabs. Wave at the camera. See the wave arrive in the other tab.**

You need a Linux x86_64 host, a [build-ready checkout](/build), a camera,
a microphone, and headphones. Run commands from the repository root.

## 1. Start PulseBeam

Keep this terminal running:

```bash
./bazel run //:server -- --dev
```

This is a local development server, not a production authentication setup.

## 2. Give each tab an identity

In a second terminal, create two tokens, then start the React example:

```bash
./bazel run //:cli -- token --room react-demo --participant alice
./bazel run //:cli -- token --room react-demo --participant bob

./bazel run //agents/react:quickstart_dev
```

Open the printed localhost URL in two tabs. In each, keep the endpoint
`http://localhost:7070`, paste its token, and click **Open room**:

| Tab A | Tab B |
| --- | --- |
| Token for `alice` | Token for `bob` |

Same room, different participant credentials. Keep tokens private; signing
keys never belong in browser code. The runner uses local SDK packages, not npm.

## 3. Wave and listen

In **both tabs**:

1. Click **Join**. Look for `Connection: connected` and your participant name.
2. Click **Enable camera and microphone**. Allow browser access; your preview appears.
3. Wave. The other tab should show your moving video. Speak and listen there too.

**That is the working connection:** media traveled from one participant to another,
not just into a local preview.

No sound? Click **Enable remote audio**. If video playback is blocked, click
**Retry video playback**. A publication count alone does not prove playback.

## 4. Leave

Click **Leave and stop capture**. Your publications disappear from the other tab.
Click **Unmount room** in both tabs; camera and microphone should no longer be active.
Disconnecting alone would not stop capture.

## Something didn't work?

- [Camera or microphone blocked](/react/troubleshooting#permission-and-capture-failures)
- [Can't join](/react/troubleshooting#admission-and-connection)
- [Connected, but no video or sound](/react/troubleshooting#missing-media)

## Make it yours

[Understand the three moving parts](/react/) or
[add screen sharing, controls, and messages](/react/guides).

### Complete source

::: details Open the complete React app
These are the files served by the runner.

#### Room.tsx

<<< ./Room.tsx

#### main.tsx

<<< ./main.tsx

The HTML entry supplies `<div id="root"></div>` and loads bundled `main.js`.
For SSR, mount the room only in the browser; do not initialize capture, Web Audio,
or the WASM runtime on the server. `useAgent` can initially return null.
:::

## Use the SDK in your app

::: details Install local packages in an existing app
From the same checkout:

```bash
./bazel build //agents/react:package //agents/pulsebeam-agent-web:package
```

With pnpm 10.12.4, add this override to your app's `package.json`.
Replace `/absolute/checkout` with the checkout's absolute path.

```json
{
  "pnpm": {
    "overrides": {
      "@pulsebeam/web": "file:/absolute/checkout/bazel-bin/agents/pulsebeam-agent-web/package"
    }
  }
}
```

Then install:

```bash
pnpm add file:/absolute/checkout/bazel-bin/agents/react/package react@18.3.1 react-dom@18.3.1
```

Import from `@pulsebeam/react`. The override supplies its internal Web dependency;
public registry availability is not established. Rebuild and reinstall together
when changing versions. Do not edit generated output.

Your bundler must serve `dist/wasm/pulsebeam_agent_web_bg.wasm` from the Web package
at its module-resolved URL. A WASM request must not return HTML.
See [asset checks](/react/troubleshooting#package-and-wasm).
:::

::: details Browser and server requirements
Capture needs HTTPS or localhost, WebRTC, WebAssembly, Web Audio, and
`navigator.mediaDevices.getUserMedia`. An HTTPS page cannot use an insecure remote endpoint.

The example uses React/React DOM 18.3.1 and pinned Chromium. The package's React
18+ peer range does not mean every React version or browser was tested.

Use the base HTTP(S) endpoint, not `/api/v1/native`. The dev media port is 3478;
remote servers need reachable media ports, TLS, and firewall settings from
[deployment](/deployment). CLI tokens expire after one hour by default.
:::

For production, fetch tokens from your authenticated backend and renew before
expiry. [Token renewal](/react/guides#connection-renewal-and-errors) explains
how to keep the same session without exposing signing keys.
