---
title: React SDK troubleshooting
description: Find the first failing step and fix it.
---

# Get it working

Pick the symptom. Fix the first failing step, then try again.

| Symptom | Start here |
| --- | --- |
| No camera, microphone, or screen | [Capture](#permission-and-capture-failures) |
| Can't join or stay connected | [Connection](#admission-and-connection) |
| Connected, but no video or sound | [Media](#missing-media) |
| Playback blocked | [Autoplay](#autoplay) |
| SDK won't initialize | [Packages/WASM](#package-and-wasm) |
| Messages fail or disappear | [Topics](#topic-failures) |

## Permission and capture failures

Read `capture.error?.code` and catch `capture.request()` rejection.

| Code or symptom | Try this |
| --- | --- |
| `permission-denied` | Allow browser/site and OS access; use HTTPS/localhost. Check iframe permissions. |
| `device-not-found` | Choose an available device or remove a stale exact constraint. |
| `device-busy` | Release competing capture and check OS access. |
| `constraint-unsatisfied` | Relax device/resolution constraints. |
| `request-cancelled` | Request again only when intended. |
| `not-supported` | Check secure context and the required browser capture API. |
| Error but `state === "active"` | Replacement failed; keep old capture and choose again. |
| Device stays active after leaving | Call `capture.stop()` or unmount its hook. |

Fix the cause before requesting again. Screen chooser cancellation may appear
as permission denial; codes vary by browser.

## Admission and connection

Read `getSnapshot().connection`, `failure`, admitted external IDs, and expiry.
Calling `connect()` is intent, not proof of admission.

| Symptom | Check |
| --- | --- |
| Can't join | Base endpoint, matching server/SDK version, unexpired token, same room, distinct participant credentials |
| HTTP request fails | TLS, mixed content, and CORS |
| Joined, but transport stalls | Advertised media address and reachable ports in [deployment](/deployment) |
| Temporary retry state | Runtime retries while connection is desired; disconnect cancels it |
| `terminal-failure` | Fix the reported assets/configuration/authorization/protocol cause before creating a fresh Agent |
| Renewal fails | Backend token must keep the same room and participant |

For more detail, enable `logging: {level: "debug"}` and Chrome's Verbose console
level. Changing logging replaces a hook-owned Agent. **Never log bearer tokens.**
Reconnect cannot repair bad credentials or missing assets.

## Missing media

Check in order:

1. **Joined?** Both participants show `connected`, with distinct external IDs in the same room.
2. **Published?** Sender capture is active and bound to the right local handles.
   Disabled means black/silent; `setSource(null)` means withdrawn.
3. **Discovered?** `snapshot.catalog` lists the publication.
4. **Assigned?** `snapshot.mapping` and `tracks` show a receiver assignment.
   Discovery alone does not prove this.
5. **Playing?** Remote `Video` is visible and sized; topology has capacity.
   For audio, check autoplay, sender silence, and browser/OS output.

A local preview proves capture, not delivery. Audio `receiving` proves assignment,
not sound. Match logical handles by external participant ID and label.

## Autoplay

Run recovery **directly from a button click** and catch rejection:

| Media | Recovery |
| --- | --- |
| Audio | `agent.remote.resumeAudio()` |
| Video | The `retry` supplied to `onPlaybackError` |

Do not defer the gesture behind a network request or replace it with a timer.
With `autoPlay={false}`, video still requests media; provide native controls.
If recovery fails, inspect the browser error.

## Package and WASM

Start with the [Next.js setup](/react/quickstart#1-create-an-app) and the
[client component](/react/quickstart#4-connect-and-publish).
The published `@pulsebeam/react@0.2.0` package depends on `@pulsebeam/web@^0.2.0`;
the Web package includes the compiled WebAssembly (WASM) runtime.

1. In Network, find the request for `pulsebeam_agent_web_bg.wasm`. It must
   return the WASM file with `Content-Type: application/wasm`, not an HTML
   fallback or a 404.
2. In Next.js, keep `useAgent` and `Video` inside a `"use client"` component;
   keep `signParticipantToken` on the server. Do not initialize the browser SDK
   during server-side rendering.
3. Confirm the bundler emits or serves the WASM at the module-resolved URL.
   Check the deployed base path and Content Security Policy.
4. Keep JavaScript, declarations, and WASM from the same package build; do not
   substitute a WASM file from another release or local checkout.
5. Check WebRTC and Web Audio support. Test both development and production
   builds, not only the TypeScript compiler.

Try **Open room** and **Join** again. Successful WASM loading gets you past
initialization; it does not by itself confirm server admission or media delivery.

## Topic failures

Catch publish and iteration errors. Check JSON size, name/mode, and cleanup.
Use `snapshot.topics` and events such as `topic-send-dropped`,
`topic-channel-failed`, `topic-recovery-gap`, and `topic-resynchronized`.

Accepted sends are not delivery acknowledgments. Queues are bounded and recovery
can lose messages; reload backend state when needed. [Exact limits](/react/reference#topics).
