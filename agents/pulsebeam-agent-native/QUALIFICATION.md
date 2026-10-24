# Native support and verification scope

Use this page to choose a receive mode, identify the relevant contributor proof,
and understand what the local integration does not establish. Start with the
[native API guide](README.md) for source ownership and session setup.

## Supported local integration

The native integration is unpublished and targets Linux x86_64. Applications provide
capture, device permissions, rendering and credentials. The same libwebrtc actor
runs with production `ThreadedRuntime` services or controlled execution services;
core owns session, topology, topics and recovery policy.

Use the [local sys package](../../crates/pulsebeam-libwebrtc-sys/README.md), which
owns pinned upstream acquisition, behavior patches and the declared GN native
build. Core is the default; the package-owned flavor setting selects native for
the complete configured dependency graph. Rust bridge declarations and C++
adapters are editable local source and invalidate the applicable native action.
Bazel owns the resulting libraries and native desktop runtime closure. Generated
foreign bindings remain build outputs, not editable source.

Local inputs are owned PCM/I420 or admitted encoded Opus/Annex-B H.264. H.264
supports L1T1, single-encoding L1T3, and the documented three-encoding L1T1
simulcast input. This is not an opaque encrypted-video or arbitrary-codec
passthrough guarantee. Native validation and feedback remain authoritative;
successful input admission does not promise encoding or remote delivery.

## Choose the observation that matches the task

| Application task | Observation | What it establishes |
| --- | --- | --- |
| Play or inspect decoded video | `ReceiveMode::Decoded`, owned I420 frames and native receiver statistics | Actual built-in decoding and native playout output. |
| Inspect encoded access units or run an independent decoder | `ReceiveMode::Encoded`, native-ordered H.264 payload and available metadata | Actual complete-AU observations, not proof of built-in decoding or presentation. |
| Correlate native decoded output with encoded evidence | `ReceiveMode::EncodedAndDecoded`, both outputs from the same receiver | Native I420 and ordered encoded observations with actual generation/receiver/track/RTP identity. |
| Play decoded audio | `ReceiveMode::Decoded`, owned PCM | Actual native decoded audio, without invented per-block publisher/SSRC attribution. |
| Inspect native transport or codec statistics | Generation-tagged SDK report JSON | Native fields that the SDK actually reports, not synthesized missing counters. |

The ordered encoded tap forwards original frames to the configured decoder
unchanged. Encoded observations alone do not establish native decoded output;
read actual decoder statistics and I420 when that is the boundary under test.
An independent OpenH264 probe is a different oracle from libwebrtc's own I420
output. If a scenario asserts both, a pass in one neither replaces nor waives
assertions in the other. Select the oracle for the boundary being tested; the
[simulator playback contracts](../../crates/pulsebeam-simulator/README.md#native-playback-contracts)
separate native playback from independent encoded-codec checks.
Combined mode requires a decoding-capable factory, retains independent polling
queues, and preserves the encoded observer's receiver-lifetime reservation.
Changing or reattaching modes can therefore require a new receiver/generation.

After generation replacement, reacquire source/remote handles and use current
generation observations. Retired handles do not reattach automatically. Returned
owned frames remain valid after stream closure; closure does not keep the RTC
graph alive. Observe native sink drops and application lag rather than treating
missing observations as successful delivery.

Rust callers can opt into `Config::decoded_video_observer` for synchronous owned
I420/error facts from the same native sink before best-effort polling retention.
This adds neither a sink nor native wants, decoder, media policy or larger queue.
The observer captures actual native track identity before attachment. It must
not block or synchronously control the SDK; a panic retires it and fails native
progress. Default and generated-language configuration remains unchanged.
`observed_at` is the host callback clock, while `native_observed_at_us` is the SDK
global monotonic callback clock. The controlled simulator explicitly projects
the latter through its native/simulator scheduling anchor, including when the
callback has no Tokio context. Polling drops are not callback losses. Callback
collection/conversion failures invalidate strict native qualification. These
facts establish decoded output, not UI rendering or presentation completion.

For source correlation in combined mode, Rust callers can also configure
`Config::encoded_video_observer` alongside the decoded observer. It receives
owned access units through the existing native ordered tap before independent
polling retention can lose them. Encoded and I420 callbacks may arrive in either
order. Missing native identity or pending metadata produces an explicit `Dropped`
observation, which revokes source evidence; polling eviction does not revoke
independent callback facts. The simulator joins authenticated fixture bytes to
actual I420 using the full native track identity and RTP timestamp, never a
catalog assignment or guessed SSRC. Default and generated-language callers
install neither callback.

## Focused contributor proof owners

Use the local source build described above. The table names executable owners, not a claim that
every aggregate gate is green. Use the [testing contract](../../docs/build.md#testing-contract)
to select affected checks and reuse unchanged evidence.

| Boundary | Executable owner |
| --- | --- |
| Sys codec/runtime behavior, native linking and lifetime | `//crates/pulsebeam-libwebrtc-sys:fast`, `:runtime_tests`, `:lifetime` in separate core/native configurations |
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
- `tests::tcp_transport::`: established-stream simulator reliability, including
  one pooled HTTP connection with exactly one POST admission and capacity-one
  receive backpressure through FIN retransmission. SYN and final socket teardown
  retain the simulator's existing semantics; this is not general TCP qualification.
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
The noncatalog continuation adds same-receiver admitted-origin/RTP correlation.
Conference playback, publisher rejoin and slot replacement now assert actual
native I420, current source identity and bounded visibility/continuity instead
of encoded polling completeness or arbitrary replacement byte totals. These
scenarios retain authenticated encoded source samples, but do not independently
decode them or generate keyframe requests when that polling observer loses
samples. Dedicated encoded-forwarding/codec scenarios retain their independent
decoder assertions. The initial-ramp scenario retains its original two-second
window and encoded quality checks in addition to native proof.

After rebasing onto main's resource/catalog ownership changes, the latest
scoped regression passes all 183 fast and 15 slow committed cases with only the
two exclusions below, using the same pinned source/native pair. This includes
owned-publication relabel through a fresh native wire session: protocol identity
is checked against the actual receiver mapping/catalog, while successful source
decodes are independently authenticated to the current admission. Native receiver
track IDs are not treated as protocol publication IDs. Root checks, all
non-simulator fast owners, controlled RTC interoperability and browser checks
also pass. The original recovery roster and five-second
publication setup cases pass without changing their deadlines or assertions.
Established-stream TCP retransmission repairs the lost signaling request;
BoringSSL DTLS retransmission now uses the controlled native clock. Playback
source proof uses actual native callbacks rather than requiring complete encoded
polling. The native lifecycle case also passes; earlier retained freeze traces
have not been individually causally attributed by this renewal.

These scoped witnesses are not a replacement for whole-workspace acceptance.
The catalog-sensitive
`tests::histories::required_intent_histories` and
`tests::data_channel::ordered_recovery_reports_retention_gaps_without_replaying_late_history`
remain excluded from that continuation, not fixed or qualified.

Static/dynamic library builds and foreign binding generation establish artifact
compatibility only for the exercised toolchain. Generated Swift/Kotlin code does
not qualify mobile runtime behavior. No hardware capture/codec, other platform,
release publication or remote deployment is claimed. Native-agent execution
uses libwebrtc; the workspace's remaining str0m consumers do not define its
production or controlled runtime.
