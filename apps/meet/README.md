# PulseBeam Meet

Static Meet client using only the local `@pulsebeam/react` SDK.

From the repository root, run `just prepare` before `just check` or
`just meet-build`. Production builds use
`https://demo.pulsebeam.dev/api/v1`. `just dev` defaults to the local API at
`http://localhost:7070/api/v1` when `NEXT_PUBLIC_PULSEBEAM_API_URL` is unset.
Serve an export with `pnpm start`.
