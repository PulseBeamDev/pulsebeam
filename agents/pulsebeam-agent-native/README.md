# PulseBeam Agent Native

Native Rust and generated foreign APIs for caller-fed media and PulseBeam session control. The shared actor executes `agent-core` effects through libwebrtc; applications own capture, device permissions and rendering.

This is an unpublished Linux x86_64 candidate. Obtain the matching local sys source/native archives and follow the [candidate setup](../../tools/README.md#use-the-local-native-webrtc-candidate) before building. Contributor proofs cover controlled real-SFU media/topics/reconnect and a bounded production raw-I420 encode/decode witness. The production witness also calls the generated Python API through the native dynamic library. Swift/Kotlin binding generation does not qualify mobile runtime execution; hardware qualification and release publication are separate.

## Configure and connect

Rust callers supply a core `AgentConfig`, native `MediaConfig`, and an HTTP `Host`, then call `Agent::spawn`. This selects `ThreadedRuntime` with normal native sockets. Submit a complete core `DesiredState` to connect and publish/subscribe; core remains the owner of topology, authorization, topics and recovery policy.

Swift, Kotlin and Python use the generated named async constructor `ffi::Agent::create(NativeAgentConfig)` (Python: `await Agent.create(config)`). It creates the same actor with a production HTTP client. Native configuration contains `session` and `media`, not application-owned UDP/TCP sockets. Foreign `replace_desired` accepts a complete core-owned desired state; the shared actor assigns its revision in control-command order, including close and Rust-originated changes.

Factory input choices are independent for audio and video:

| Choice | Input |
| --- | --- |
| `AudioInput::Pcm` (default) | Owned PCM samples, native Opus encoding |
| `AudioInput::EncodedOpus { channels }` | Owned Opus packets matching the declared mono/stereo source; factory input is fixed to this mode |
| `VideoInput::I420` (default) | Owned CPU I420 pixels, stock native H.264 encoding |
| `VideoInput::EncodedH264` | Owned Annex-B H.264 access units |

`MediaConfig.video_publications` names local core slots and their native encodings. H.264 configuration admits L1T1 or single-encoding L1T3. Raw video admits one to three encodings; the existing encoded-input factory admits one encoding or three L1T1 simulcast encodings, not two. Multiple encodings require distinct valid RIDs and appropriate scale/rate declarations. Unsupported combinations return errors rather than adding a peer, transcoding, or substituting a runtime-specific pipeline. Both runtimes explicitly register the stock H.264 decoder, including with encoded local input.

## Own sources and observations

- `Agent::source(MediaSlot)` acquires/replaces the producer in a local slot. Foreign callers use async `local_video`/`local_audio`. Retain the returned `LocalSource` while producing.
- `LocalSource::submit(InputFrame)` takes owned PCM, I420, Opus or H.264 data. Encoded simulcast takes three genuine encoding-specific access units in declared RID order, not duplicated payload labeled as different rungs.
- Successful submission acknowledges native input admission, not encode completion or remote delivery. The pending media-command queue has four entries and an aggregate 16 MiB payload limit per submission; admission reports `Backpressure`, invalid input, native refusal, or closure explicitly.
- Source `close` detaches the producer. Replacement and generation retirement invalidate old source identities. Dropping the final source handle schedules detachment; handles contain no engine pointer and do not keep a closed native generation alive.
- `set_enabled` returns native track readback. `encoding_state` and `set_limits` expose native sender parameter readback; `feedback` reports native keyframe requests, encoding-specific requests, rate control and encoder errors.
- `Agent::remote(MediaSlot, ReceiveMode)` selects an encoded or decoded native sink. Foreign callers use `remote_video`/`remote_audio` with the same explicit mode. A receiver cannot promise simultaneous encoded interception and decoded output. Native receiver-lifetime restrictions are surfaced as refusals; changing mode may require a new generation.
- Select `ReceiveMode::Decoded` for native receive buffering, built-in decoding and playout output. Encoded video intercepts completed access units before the native decoder; its observations are not a decoder-ready playout-order guarantee.
- `RemoteMedia::next` returns owned track/frame updates, native sink drops, observer lag or terminal closure. Each receiver has at most 32 observers, with four queued updates per observation channel. Closing a stream invalidates and releases queued observations; values already returned remain owned and valid.

Remote frame identity describes the native generation, receiver, track and transceiver MID/media index. Catalog publication/selector state is a separate core snapshot. In particular, decoded PCM does not claim per-block publication, SSRC or RTP timestamp attribution. Optional frame metadata is populated only when the native callback supplies it.

Snapshot, event and producer-feedback observations are bounded and report `Lagged`/`Closed` explicitly. Native statistics are owned SDK report JSON plus its generation and native timestamp, not synthetic transport counters.

`close` requests graceful core disconnection and waits for its disconnected/native-retired state. `abort` cancels work and retires resources without completing graceful signaling. Abort and source closure are idempotent. Dropping the final agent handle requests abort; retained media and observation handles do not retain the RTC graph.

## Controlled execution

`Agent::managed(config, host, runtime_factory)` returns the same agent API and a non-Send actor future. The outer driver launches that future on its controlled world's OS thread and supplies a managed runtime for each generation. It owns one process-exclusive `ControlledWorld`, native packet delivery, pumping and deadline/time advancement. Threaded libwebrtc graphs must not coexist with that controlled world. There is no alternate simulated media backend or decoder injection.

## Contributor checks

With candidate archive environment variables set:

```sh
./bazel test //agents/pulsebeam-agent-native:unit_tests \
  //agents/pulsebeam-agent-native:clippy \
  //agents/pulsebeam-agent-native:format \
  //crates/pulsebeam-cli:production_native
./bazel build //agents/pulsebeam-agent-native:bindings \
  //agents/pulsebeam-agent-native:pulsebeam-agent-native-sim
```

Binding generation reads the native shared library and emits the native and core namespaces. Core-owned portable types remain in core. Generated files are build artifacts; change Rust definitions and run the owner generator, never edit generated output.

Implementation owners are `native_actor` (effects and lifecycle), `native_peer` (one generation), `native_source`/`native_receiver` (native resources), `native_media` (owned values), and `ffi` (thin foreign boundary). `media` retains an application-side Annex-B fixture slicer. The former str0m transport, RTP packetization, jitter and socket-pumping modules are removed.
