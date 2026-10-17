# `pulsebeam-cli`

Headless load and benchmark client for a running PulseBeam server. The current
`bench` creates rooms of `pulsebeam-agent-native` peers, publishes the embedded
encoded fixtures, automatically subscribes each peer to remote video, and
records latency and transport snapshots as CSV.

## Production keys

Generate the initial project and API key with no arguments:

```text
./bazel run //:cli -- key
```

The command writes nothing to disk. It prints two values to the terminal:

- an opaque **API key** (private), for your application backend to pass explicitly
  to a PulseBeam server SDK when signing participant tokens;
- a **key registry** (public JSON), containing the one project ID and the public
  keys the PulseBeam server trusts.

The production PulseBeam server reads `./key-registry.json` from its working
directory. How you store the private API key is application policy; the SDK does
not implicitly read an environment variable.

To add a replacement key without changing the project identity, feed the
existing public key registry to the explicit rotation command:

```text
./bazel run //:cli -- key rotate < key-registry.json
```

Rotation prints the new private API key and an updated public key registry containing
both the old and new public keys. Replace the server key registry only after saving
the new private key. Retain old public keys until participant tokens signed by
them have expired.

For a local `pulsebeam --dev` server, mint a one-hour development bearer token
with fixed development credentials:

```text
pulsebeam-cli token --room general --participant alice
```

Use `--ttl SECONDS` to select a different positive lifetime. This command does
not accept production signing material.

Canonical entity IDs can be derived for log lookup with `id room`,
`id participant`, and `id track`. Room and participant derivation require
`--project-id` and the corresponding external identity chain. Track derivation
requires the actual `--connection-id`, `--kind`, and `--label`: a reconnect
mints a new connection and consequently new TrackIds for the same participant.

```text
./bazel run --config=release //:cli -- \
  --api-url http://127.0.0.1:7070 bench \
  --rooms 5 --users-per-room 4 --max-rooms 50
```

Keep command documentation aligned with `--help`; do not document unimplemented
subcommands. Focused verification is `./bazel test //crates/pulsebeam-cli:unit_tests`, followed by
the root `./bazel test //:check` and `./bazel test //:test` gates.
