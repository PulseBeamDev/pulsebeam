# PulseBeam Meet

Static Meet client using only the local `@pulsebeam/react` SDK.

From the repository root, run `just prepare` before `just check` or
`just meet-build`. Production builds require
`NEXT_PUBLIC_PULSEBEAM_API_URL` to contain the HTTPS API URL. `just dev`
defaults to the local API at `http://localhost:7070/api/v1` when the variable
is unset. Serve an export with `pnpm start`.
