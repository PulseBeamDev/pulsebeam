# Additive agent architecture

The agent crates are additive workspace members. The existing
`pulsebeam-agent` implementation and the sibling `pulsebeam-js` repository are
reference/protected trees and are not modified by this architecture.

```text
                    value inputs/events/effects
 runtime             +---------------------------+
 native Tokio/I/O --|                           |-- browser/WASM APIs
                    |     pulsebeam-agent-core  |
                    |  SANS-IO AgentCore + data |
                    +---------------------------+
```

| Layer | Owns | Does not own |
| --- | --- | --- |
| `pulsebeam-agent-core` | lifecycle, generations, reconnect deadlines, signaling/session reduction, intents, allocation, topics, presets, E2EE framing | sockets, Tokio, browser APIs, ambient time |
| `pulsebeam-agent-native` | Tokio, UDP/TCP, `str0m`, RTP/media pipelines, timers, bounded mailboxes | shared client algorithms |
| `pulsebeam-agent-web` | browser peer connections, transceivers, data channels, media, fetch, timers, encoded-transform interop | lifecycle or protocol decisions |

The web adapter keeps the browser runtime behind the `browser` feature and
protocol/session/topic code behind `protocol`; E2EE adds the `e2ee` feature.
The size fixture measures both the minimal browser runtime and the production
`browser` + `protocol` + `e2ee` composition. It builds with `opt-level = "z"`,
one codegen unit, LTO, and panic abort. `make agent-wasm-size` enforces 51,200
bytes of minimal generated WASM and 102,400 bytes of production generated WASM
plus JavaScript after gzip; production raw size is reported separately.

Each adapter drives one `AgentCore`. Effects are consumed in FIFO order. An
adapter tags asynchronous results with the generation that created the
resource; stale results are rejected before they can affect current state.

Repository checks for the additive architecture are available as
`make agent-check`, `make agent-test`, `make agent-conformance`,
`make agent-wasm`, `make agent-wasm-size`, `make test-sim-agent-native`, and
`make protected-paths`. The ordinary `make test` and
`make lint-check` remain workspace-wide gates for the existing implementation
as well as the additive crates.
