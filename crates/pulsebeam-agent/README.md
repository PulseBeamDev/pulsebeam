# `pulsebeam-agent`

Native reference WebRTC peer used by the CLI and deterministic simulator. It
drives `str0m`, performs PulseBeam HTTP signaling, publishes and subscribes to
RTP tracks, and exposes media frame pipelines and connection statistics.

This is an ordinary asynchronous client, not an SFU shard. The server's
thread-per-core restrictions do not apply inside it, but its clock and network
abstractions must remain compatible with deterministic simulation.

## Main surfaces

- `agent`: session lifecycle, tracks, subscriptions, and statistics;
- `api`: the PulseBeam HTTP signaling client;
- `media` and `pipeline`: deterministic media sources, frame assembly, and
  jitter buffering;
- `actor`: native runtime integration.

Run `cargo test -p pulsebeam-agent` for focused work. Changes used by simulated
clients must also pass the root `just test` gate.
