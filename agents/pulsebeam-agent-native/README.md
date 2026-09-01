# PulseBeam Agent Native

Native runtime for the PulseBeam agent.

`agent-native` executes the SANS-I/O effects produced by `agent-core` using native platform facilities. New code starts with `Agent::spawn`, supplies complete `DesiredState` values, and observes coherent snapshots or ordered events.

## Owns

* Native WebRTC integration
* Native media integration
* HTTP execution
* Timer execution
* Runtime and lifecycle plumbing
* Native resource handles
* UniFFI bindings for native environments

## Contributor map

* `runtime`: the small public API and the serial owner of core effects, RTC generations, HTTP, timers, sockets, media endpoints, coherent media bindings, and graceful or abrupt teardown;
* `pipeline`: RTP packetization, frame assembly, jitter handling, and media metadata;
* `media`: bounded Annex-B access-unit slicing for encoded fixture sources;
* `tcp`: RFC 4571 framing and bounded partial-write handling;

## Does not own

* Signaling policy
* Reconnection policy
* Topic protocol semantics
* Platform-independent agent behavior
* Application UI

```text
Rust / Swift / Kotlin / other hosts
              ↓
         agent-native
              ↓
          agent-core
```

Run `cargo test -p pulsebeam-agent-native` for focused work. The real-server deterministic scenario is `tests::native_runtime::native_agents_prove_media_topics_reconnect_and_close` in `pulsebeam-simulator`.
The simulator and benchmark CLI both use this runtime directly.
