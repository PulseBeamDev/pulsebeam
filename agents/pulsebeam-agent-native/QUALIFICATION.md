# Native candidate support and verification scope

Use this page to choose a receive mode, identify the relevant contributor proof,
and understand what the local candidate does not establish. Start with the
[native API guide](README.md) for source ownership and session setup.

## Supported local integration

The candidate is unpublished and targets Linux x86_64. Applications provide
capture, device permissions, rendering and credentials. The same libwebrtc actor
runs with production `ThreadedRuntime` services or controlled execution services;
core owns session, topology, topics and recovery policy.

Use the matching source/native archives selected by
[`tools/webrtc-candidate.json`](../../tools/webrtc-candidate.json), not an arbitrary
producer checkout or an archive with the same filename. Follow the
[candidate preparation procedure](../../tools/README.md#use-the-local-native-webrtc-candidate).
The source Git revision, both archive digests and producer provenance must agree.
Native C++ adapter changes require a matching native artifact export. Extracted
candidate inputs and generated foreign bindings are not editable source.

Local inputs are owned PCM/I420 or admitted encoded Opus/Annex-B H.264. H.264
supports L1T1, single-encoding L1T3, and the documented three-encoding L1T1
simulcast input. This is not an opaque encrypted-video or arbitrary-codec
passthrough guarantee. Native validation and feedback remain authoritative;
successful input admission does not promise encoding or remote delivery.

## Choose the observation that matches the task

| Application task | Observation | What it establishes |
| --- | --- | --- |
| Play or inspect decoded video | `ReceiveMode::Decoded`, owned I420 frames and native receiver statistics | Actual built-in decoding and native playout output. |
| Inspect encoded access units or run an independent decoder | `ReceiveMode::Encoded`, owned H.264 payload and available metadata | Completed encoded capture, not a decoder-ready ordering/dependency guarantee. |
| Play decoded audio | `ReceiveMode::Decoded`, owned PCM | Actual native decoded audio, without invented per-block publisher/SSRC attribution. |
| Inspect native transport or codec statistics | Generation-tagged SDK report JSON | Native fields that the SDK actually reports, not synthesized missing counters. |

Encoded interception consumes frames before the peer decoder. Zero peer decoder
frame counters in that mode do not diagnose a decoder failure. An independent
OpenH264 probe is a different oracle from libwebrtc's own I420 output. A pass in
one mode neither replaces nor waives assertions in the other mode. A receiver
cannot promise both modes simultaneously; native lifetime restrictions may
require a new generation to change modes.

After generation replacement, reacquire source/remote handles and use current
generation observations. Retired handles do not reattach automatically. Returned
owned frames remain valid after stream closure; closure does not keep the RTC
graph alive. Observe native sink drops and application lag rather than treating
missing observations as successful delivery.

## Focused contributor proof owners

Set up the candidate first. The table names executable owners, not a claim that
every aggregate gate is green. Use the [testing contract](../../docs/build.md#testing-contract)
to select affected checks and reuse unchanged evidence.

| Boundary | Executable owner |
| --- | --- |
| Archive pairing and negative mismatch checks | `//tools:webrtc_candidate_contract` |
| Native API, admission, controls and lifecycle components | `//agents/pulsebeam-agent-native:unit_tests`, `:format`, `:clippy` |
| Current dynamic ABI, generated Python use, real-SFU I420 media and retirement | `//crates/pulsebeam-cli:production_native` |
| Generated Swift/Kotlin/Python artifacts and native library variants | `//agents/pulsebeam-agent-native:bindings`, `:native_static`, `:pulsebeam-agent-native-sim` |
| Shared controlled/native RTC interoperability | `//crates/pulsebeam-rtc/simulation:interop` |
| Web artifact/type/runtime contracts | `//agents/pulsebeam-agent-web:node_runtime_tests`, `:uniffi_types`, `:public_contract` |
| Irreducible browser glue against the local SFU | `//agents/pulsebeam-agent-web:browser` |

The existing simulator replay owner also includes distinct focused scenarios:

- `tests::native_runtime::native_agents_prove_media_topics_reconnect_and_close`:
  real native media, topics, generation replacement and graceful retirement.
- `tests::connectivity::a_tcp_connection_recovers_as_the_same_participant_test`:
  controlled plain-TCP framing and same-participant recovery. This is not general
  production TCP/TLS or TURN qualification.
- `tests::video::marker_only_simulcast_reaches_native_decoded_video_test`:
  marker-only three-rung ingress to a DD-capable receiver, requiring at least 150
  fresh changing 720p native I420 frames in a ten-second world and native decoder
  progress. The added witness executed in 8.99 seconds excluding build time; it is
  not a throughput benchmark. The separate raw encoded-frame oracle is unchanged.

## Acceptance and distribution limits

This delivery is not full native MVP acceptance. Passing focused proofs or
finishing unblocked delivery work does not mean `//:test`, full simulator
acceptance, or every recovery/media scenario passed. The
[ordered-recovery checkpoint](../pulsebeam-agent-core/ORDERED_RECOVERY_CHECKPOINT.md)
retains its exact historical candidate and unresolved qualification boundary.
The raw encoded-frame/independent-decoder scenario also remains unqualified;
the native-I420 witness is not its replacement.

Static/dynamic library builds and foreign binding generation establish artifact
compatibility only for the exercised toolchain. Generated Swift/Kotlin code does
not qualify mobile runtime behavior. No hardware capture/codec, other platform,
release publication or remote deployment is claimed. Native-agent execution
uses libwebrtc; the workspace's remaining str0m consumers do not define its
production or controlled runtime.
