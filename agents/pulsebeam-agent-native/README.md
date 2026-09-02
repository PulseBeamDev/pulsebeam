# PulseBeam Agent Native

Native runtime for the PulseBeam agent.

`agent-native` executes the SANS-I/O effects produced by `agent-core` using native platform facilities. Rust infrastructure starts with `Agent::spawn`; Swift and Kotlin use the generated `ffi::Agent` constructor. Both paths reach the same serial runtime and core state machine.

## Public boundaries

Direct Rust callers provide `Host` so simulation and CLI code can inject deterministic HTTP and sockets. The UniFFI constructor is the production host: it binds UDP, creates the HTTP client, optionally connects active TCP, discovers network interfaces, and keeps those implementation details out of generated APIs.

The foreign surface accepts the configuration, complete desired state, snapshots, notifications, topics, errors, media frames, and statistics defined by `agent-core`. Native adds only host options, simulcast declarations, object streams, and native lifecycle objects. Desired revisions are assigned by one actor. Snapshot, event, and remote-media streams have async `next` methods and report `Lagged` explicitly when a consumer falls behind their bounded buffers.

Applications own capture and encoding. A local sender validates and takes an owned encoded byte buffer; a remote receiver returns an owned copy. The SDK does not request permissions, open devices, capture, encode, decode, render, or stop application media.

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
* `ffi`: the generated-facing actor facade, portable/native conversion, bounded observation streams, and production host constructor;
* `pipeline`: RTP packetization, frame assembly, jitter handling, and media metadata;
* `media`: bounded Annex-B access-unit slicing for encoded fixture sources;
* `tcp`: RFC 4571 framing and bounded partial-write handling;

## Does not own

* Signaling policy
* Reconnection policy
* Topic protocol semantics
* Platform-independent agent behavior
* Application UI
* Media capture, device selection, encoding, decoding, or rendering

```text
Rust / Swift / Kotlin / other hosts
              ↓
         agent-native
              ↓
          agent-core
```

Focused commands are:

* `just --justfile agents/pulsebeam-agent-native/Justfile check`;
* `just --justfile agents/pulsebeam-agent-native/Justfile test`;
* `just --justfile agents/pulsebeam-agent-native/Justfile bindings` to write Swift and Kotlin build artifacts under `target/uniffi/native`.

Binding generation reads the native dynamic library and emits both the `pulsebeam_agent_core` and `pulsebeam_agent_native` namespaces; native bindings reference core-owned records instead of redefining them in the native namespace. Generated sources are build artifacts, not hand-edited repository source.

The real-server deterministic scenario is `tests::native_runtime::native_agents_prove_media_topics_reconnect_and_close` in `pulsebeam-simulator`; it injects the deterministic host into the direct Rust runtime, then drives the exported facade. The benchmark CLI continues to use the direct Rust runtime.
