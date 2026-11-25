# `pulsebeam-cli`

Command-line tools for operating and benchmarking PulseBeam. The `bench`
command creates rooms of `pulsebeam-agent-native` peers, publishes the embedded
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

## Move a native publisher between provisioned nodes

Enable `--operator-socket` on the source and destination servers. Run the CLI
locally as the server user, with `--data-dir` pointing to that server's data
directory. The CLI reads one JSON request from stdin and returns one JSON
response. Supply a current token for the publisher and room in the top-level
`token` field. Keep tokens out of command arguments and shell history. For
example, pass a restricted request file with `pulsebeam-cli ownership --data-dir
"$DATA_DIR" < "$REQUEST_FILE"`.

1. On the destination, send `action: {"operation":"prepare_handoff","room":"general"}`.
   Omit the top-level `connection` field. Save the entire `successor` object
   from the `handoff_prepared` response. Preparation does not grant authority.
2. On the source, set the top-level `connection` to the live predecessor's
   `conn_*` ID. Send `action` with `operation: "handoff"`, the saved
   `successor` object, and `sequence: 1` for the first move in this room and
   participant scope. Increment the sequence for each subsequent move.
3. Wait for `handoff_completed`, which means the predecessor stopped publishing
   and all pinned peers committed the new fence. If the response is lost or
   `outcome_unknown`, ask the source for `operation: "handoff_status"` with the
   same predecessor connection, successor and sequence, plus the room in the
   action. `handoff_pending` means delivery is still outstanding. Do not start
   a new move or substitute a new successor to resolve a pending move.
4. Start one fresh native admission for the same participant and room on the
   destination. Verify the admitted connection matches `successor.connection`.
   Completion of the handoff does not mean the publisher has connected or that
   media is flowing.

Prepare only when ready to move. Each destination generation holds up to 128
unconsumed reservations, including abandoned ones. A reservation does not
expire with time, so delayed delivery can still finish in that generation.
Keep the destination generation running until the publisher is admitted. A
new generation cannot use the old reservation or bypass an existing fence.

## Benchmark

```text
./bazel run --config=release //:cli -- \
  --api-url http://127.0.0.1:7070 bench \
  --rooms 5 --users-per-room 4 --max-rooms 50
```

Keep command documentation aligned with `--help`; do not document unimplemented
subcommands. Focused verification is `./bazel test //crates/pulsebeam-cli:unit_tests`, followed by
the root `./bazel test //:check` and `./bazel test //:test` gates.
