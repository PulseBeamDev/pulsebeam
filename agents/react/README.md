# PulseBeam React

`@pulsebeam/react` lets a browser React app capture, publish, and receive media.
Start with the [complete quickstart](../../docs/react/quickstart.md):
run two participants, see remote video, hear audio, and clean up on leave.

## Run the checked example

From a repository checkout with the [build prerequisites](../../docs/build.md):

```sh
./bazel run //:server -- --dev
```

Keep that server running. In another terminal:

```sh
./bazel run //agents/react:quickstart_dev
```

The [quickstart](../../docs/react/quickstart.md) explains distinct participant
tokens, browser requirements, local package installation, and WASM assets.
The runner builds local packages; metadata does not imply npm availability.
Signing keys belong on your backend, not in browser code.

## Choose your next task

| Goal | Documentation |
| --- | --- |
| Understand sessions, capture, and publications | [Concepts](../../docs/react/index.md) |
| Add device controls, screen sharing, or topics | [Integration guides](../../docs/react/guides.md) |
| Look up exports, signatures, defaults, and limits | [API reference](../../docs/react/reference.md) |
| Diagnose permission, connection, or playback failures | [Troubleshooting](../../docs/react/troubleshooting.md) |

## Ownership at a glance

| Resource | Owner and cleanup |
| --- | --- |
| Agent from `useAgent` | React hook; closes on unmount |
| Agent from `createAgent` | Caller; unsubscribe and call `close()` |
| Camera/microphone or display capture | Capture hook; `stop()` or unmount releases devices |
| Remote video presentation | `Video`; unmount releases receive demand |
| Topic subscription | Consumer; abort or end iteration |

Agents start disconnected. Call `connect()` and observe the snapshot; intent
alone does not prove admission. Disconnecting an Agent does not stop borrowed
capture. Remote audio plays automatically, subject to browser autoplay rules.

## Check changes

```sh
./bazel test //agents/react:fast
```

The fast gate includes type-checking the actual quickstart sources.
`//agents/pulsebeam-agent-web:browser` exercises the same application with
two participants and real browser media.
