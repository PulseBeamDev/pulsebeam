# `pulsebeam-cli`

Headless load and benchmark client for a running PulseBeam server. The current
`bench` creates rooms of `pulsebeam-agent-native` peers, publishes the embedded
encoded fixtures, automatically subscribes each peer to remote video, and
records latency and transport snapshots as CSV.

This binary does not run the SFU. Its `auth-key` command creates production
authentication material without issuing production bearer tokens:

```text
pulsebeam-cli auth-key \
  --public-registry project-registry.json \
  --private-signing-bundle signing-key.json
```

Pass `--project-id p_0...` to generate a rotation key for an existing project.
Both outputs must be new files; the private output is owner-only on Unix.

Canonical entity IDs can be derived for log lookup with `id room`,
`id participant`, and `id track`. Each command requires a canonical
`--project-id` and the external identity chain for the requested entity.

```text
cargo run --release -p pulsebeam-cli -- \
  --api-url http://127.0.0.1:7070 bench \
  --rooms 5 --users-per-room 4 --max-rooms 50
```

Keep command documentation aligned with `--help`; do not document unimplemented
subcommands. Focused verification is `cargo test -p pulsebeam-cli`, followed by
the root `just check` and `just test` gates.
