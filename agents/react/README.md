# PulseBeam React

**See and hear the other participant in a React room.**

[Install the SDK and add a room](../../docs/react/quickstart.md) in your own app.
These examples describe the current source API. Released package/server
compatibility for this revision has not been qualified here. Obtain matching
SDK archives and a server build from a trusted build, then install the archives
in your app:

```sh
npm install /path/to/web-package.tgz /path/to/react-package.tgz
```

The React SDK runs in the browser. Issue participant tokens using
`@pulsebeam/server` on your Node.js backend (or a Rust, Go, or Python server SDK).
Keep the opaque API key on your backend. Only short-lived participant JWTs
belong in browser code.

Use the [framework-independent quickstart](../../docs/react/quickstart.md) to
start PulseBeam, issue tokens, and join or publish media in React.
`useAgent({endpoint, token})` uses fixed defaults of 2 local videos,
2 local audios, 16 remote videos, and 8 remote audios. See
[capacity limits](../../docs/react/reference.md#capacity) for overrides.

## Build your next feature

- [Screen sharing, devices, controls, and live messages](../../docs/react/guides.md)
- [How the parts fit together](../../docs/react/index.md)
- [API reference](../../docs/react/reference.md)
- [Fix a failing step](../../docs/react/troubleshooting.md)

Import from `@pulsebeam/react`. `useAgent` returns `null` until its committed
effect has created and initialized the Agent; render a mounted child component
for Agent-dependent hooks. React owns the runtime lifecycle. Use a React `key` to
replace logical sessions.
Capture hooks own the devices: disconnecting an Agent does not stop capture.
Look up local handles with `agent.video(trackId)`, `agent.audio(trackId)`, or
`agent.data(trackId, delivery)`. Look up a room-pinned remote handle with
`agent.room(room).participant(id).video(trackId)`, `.audio(trackId)`, or
`.data(trackId, delivery)`. The room and participant ID are fixed
by that reference and do not follow Agent room changes. Lookup is inert.
`usePublish` owns publication; `useSubscribe` owns reception. Pass the resulting
subscription to `Video` or `Audio` through `source`. Playback owns only its
observation clone, never the independent subscription. Configure receive hints
through `useSubscribe`, not playback props. `useCapture` accepts typed exact devices
and portable best-effort settings; `useDevices` inventories cameras and
microphones. Applications own data codecs and schema validation.

`Data.onEvent` accepts synchronous or async handlers. Each mount awaits its
handler before taking the next event, independently of other mounts. This uses
bounded receive buffering, not transport backpressure or durable storage. A
handler or receive failure releases that mount and reports through `onError`
(or a diagnostic when absent). Remount to restart. Catch processing errors inside
`onEvent` when reception should continue. Unmount releases ownership without
waiting for application promises; it cannot undo their side effects.

`useAgent` returns the actual Web SDK Agent, not a React facade. `usePublish(track,
source?, options?)` and `useSubscribe(remoteTrack, options?)` likewise return the
SDK's typed owned handles, or null until their effects create them. Read their
immutable `state()` snapshots directly in render; the hooks observe `updates()`
through one shared store bridge. Source changes replace only the pipe, and
unmount closes owned handles without stopping borrowed capture. This replaces
React descriptors and `.track(spec)` without compatibility aliases.

React creates Agents through `useAgent`, with no `createAgent` export or
advanced escape hatch. Non-React applications use caller-owned `new Agent(config)`
from `@pulsebeam/web`. Managed lifecycle functions are internal, not public SDK
APIs. Keep signing keys on your backend.

## Discover a bounded roster

Use `useWatch` on a memoized `agent.room(name)` reference. It returns the actual
owned Watch or null until committed, and observes `state()` internally. Its
immutable scope is room, limit and cursor bytes; equivalent configs do not churn
ownership. Unmount releases the watch, not independent subscriptions.

Render loading, failure, a complete page and explicit Next/First-page controls.
The default page limit is 100, maximum 500; an Agent owns at most eight watches.
Pages include presence-only participants and immutable publication metadata.
Expired continuations fail visibly and can restart from the first page. Do not
traverse every page or equate the page size with receive capacity.

Discover permission grants metadata, not reception. Use the participant's `id`
and the publication's `trackId` for pinned lookup, then let mounted consumers
own `useSubscribe`; exclude self. Known-address Subscribe needs no Discover.
`agent.participants`, `participant.tracks` and global snapshot discovery are absent.
See the [typed bounded roster and receive guide](../../docs/react/guides.md#remote-discovery-and-video-receive-policy).

## Check changes

```sh
./bazel test //agents/react:fast
./bazel test //agents/react:component_tests
./bazel test //agents/react:quickstart_build
```

The owner `:fast` gate includes SDK/package contracts and is not the root cheap
loop. Component tests execute production hooks with real React commits,
effects, events and unmounts in Happy DOM, controlling only external runtime,
capture and playback outcomes. These tests and pure session-identity decisions
also run through root `//:fast`. The quickstart build and type targets check the
documented example without a browser or production-server fixture.
Run `./bazel run //agents/react:quickstart_dev` to serve its built bundle locally.

See the [client QA boundaries](../../docs/build.md#client-qa-boundaries) for
model limits and the required Web-platform witness. Component tests do not prove
browser layout, decoded media, WebRTC compatibility or autoplay security.
