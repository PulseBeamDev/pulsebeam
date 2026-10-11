---
title: React SDK troubleshooting
---

# Troubleshooting

Start with the [checked quickstart](/react/quickstart) and the
[API reference](/react/reference). Separate capture, admission, discovery,
Mapping, and playback: success at one boundary does not prove the next.

## Permission and capture failures

Inspect `CaptureResult.state`, `error.code`, and the rejected `request()`.

| Code/symptom | Diagnostic and recovery |
| --- | --- |
| `permission-denied` | Check browser/site and OS permission, HTTPS/localhost, iframe permissions policy. Let the user change permission, then request from a gesture. |
| `device-not-found` | Enumerate devices after permission, select an available device, or remove stale exact constraints. |
| `device-busy` | Release competing application capture or check OS device access, then request explicitly. |
| `constraint-unsatisfied` | Relax device/resolution constraints; do not repeat the same impossible request. |
| `request-cancelled` | Chooser cancellation or a superseded request is visible failure, not successful capture. Request again only when intended. |
| `not-supported` | Inspect mediaDevices/getUserMedia/getDisplayMedia and secure context; use a supported browser/environment. |
| Replacement error with `active` | Old capture was retained. Keep previewing it and let the user select a replacement. |
| Camera light remains after leave | Disconnect does not stop borrowed capture. Call hook `stop()` or unmount it. |

Display-media errors are browser-normalized and cancellation can also appear
as permission denial. Never promise a specific chooser behavior across browsers.

## Admission and connection

Read `getSnapshot().connection`, `failure.class/message`, admitted IDs, and
`authorizationExpiresAt`. Enable `logging: {level: "debug"}` during diagnosis;
this replaces a hook-owned Agent, and Chrome requires Verbose console output
to show debug logs. Avoid logging bearer tokens.

- No admission: verify the absolute base endpoint, matching server/SDK version,
  token expiry, room, and distinct participant credential. Inspect the native
  HTTP request and CORS/TLS/mixed-content failures.
- Admission succeeds but transport stalls: verify advertised media address and
  UDP/TCP ports using [deployment](/deployment). An HTTP response alone is not
  media connectivity.
- Transient retry states: the runtime retries while connection is desired.
  Disconnect cancels intent. Reconnect preserves desired state but cannot fix
  bad credentials or assets.
- `terminal-failure`: identify the failure class. Fix configuration/assets,
  obtain appropriate authorization, or correct incompatible protocol versions
  before creating a fresh Agent. Blind retry is not a supported repair.
- Renewal failure: ask the backend for a token authorizing the same session.
  Changing rooms or identities requires a new Agent, not token-only renewal.

## Missing media

- Confirm both tabs are connected with different participant IDs in one room.
- Confirm local capture is active and sources are bound to the correct local
  `camera`/`microphone` handles. `setSource(null)` withdraws; disabling
  retains a black/silent publication.
- Catalog presence is discovery, not receive Mapping. Inspect
  `snapshot.catalog`, `snapshot.mapping`, and `snapshot.tracks`.
- Give remote `Video` a visible nonzero size. Hidden/off-screen elements do not
  demand video. Check remote topology capacity and console warnings for excess
  visible consumers.
- Check publication labels and application external IDs rather than matching
  against opaque snapshot participant IDs.
- Remote audio `receiving` indicates Mapping only. A silent sender, suspended
  context, OS output selection, or autoplay can still mean no audible sound.
- If capture was stopped, update/clear its bound sources. Disconnect or source
  clearing alone does not release the capture device.

## Autoplay

Remote audio needs no component. Automatic failures are logged. Add a gesture
button that calls `remote.resumeAudio()` directly and catches failure.
For video, display a button using `onPlaybackError`'s supplied `retry`.
Do not defer these recovery calls until after an unrelated network request.
`autoPlay={false}` intentionally leaves video paused but still requests media;
native controls can start it. Repeat failures require inspecting the actual
browser error, not adding global timers.

## Package and WASM

Use the local [package setup](/react/quickstart#_1-build-the-available-packages-and-example).
Package metadata does not establish registry availability.

Check Network for `pulsebeam_agent_web_bg.wasm`: it must be a real WASM
response, not HTML from an SPA fallback. Serve it at the module-resolved URL
with an appropriate WASM content type and permitted CSP. Keep generated JS,
declarations, and WASM from the same build. Check the console for initialization
or unavailable WebRTC/Web Audio APIs. Do not import browser runtime into
server-side execution or assume the bundler copies WASM automatically.

The repository runner supplies the asset explicitly. If it works but your
application does not, compare emitted module/asset URLs, base path, MIME, CSP,
and browser-only mounting before investigating media permissions.

## Topic failures

Catch publish/iteration errors and inspect `snapshot.topics` plus
`topic-send-dropped`, `topic-channel-failed`, `topic-recovery-gap`, and
`topic-resynchronized` events. Check JSON size, mode/name consistency, and
subscription cleanup. Bounded queues and transient transport replacement can
drop messages. Recover durable application state through your backend, not by
assuming every admitted send was received.
