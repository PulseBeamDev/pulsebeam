# PulseBeam React

**See and hear the other participant in a React room.**

[Install the SDK and add a room](../../docs/react/quickstart.md) in your own app.
The quickstart uses the published npm package, a server endpoint, and two participant
tokens. A source-based local sandbox is available if you do not have a server yet.

```sh
npm install @pulsebeam/react@0.2.0
```

The compatible `@pulsebeam/web` runtime and its WASM asset are installed as a
dependency. Follow the quickstart's bundler setup so the asset is served correctly.

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

The fast gate checks the quickstart's actual sources. The browser owner
`//agents/pulsebeam-agent-web:browser` exercises the same app with two participants.
