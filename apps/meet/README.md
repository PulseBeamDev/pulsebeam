# PulseBeam Meet

Static Meet client using only the local `@pulsebeam/react` SDK.

From the repository root, run `just prepare` before `just check` or
`just meet-build`. Production builds use
`https://demo.pulsebeam.dev`. `just dev` defaults to the local server at
`http://localhost:7070` when `NEXT_PUBLIC_PULSEBEAM_SERVER_URL` is unset.
Serve an export with `pnpm start`.
