# PulseBeam React

**See and hear the other participant in a React room.**

[Install the SDK and add a room](../../docs/react/quickstart.md) in your own app.
Install the React SDK in your app:

```sh
npm install @pulsebeam/react
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

Import from `@pulsebeam/react`. `useAgent` returns an Agent immediately; React
owns its runtime lifecycle. Use a React `key` to replace logical sessions.
Capture hooks own the devices: disconnecting an Agent does not stop capture.
Look up local handles with `agent.video(label)`, `agent.audio(label)`, or
`agent.data(label, delivery)`. Look up a remote handle with
`agent.remote.participant(id).video(label)`, `.audio(label)`, or
`.data(label, delivery)`. Lookup is inert. `Publish`, `Data`, `Video`, and `Audio`
own their resources only while committed. Use `source`, `enabled`, and `Video.receive`
rather than synchronization effects. `useCapture` accepts typed exact devices
and portable best-effort settings; `useDevices` inventories cameras and
microphones. Applications own data codecs and schema validation.

`Data.onEvent` accepts synchronous or async handlers. Each mount awaits its
handler before taking the next event, independently of other mounts. This uses
bounded receive buffering, not transport backpressure or durable storage. A
handler or receive failure releases that mount and reports through `onError`
(or a diagnostic when absent). Remount to restart. Catch processing errors inside
`onEvent` when reception should continue. Unmount releases ownership without
waiting for application promises; it cannot undo their side effects.

This API replaces React descriptors and `.track(spec)` without compatibility
aliases. Local media handles expose only readonly `kind` and `label`; use
`Publish` props instead of imperative media setters. The Web SDK descriptor API
is unchanged.

React exposes only `useAgent` for Agent ownership, with no `createAgent` export
or advanced escape hatch. Non-React applications use caller-owned `createAgent`
from `@pulsebeam/web`. Managed lifecycle functions are internal, not public SDK
APIs. Keep signing keys on your backend.

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
