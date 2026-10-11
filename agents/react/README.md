# PulseBeam React

**See and hear the other participant in a React room.**

[Run the two-tab demo](../../docs/react/quickstart.md), then adapt its source.
It uses local SDK packages from this checkout; registry availability is not assumed.

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
