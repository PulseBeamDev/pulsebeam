---
title: React SDK troubleshooting
description: Find the failing step, check its evidence, and choose a safe recovery.
---

# Troubleshooting

Find the first step that fails. Capture, connection, discovery, receiver
assignment, and playback are separate: success at one does not prove the next.

| What fails? | Start here |
| --- | --- |
| Camera, microphone, or screen access | [Permission and capture](#permission-and-capture-failures) |
| Joining or staying connected | [Admission and connection](#admission-and-connection) |
| Connected, but media is missing or silent | [Missing media](#missing-media) |
| Browser blocks playback | [Autoplay](#autoplay) |
| SDK fails before joining | [Package and WASM](#package-and-wasm) |
| Messages fail or disappear | [Topic failures](#topic-failures) |

If you have not seen a working room yet, try the
[checked quickstart](/react/quickstart) first.

## Permission and capture failures

Read `capture.state`, `capture.error?.code`, and the rejection from
`capture.request()`. Do not retry the same request without fixing its cause.

| Code or symptom | Next action |
| --- | --- |
| `permission-denied` | Check browser/site and OS permission, HTTPS/localhost, and iframe permissions policy. Request again after the user changes permission. |
| `device-not-found` | Enumerate after permission. Choose an available device or remove a stale exact constraint. |
| `device-busy` | Release competing capture or check OS device access, then request again. |
| `constraint-unsatisfied` | Relax the device or resolution constraint. |
| `request-cancelled` | The chooser was cancelled or the request was superseded. Request again only when intended. |
| `not-supported` | Check `mediaDevices`, the required capture API, and secure context. Use a supported environment. |
| Replacement error with `active` | Keep the old capture. Let the user choose another replacement. |
| Camera light remains after leaving | Call the hook's `stop()` or unmount it. Disconnect does not stop borrowed capture. |

Browsers normalize display-capture failures differently. Chooser cancellation
may appear as permission denial; do not depend on one code in every browser.

## Admission and connection

**Admission** is the server accepting the participant into a room.
Calling `connect()` only expresses intent.

1. Read `getSnapshot().connection`, `failure.class`, and `failure.message`.
2. Check the admitted external IDs and `authorizationExpiresAt`.
3. If needed, enable `logging: {level: "debug"}`. Changing logging replaces
   a hook-owned Agent. Enable Verbose in Chrome's console to see debug logs.

Never log bearer tokens.

| Symptom | Check and recovery |
| --- | --- |
| No admission | Base HTTP(S) endpoint, matching server/SDK version, token expiry, room, and distinct participant credential |
| Native HTTP request fails | TLS, mixed content, and cross-origin access (CORS) settings |
| Admitted, but media connection stalls | Advertised media address and UDP/TCP ports in [deployment](/deployment) |
| Transient retry state | Runtime retries while connection is desired; disconnect cancels that intent |
| `terminal-failure` | Fix the reported configuration, assets, authorization, or protocol issue before creating a fresh Agent |
| Renewal fails | Get a backend token for the same session; use a new Agent to change room or participant |

An HTTP response alone does not prove media connectivity.
Reconnect preserves desired state, but cannot repair bad credentials or missing
assets. Do not loop on terminal errors.

## Missing media

Check in order. Stop at the first missing piece of evidence.

1. **Both participants joined.** Each tab shows `connection === "connected"`
   and a distinct admitted participant identity in the same room.
2. **The sender captured and published.** Capture is active and the sources
   are bound to the correct `camera` and `microphone` handles.
   `setSource(null)` removes a publication; disabling keeps it black/silent.
3. **The receiver discovered the publication.** Read `snapshot.catalog`,
   the list of discovered participants and publications.
4. **The SDK assigned a receiver.** Read `snapshot.mapping` and
   `snapshot.tracks`. This assignment is called **Mapping**.
   Discovery alone does not prove it happened.
5. **Video is visible and playing.** Give remote `Video` a nonzero size.
   Hidden/off-screen elements do not request receivers. Check topology
   capacity warnings and [autoplay](#autoplay).
6. **Audio is audible.** `receiving` only reports Mapping. Check sender
   silence, the browser audio context, autoplay, and OS output selection.

Match remote handles by external participant ID and label, not the opaque
internal IDs in raw snapshot publications.

If capture was stopped, clear or update its bound sources. Clearing a source
or disconnecting an Agent does not release the capture device.

## Autoplay

Use a button gesture to recover blocked playback.

| Media | Recovery |
| --- | --- |
| Audio | Call `agent.remote.resumeAudio()` directly in the click handler and catch rejection |
| Video | Store `onPlaybackError`'s supplied `retry`; call it from a button and catch rejection |

Audio needs no component; automatic failures are logged.
Do not defer recovery until after an unrelated network request.
With `autoPlay={false}`, video stays attached and requests media, but needs
native controls or a gesture to start playback.

If recovery still fails, inspect the browser error rather than adding timers.

## Package and WASM

Use the local [package setup](/react/quickstart#use-the-sdk-in-your-app).
Package metadata does not prove registry availability.

**WASM** is WebAssembly, the SDK's compiled browser runtime. Check these
before debugging camera permission:

1. In Network, find `pulsebeam_agent_web_bg.wasm`. Its response must contain
   WASM, not a 404 or the app's HTML fallback.
2. Check the module-resolved URL, base path, and WASM content type.
3. Check Content Security Policy (CSP) and console initialization errors.
4. Keep generated JavaScript, declarations, and WASM from the same build.
5. Confirm WebRTC and Web Audio are available and the SDK mounts only in the browser.

Do not assume your bundler copies the asset. The quickstart runner does;
compare its emitted asset layout with your app if only your app fails.
Never execute browser media initialization during server-side rendering.

## Topic failures

1. Catch publish and iteration errors.
2. Check JSON size, topic name and mode, and subscription cleanup.
3. Read `snapshot.topics` and the events `topic-send-dropped`,
   `topic-channel-failed`, `topic-recovery-gap`, and `topic-resynchronized`.
4. Reload authoritative app state from your backend when gaps matter.

Queues are bounded. Connection recovery may drop messages, and accepted sends
do not prove delivery. See [topic contracts](/react/reference#topics) rather
than treating topics as durable storage.
