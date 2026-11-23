# `pulsebeam-cli`

Headless load and benchmark client for a running PulseBeam server. The current
`bench` creates rooms of `pulsebeam-agent-native` peers, publishes the embedded
encoded fixtures, and records latency and transport snapshots as CSV. Each peer
selects up to seven remote camera tracks from the first live catalog page,
requested with a limit of 500 participants. It does not scan subsequent pages.
The benchmark retains its selected subscriptions when tracks disappear from the
catalog and acquires a fresh physical observation when a source returns.

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

## Reconstruct an incident

Have your application backend retain the returned `jti` with the project and
app session [when issuing or renewing credentials](../../docs/react/quickstart.md).
For a client report, ask for the confirmed `conn_*` support key. You can also
start with a confirmed `pub_*` publication ID. A reconnect or publisher
replacement can change those keys, so capture them at the time of the incident.
Do not collect JWTs, receipts, or application names for this lookup.

Use an authorized NDJSON export of the server's structured logs (`--json-logs`).
Include enough lookback to capture admission or earlier binding changes, and
keep some time after the incident to catch accepted credential renewals. Give
the CLI the incident window separately from the export's time range:

```sh
./bazel run //:cli -- incident --conn "$CONNECTION_ID" \
  --from 2026-04-15T10:25:00Z --to 2026-04-15T10:30:00Z < incident.ndjson
```

Replace `--conn` with `--publication "$PUBLICATION_ID"` or
`--project "$PROJECT_ID" --jti "$TOKEN_ID"`. Optionally narrow media evidence
with `--kind audio`, `--kind video`, or `--kind data`. A token can match more
than one connection. Review all reported candidates rather than choosing the
first one.

The report distinguishes observed, partial, and missing evidence. An interval
without end evidence has no observed cutoff in the incident window. A binding
or installed route is not proof of delivery. If lookback is missing, widen the
export within the authorized retention period. Expired evidence cannot be
reconstructed from names. Keep exports and reports access-restricted and
short-lived: these runtime IDs are pseudonymous, not anonymous.

## Benchmark

```text
./bazel run --config=release //:cli -- \
  --api-url http://127.0.0.1:7070 bench \
  --rooms 5 --users-per-room 4 --max-rooms 50
```

Keep command documentation aligned with `--help`; do not document unimplemented
subcommands. Focused verification is `./bazel test //crates/pulsebeam-cli:unit_tests`, followed by
the root `./bazel test //:check` and `./bazel test //:test` gates.
