# PulseBeam Meet

Static Meet client using only the local `@pulsebeam/react` SDK.

Install dependencies with `pnpm install`, then use `just dev`, `just check`,
`just test`, or `just build`. Serve an export with `pnpm start`.

For browser acceptance, start the SFU with
`cargo run --release -p pulsebeam -- --dev`, build the app, serve `out`, and
run `MEET_URL=http://127.0.0.1:3000 just test-browser`. Browser tests require
synthetic capture devices or real permission grants; display-capture picker
interaction remains manual.
