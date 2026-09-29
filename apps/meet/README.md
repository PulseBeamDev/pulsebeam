# PulseBeam Meet

Static Meet client using only the local `@pulsebeam/react` SDK.

From the repository root, run `just prepare` before `just check` or
`just meet-build`. Production builds use
`https://demo.pulsebeam.dev`. `just dev` defaults to the local server at
`http://localhost:7070` when `NEXT_PUBLIC_PULSEBEAM_SERVER_URL` is unset.
Serve an export with `pnpm start`.

Meet has no app-level browser tests. Its `check` recipe covers static checks,
and `build` produces the static export. Capture, device replacement, Agent
ownership, and transport behavior are tested by the React SDK and Web runtime
owners rather than coupled to the Meet UI.
