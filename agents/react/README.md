# PulseBeam React

**See and hear the other participant in a React room.**

[Install the SDK and add a room](../../docs/react/quickstart.md) in your own app.
Install the React SDK in your app:

```sh
npm install @pulsebeam/react
```

Use `@pulsebeam/server` on a Node.js backend, or the corresponding Rust,
Go, or Python server SDK, to issue participant tokens.

Use the [framework-independent quickstart](../../docs/react/quickstart.md) to
issue participant tokens on your backend, then join and publish media in React.

## Build your next feature

- [Screen sharing, devices, controls, and live messages](../../docs/react/guides.md)
- [How the parts fit together](../../docs/react/index.md)
- [API reference](../../docs/react/reference.md)
- [Fix a failing step](../../docs/react/troubleshooting.md)

Import from `@pulsebeam/react`. Use `useAgent` for React-owned session cleanup.
Capture hooks own the devices: disconnecting an Agent does not stop capture.
Keep signing keys on your backend.

## Check changes

```sh
./bazel test //agents/react:fast
```

The fast gate checks SDK contracts and the repository's separate Vite test
fixture. `//agents/pulsebeam-agent-web:browser` exercises that fixture with
two participants; these checks don't validate the tutorial's other frameworks.
