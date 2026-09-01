# `pulsebeam-agent` (deprecated compatibility facade)

This crate preserves the former API for the CLI and simulator while migration
finishes. Its implementation now lives in `agents/pulsebeam-agent-native`; do
not add new runtime behavior here.

This is an ordinary asynchronous client, not an SFU shard. The server's
thread-per-core restrictions do not apply inside it, but its clock and network
abstractions must remain compatible with deterministic simulation.

## Migration

- New integrations use `pulsebeam-agent-native::Agent` and complete desired-state updates.
- Existing imports continue through the re-exported `agent`, `api`, `media`, `pipeline`, and `actor` modules.
- Compatibility modules are removed as their remaining consumers migrate.

Run `cargo test -p pulsebeam-agent` for focused work. Changes used by simulated
clients must also pass the root `just test` gate.
