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
| Messages fail or disappear | [Data](#data-failures) |

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

Read `agent.connection`, `agent.error`, admitted room and participant IDs, and expiry directly.
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

1. **Joined?** Both participants show `connected`, with distinct participant IDs in the same room.
2. **Published?** Sender capture is active and passed to `usePublish` with the
   intended logical handle. `{ enabled: false }` disables sending; a null source
   withdraws the input without closing publication ownership.
3. **Discovering or addressing a known peer?** For a roster, inspect the owned
   `useWatch(room, config)` handle's `state()`. Loading is not an empty page;
   Failed carries an error. A Live page contains participants and publications,
   including presence-only entries. There is no global `agent.participants` or
   participant `tracks` listing. Known-address reception needs Subscribe, not
   Discover; discovery alone does not create receive ownership.
4. **Owned?** Create the matching video or audio subscription with `useSubscribe`
   and pass it to playback through `source`. Inspect its `state()` for admission
   and execution. Playback alone does not create reception. Confirm topology has capacity.
5. **Playing?** Handle `Video.onPlaybackError` and `Audio.onPlaybackError`.
   Also check sender silence and browser or OS output.

A local preview proves capture, not delivery. Match logical handles by external
participant ID, kind, and track ID.

## Roster loading or failure

| Symptom | Diagnosis and action |
| --- | --- |
| Loading roster | No complete page is available yet. Check session connectivity; do not display loading as an empty successful roster. |
| Permission failure | Check Discover for the exact watched room. Subscribe grants do not authorize metadata. Correct authorization and create a new watch. |
| Expired or invalid continuation | Choose First page, omitting the cursor. A token belongs to one room and limit and expires after 60 seconds; do not reuse it with a changed scope. |
| Eight-watch capacity error | Release unused watch owners. Closing only an update cursor does not release its watch. |
| Participant without publications | Presence-only participants are valid. Do not invent subscriptions or hide the participant because the publication list is empty. |
| Media continues after watch close | Expected for an independently held subscription. Close the receive owner separately when it is no longer needed. |

Use `page.hasMore` to enable Next page. Do not retry expired continuations in a
loop or traverse all pages to reconstruct a global roster. See the
[bounded roster example](/react/guides#remote-discovery-and-video-receive-policy).

## Autoplay

Run recovery **directly from a button click** and catch rejection:

| Media | Recovery |
| --- | --- |
| Audio | The `retry()` supplied by `Audio.onPlaybackError` |
| Video | The `retry()` supplied by `Video.onPlaybackError` |

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

## Data failures

Catch `DataPublication.send()` rejection and provide `onError` on mounted `Data`
owners. Read `agent.connection` and `agent.error` directly in React. Decode and
validate payloads inside `onEvent`; malformed application payloads should be
ignored or reported by the application without ending the SDK event stream.

| Symptom | Diagnosis and action |
| --- | --- |
| Send rejects | Check connection intent, matching server and SDK versions, track ID and delivery, payload size, and queue capacity. Retry as a new application action after recovery. |
| `onError` fires | A receive operation or application handler failed and this mount released its subscription. Correct the cause, then remount to restart. Catch expected processing errors inside `onEvent` when reception should continue. |
| An inline `gap` arrives | An ordered range was skipped. Display the notice before later messages from that track and reload backend state if completeness matters. |
| Latest values disappear | Supersession is expected per data track. Use `lossy` for distinct transient events or `ordered` when ordered recovery matters. |
| No old messages after subscribing or reconnecting | Subscriptions start live; replacement tracks have fresh IDs and sequence spaces. There is no offline replay. |
| Metadata fails to serialize as JSON | Sequence and gap fields are `bigint`. Store decimal strings; never round through `number`. |

Confirm with two participants using the same track ID and delivery.
Sending does not loop back to its publisher. Accepted sends are not delivery
acknowledgments. Queues and recovery are finite. See the [data reference](/react/reference#data).
