---
title: React quickstart
description: Install the SDK and add a two-participant video room to a React application.
---

# Add your first React room

**See another participant's video and hear their microphone in your own app.**

This example uses the published React SDK. You can adapt the `Room` component
in an existing browser-rendered React app, or follow the small Vite setup below
in a new directory. No PulseBeam repository checkout is needed for the client.

## Before you start

- **Server and credentials:** a compatible PulseBeam endpoint and two unexpired
  participant tokens for the **same room**, with **different participant IDs**.
  Your application's token backend supplies these. If you do not have one yet,
  use the [local development sandbox](#local-development-sandbox).
- **Client tooling:** Node.js 22 and npm for this example; React 18 or newer.
  The code below uses React/React DOM 18.3.1 and Vite 5.4.21.
- **Browser:** camera, microphone, headphones, and browser support for WebRTC,
  WebAssembly, Web Audio, and `navigator.mediaDevices.getUserMedia`.
  Capture requires **HTTPS or localhost**. An HTTPS page cannot connect to an
  insecure remote HTTP endpoint.

Use the server's HTTP(S) **base URL**, not `/api/v1/native`. Keep bearer tokens
private. Signing secrets and token generation never belong in the browser.
The React peer range is not a guarantee that every React version or browser
has been tested.

## 1. Install the SDK

In an existing React app:

```bash
npm install @pulsebeam/react@0.2.0
```

The package installs its compatible `@pulsebeam/web` runtime dependency, including
WebAssembly (WASM), the compiled browser runtime. Import application APIs from
`@pulsebeam/react`.

For a new, minimal app, run this **instead** in an empty working directory:

```bash
mkdir pulsebeam-room
cd pulsebeam-room
npm init -y
npm install @pulsebeam/react@0.2.0 react@18.3.1 react-dom@18.3.1
npm install --save-dev vite@5.4.21
mkdir src
```

Keep the following client commands in this app directory, not in a PulseBeam checkout.

## 2. Set up the browser entry

For the minimal app, create `index.html`:

```html
<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>PulseBeam room</title>
  </head>
  <body>
    <div id="root"></div>
    <script type="module" src="/src/main.tsx"></script>
  </body>
</html>
```

Create `vite.config.mjs`:

```js
export default {
  esbuild: { jsx: "automatic" },
  optimizeDeps: { exclude: ["@pulsebeam/react", "@pulsebeam/web"] },
};
```

The exclusions keep Vite's development dependency optimizer from relocating
the SDK loader without its WASM asset. Your bundler must preserve or emit
`pulsebeam_agent_web_bg.wasm` at the loader's module-resolved URL. For another
bundler, follow the [asset checks](/react/troubleshooting#package-and-wasm).
Do not initialize the SDK during server-side rendering; mount this room in the
browser. This is a plain React example, not a requirement to use Vite in your app.

## 3. Add the room

Create `src/Room.tsx`. The component connects only after **Join** and requests
capture only after **Enable camera and microphone**. It binds captured sources
to publications, shows a local preview, renders remote video, and offers audio
and video playback recovery.

<<< ./Room.tsx

Create `src/main.tsx`. This browser entry asks for the endpoint and participant
token, then mounts the room. The topology reserves one outgoing camera and
microphone, and capacity for two incoming videos and audio tracks.

<<< ./main.tsx

In an existing app, mount `Room` with the same `AgentConfig` fields instead of
replacing your entry point. Fetch tokens from your authenticated backend before
mounting; the form is a manual credential input for this tutorial.

## 4. Connect two participants

For the minimal app:

```bash
npx vite --host 127.0.0.1
```

Keep the process running. Open the printed localhost URL in **two tabs**.
In each tab, enter your server base URL and that tab's token, then click
**Open room**. Replace the form's localhost default if using a remote server.

| Tab A | Tab B |
| --- | --- |
| Token for participant `alice` | Token for participant `bob` |
| Same server and room | Same server and room |

In **both tabs**:

1. Click **Join**. Wait for `Connection: connected` and the admitted participant ID.
2. Click **Enable camera and microphone** and allow browser access. Your preview appears.
3. Wave in one tab. The other tab should show the moving remote video.
4. Speak and listen from the other participant. Wear headphones to avoid feedback.

**Success:** both participants are connected, each sees the other's moving video,
and remote microphone audio plays. A local preview or publication count alone
is not proof of delivery.

If audio is blocked, click **Enable remote audio**. If video playback is blocked,
click **Retry video playback**. On one device, both tabs may share the same camera
and microphone; use two devices if you need to distinguish the participants or
if the browser cannot share capture. For remote devices, serve the app over HTTPS.

## 5. Leave and release devices

Click **Leave and stop capture**. The other tab should lose your publications.
Click **Unmount room** in both tabs. Camera and microphone should no longer be active.

`useAgent` closes its Agent on unmount; capture hooks stop their owned tracks on
unmount. Disconnecting an Agent alone does not stop capture.

## If a step fails

- [SDK or WASM will not load](/react/troubleshooting#package-and-wasm)
- [Camera or microphone is blocked](/react/troubleshooting#permission-and-capture-failures)
- [Cannot join](/react/troubleshooting#admission-and-connection)
- [Connected, but no video or sound](/react/troubleshooting#missing-media)

## Move beyond the tutorial

Before production, replace manual token entry with an authenticated backend
handoff and [renew tokens before expiry](/react/guides#connection-renewal-and-errors).
Use new Agents for a different room or participant. Development credentials
must not be used for a public service.

Next, [add screen sharing, device controls, or live messages](/react/guides),
learn [how the room works](/react/), or [look up an API](/react/reference).

## Local development sandbox

This is an **optional source-based setup** for readers who do not yet have a
compatible server and token backend. It is not required to install the browser SDK.
A standalone token-helper download is not provided by the server release.

You need a Linux x86_64 host and a [build-ready PulseBeam checkout](/build).
Run these commands from its repository root. Start the server in one terminal:

```bash
./bazel run //:server -- --dev
```

Keep it running. In another terminal, mint two one-hour development tokens:

```bash
./bazel run //:cli -- token --room react-demo --participant alice
./bazel run //:cli -- token --room react-demo --participant bob
```

Return to [step 4](#4-connect-two-participants) in your **client app directory**.
Use `http://localhost:7070` and those tokens in the form. Keep both server and
client running; do not create a second server or switch rooms during the tutorial.
The dev server uses media port 3478. Expose neither its development credentials
nor this sandbox as a production service.

::: details Run the repository example instead
To investigate or contribute to the SDK without creating a client app, run this
from the same build-ready checkout:

```bash
./bazel run //agents/react:quickstart_dev
```

This serves the same `Room.tsx` and `main.tsx` with locally built SDK packages,
not the npm installation. Open its printed URL in two tabs and use the **same
endpoint and development tokens** above. The runner replaces the client process
in step 4; it does not replace your server or token setup.
:::
