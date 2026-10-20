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
| SDK won't initialize | [SDK initialization](#sdk-initialization) |
| Messages fail or disappear | [Topics](#topic-failures) |

## Permission and capture failures

Read `capture.error?.code` and catch `capture.request()` rejection.

| Code or symptom | Try this |
| --- | --- |
| `permission-denied` | Allow browser/site and OS access; use HTTPS/localhost. Check iframe permissions. |
| `device-not-found` | Choose an available typed device or omit device selection for the default. |
| `device-busy` | Release competing capture and check OS access. |
| `constraint-unsatisfied` | Check positive finite resolution/frame-rate and boolean processing preferences. |
| `request-cancelled` | Request again only when intended. |
| `not-supported` | Check secure context and the required browser capture API. |
| Error but `state === "active"` | Replacement failed; keep old capture and choose again. |
| Device stays active after leaving | Call `capture.stop()` or unmount its hook. |

Fix the cause before requesting again. Screen chooser cancellation may appear
as permission denial; codes vary by browser.

## Admission and connection

Read `agent.connection`, `agent.error`, admitted external IDs, and expiry directly.
`useAgent` keeps those properties reactive; calling `connect()` is intent, not proof of admission.

| Symptom | Check |
| --- | --- |
| Can't join | Base endpoint, matching server/SDK version, unexpired token, same room, distinct participant credentials |
| HTTP request fails | TLS, mixed content, and CORS |
| Joined, but transport stalls | Advertised media address and reachable ports in [deployment](/deployment) |
| Temporary retry state | Runtime retries while connection is desired; disconnect cancels it |
| `terminal-failure` | Fix the reported assets/configuration/authorization/protocol cause before creating a fresh Agent |
| Renewal fails | Backend token must keep the same room and participant |

For more detail, enable `logging: {level: "debug"}` and Chrome's Verbose console
level. Changing logging updates a hook-owned Agent in place. **Never log bearer tokens.**
Reconnect cannot repair bad credentials or missing assets.

## Container networking

The published server image is `linux/amd64`, but the host may be Linux,
macOS, or Windows. On an ARM host, enable amd64 emulation in your container
runtime. On Docker Desktop (macOS/Windows), `--network=host` requires
[host networking to be enabled](https://docs.docker.com/engine/network/drivers/host/)
(Docker Desktop 4.34+). With Podman Machine, `--network=host` refers to the
Linux VM's network, not your macOS/Windows host.

If `http://localhost:7070` works but connecting or receiving media fails,
check UDP 3478 **and the RTC IP address the server advertises at startup**.
PulseBeam currently selects that address from its network interfaces; an
address inside a container/VM may not be reachable from your browser.
Forwarding ports alone does not fix an unreachable advertised ICE address.
Native Linux host networking avoids this extra VM boundary.

## Missing media

Check in order:

1. **Joined?** Both participants show `connected`, with distinct external IDs in the same room.
2. **Published?** Sender capture is active and bound to the right local handles.
   `enabled={false}` means black/silent; `source={null}` means withdrawn.
3. **Discovered?** `agent.remote.videoTracks` / `audioTracks` expose the remote publications.
4. **Assigned?** For audio, check the track's `receiving`; for video, a visible
   `<Video>` drives receiver demand. Discovery alone does not prove assignment.
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

## SDK initialization

If the SDK doesn't initialize, verify that React SDK code runs in the browser
(not during server rendering) and that you're using compatible SDK versions.
With Next.js App Router, mark browser components `"use client"`.

If the browser reports a missing `pulsebeam_agent_web_bg.wasm` file or a
WebAssembly loading error, check the failed request in Network. This is an SDK
packaging/bundler integration issue, **not an application file to copy manually**.
Report it with the SDK version, your framework/bundler version, and the failing request.

See [the quickstart](/react/quickstart) to retry a minimal integration.

## Topic failures

Catch `Chat.publish()` rejection and provide `onError` on committed consumers.
Read `agent.connection` and `agent.error` directly in React; there is no manual
snapshot/subscription loop. Handle continuity in `onEvent`, not as an error.

| Symptom | Diagnosis and action |
| --- | --- |
| Publish rejects | Check connected intent, matching server/SDK versions, explicit mode/name, JSON size, and queue capacity. Initial binding waits at most 10 seconds; a replaced generation/publication rejects rather than retargeting the send. Retry a new action after recovery. |
| `onError` reports `decode-failed` | Invalid UTF-8/JSON released that consumer. Validate publisher payloads, then remount the consumer to retry. Other consumers remain independent. |
| `onError` reports `channel-failed` | Inspect the structured direction/mode failure and Agent connection/error. Core owns transport recovery; fix terminal configuration/authorization failures before reconnecting. |
| An inline `gap` arrives | A known ordered range was skipped. Display the notice before later messages from that track and reload backend state if completeness matters. |
| Latest values disappear | Supersession is expected per data track. Use `lossy` for distinct transient events or `ordered` for ordered events. |
| No old messages after subscribing or reconnecting | Subscriptions start live; reopened publications have fresh data-track IDs and sequence spaces. There is no offline or cross-incarnation replay. |
| Metadata fails to serialize as JSON | Sequence/gap fields are `bigint`. Store decimal strings; never round through `number`. |

Confirm with two participants using the same topic name **and mode**. Sending
does not loop back to its publisher. Accepted sends are not delivery
acknowledgments; queues and recovery are finite. [Exact limits](/react/reference#topics).
