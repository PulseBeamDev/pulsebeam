# PulseBeam React

**See and hear the other participant in a React room.**

[Install the SDK and add a room](../../docs/react/quickstart.md) in your own app.
Start with Next.js and install the React and server SDKs:

```sh
npm install @pulsebeam/react @pulsebeam/server
```

Use the [quickstart](../../docs/react/quickstart.md) to issue participant tokens
on the server, then join and publish media in React.

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

The fast gate checks SDK contracts and the existing Vite-based repository test
fixture. `//agents/pulsebeam-agent-web:browser` exercises that fixture with two
participants; neither test builds the Next.js docs walkthrough.
