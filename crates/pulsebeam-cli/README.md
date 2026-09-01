# `pulsebeam-cli`

Headless load and benchmark client for a running PulseBeam server. The current
`bench` creates rooms of `pulsebeam-agent-native` peers, publishes the embedded
encoded fixtures, automatically subscribes each peer to remote video, and
records latency and transport snapshots as CSV.

This binary is a client only; it does not run or administer the SFU.

```text
cargo run --release -p pulsebeam-cli -- \
  --api-url http://127.0.0.1:7070 bench \
  --rooms 5 --users-per-room 4 --max-rooms 50
```

Keep command documentation aligned with `--help`; do not document unimplemented
subcommands. Focused verification is `cargo test -p pulsebeam-cli`, followed by
the root `just check` and `just test` gates.
