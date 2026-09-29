# PulseBeam Meet

Static Meet client using only the local `@pulsebeam/react` SDK.

From the repository root, run `just prepare` before `just check` or
`just meet-build`. Production builds use
`https://demo.pulsebeam.dev`. `just dev` defaults to the local server at
`http://localhost:7070` when `NEXT_PUBLIC_PULSEBEAM_SERVER_URL` is unset.
Serve an export with `pnpm start`.

`just --justfile apps/meet/Justfile test-slow` from the repository root builds
and exercises the real static app in Chrome with fake camera/microphone input
and an owned local development server. Its focused regression checks that
leaving a connected call closes the transport, releases old capture, and
returns to a working lobby without a teardown exception. Ports 7070 and 3478 must
be available. Set `PULSEBEAM_BROWSER_BINARY` to use an existing Chrome binary.
