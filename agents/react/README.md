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
[capacity limits](../../docs/react/reference.md#configuration-types) for
overrides and the current reconnect requirement.

## Build your next feature

- [Screen sharing, devices, controls, and live messages](../../docs/react/guides.md)
- [How the parts fit together](../../docs/react/index.md)
- [API reference](../../docs/react/reference.md)
- [Fix a failing step](../../docs/react/troubleshooting.md)

Import from `@pulsebeam/react`. `useAgent` returns an Agent immediately; React
owns its runtime lifecycle. Use a React `key` to replace logical sessions.
Capture hooks own the devices: disconnecting an Agent does not stop capture.
Keep signing keys on your backend.

## Check changes

```sh
./bazel test //agents/react:fast
```

The fast gate checks SDK contracts and the repository's separate Vite test
fixture. `//agents/pulsebeam-agent-web:browser` exercises that fixture with
two participants; these checks don't validate the tutorial's other frameworks.
