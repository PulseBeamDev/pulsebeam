# PulseBeam Agent Native

Native Rust and generated foreign APIs for caller-fed media and PulseBeam session control. The shared actor executes `agent-core` effects through libwebrtc; applications own capture, device permissions and rendering.

This is an unpublished, not fully qualified Linux x86_64 integration. The generated Node and Python archives target glibc 2.39 or newer, Node 22 and Python 3.12 respectively. Other platforms, Node versions and Python versions are not qualified. Bazel builds the [local WebRTC bindings](../../crates/pulsebeam-libwebrtc-sys/README.md) from pinned sources. See [support and verification scope](QUALIFICATION.md) for proof owners and limits. Swift/Kotlin binding generation does not qualify mobile runtime execution; hardware qualification and release publication are separate.

For source builds, generated archives and release requirements, see [Build and publish](BUILDING.md). Standalone Cargo builds are not supported yet.

## Install the generated archives

The packages are not published to npm or PyPI. Obtain the archives from a trusted build before installing them. Use a Linux x86_64 host with glibc 2.39 or newer.

With Node 22, install the npm archive in your application. npm resolves its pinned `@ubjs` dependencies from the registry:

```sh
npm install --ignore-scripts /path/to/node_package.tgz
node --input-type=module -e 'import { Agent } from "@pulsebeam/agent/native"; console.log(typeof Agent.create)'
```

With Python 3.12, install the wheel in your virtual environment:

```sh
python -m pip install /path/to/pulsebeam_agent-0.3.2-py3-none-manylinux_2_39_x86_64.whl
python -c 'from pulsebeam_agent import pulsebeam_agent_native as native; print(native.Agent)'
```

The commands print the generated constructor or class, confirming that the package and native library load. They do not establish a session. Use an application-issued session endpoint and participant token to connect; applications must keep signing secrets on their backend. The Node namespaces are `@pulsebeam/agent/core` and `@pulsebeam/agent/native`; Python uses `pulsebeam_agent.pulsebeam_agent_core` and `pulsebeam_agent.pulsebeam_agent_native`.

## Configure and connect

Rust callers supply a core `AgentConfig`, optional native `MediaConfig`, and an HTTP `Host`, then call `Agent::spawn`. This selects `ThreadedRuntime` with normal native sockets. Construction validates configuration without connecting. Call `connect` to establish connection intent; use owned publications and subscriptions for media. Core remains the owner of topology, authorization, topics and recovery policy.

Generated APIs use the named async constructor `ffi::Agent::create(AgentConfig)` (Python: `await Agent.create(config)`). It creates the same actor with default PCM/I420 input and a production HTTP client. Native-only encoder/input choices use `create_with_media(NativeAgentConfig)` instead. Native configuration contains `session` and `media`, not application-owned UDP/TCP sockets. The generated interface exposes owned logical resources, not raw desired-state replacement or publication-bound submission ports. Controlled Rust hosts can still execute explicit low-level transport fixtures.

Factory input choices are independent for audio and video:

| Choice | Input |
| --- | --- |
| `AudioInput::Pcm` (default) | Owned PCM samples, native Opus encoding |
| `AudioInput::EncodedOpus { channels }` | Owned Opus packets matching the declared mono/stereo source; factory input is fixed to this mode |
| `VideoInput::I420` (default) | Owned CPU I420 pixels, stock native H.264 encoding |
| `VideoInput::EncodedH264` | Owned Annex-B H.264 access units |

`MediaConfig.video_publications` names local core slots and their native encodings. Raw video admits one to three encodings. Encoded H.264 admits one encoding or three simulcast encodings, not two, with a uniform L1T1 or L1T3 profile. Multiple encodings require distinct valid RIDs and appropriate scale/rate declarations. L1T3 requires dependency-descriptor configuration and actual negotiation on attached senders before encoded input is admitted. Unsupported combinations return errors rather than adding a peer, transcoding, or substituting a runtime-specific pipeline. Both runtimes explicitly register the stock H.264 decoder, including with encoded local input. For L1T1 input, an absent temporal index or explicit base index zero is accepted and normalized to the native absent-index representation; enhancement indices are rejected. Each L1T3 unit requires its genuine temporal index (0, 1 or 2), with keyframes at index zero and the producer's base-layer sync flag preserved.

## Own sources and observations

### Named publications and independent inputs

`video(label)` and `audio(label)` return inert typed logical handles. Lookup uses
Core-owned key validation without acquiring publication or receive demand.
Equivalent wrappers identify the same resource; they do not bypass exclusive
publication ownership.

Create an independently owned native input, then lend it to a publication. With a
spawned Rust `agent`:

```rust
use pulsebeam_agent_native::agent_core::ffi::PublicationConfig;

let camera = agent.create_video_input(Default::default()).await?;
let publication = agent.video("camera".into())?
    .publish(&camera, PublicationConfig { enabled: true }).await?;
agent.connect().await?;
publication.ready().await?;
// Capture owned I420 frames and submit them through camera.submit(frame).await.
publication.configure(PublicationConfig { enabled: false }).await?;
publication.set_source(None).await?;
publication.close().await?;
camera.close().await?;
```

Generated callers use concrete `VideoSource`/`AudioSource` arguments and
`VideoPublication`/`AudioPublication` owners. Python uses
`await agent.video(label).publish(source, core.PublicationConfig(enabled=True))`.
Audio inputs use `create_audio_input`; native video inputs take
`VideoSourceConfig`. No source objects are nested in configuration records.

Keep the source alive independently of the publication. `set_source(None)`
withdraws its borrow, `configure` changes enabled state, and `close` releases
publication ownership. None stops an independently owned producer. Closing or
dropping the final source handle stops that producer and makes dependent input
unavailable without closing publication ownership. A replacement source and its
retired predecessor have separate lifetimes. Native sources are Agent-bound;
multi-Agent sharing and wider source fan-out are not promised.

Publication `ready` waits for an enabled, Core-authorized native binding. It does
not prove frame admission or remote delivery. Native source `ready` instead
waits for producer allocation and does not imply a publication is bound. A source
can admit frames while detached or muted; submission acknowledges native input
admission, not encoding or delivery. A disconnected source returns `NotReady`
until its native producer is rebuilt. Logical sources and publication owners
survive disconnect; terminal Agent close ends both.

`capture_time_us` reads the current producer's native clock. Queued frames and
clock reads capture one physical incarnation and never move to a replacement.
A stale queued operation can return `Closed` while the logical producer remains
open; discard that operation and capture fresh input for the current target.
Video `feedback` remains open across physical target changes. `set_limits` is a
native extension scoped to the captured authorized publication binding and
returns its native encoding readback, not an unbound producer configuration.

The shared pending media-command queue has four entries and an aggregate 16 MiB
payload limit per submission. Admission reports backpressure, invalid input,
native refusal or closure explicitly. Encoded simulcast takes three genuine
encoding-specific access units in declared RID order, with equal capture
timestamps and ascending dimensions, not duplicated payload labeled as different
rungs. Temporal indices may differ between rungs of an L1T3 batch.

### Remote observations

Retain a named subscription instead of selecting a receiver slot. With a spawned `agent`, subscribe to the publisher's external participant
ID and immutable publication label:

```rust
use pulsebeam_agent_native::{
    agent_core::VideoObservation, ReceiveMode,
};

let subscription = agent.participant("publisher".into())?.video("camera".into())?
    .subscribe(VideoObservation { visible: true, height: 720, ..Default::default() }).await?;
let observation = subscription.observe(ReceiveMode::Decoded).await?;
// Consume owned updates with observation.next().await.
observation.close().await;
subscription.close().await?;
```

For audio, use the same imports and spawned agent:

```rust
let subscription = agent.participant("publisher".into())?.audio("microphone".into())?
    .subscribe(Default::default()).await?;
let observation = subscription.observe(ReceiveMode::Decoded).await?;
// Consume owned PCM updates from this exact native receiver track.
observation.close().await;
subscription.close().await?;
```

No audio is received implicitly by default. Encoded audio uses the same parent
with `ReceiveMode::Encoded` and retains the native Opus-only, exclusive-mode
restrictions.

Keep the subscription alive while using its observations. It can wait for an
absent publication and survives connection reconstruction. Each returned
`RemoteMedia` is a terminal physical observation: after closure, call
`subscription.observe` again for the next matching incarnation. A renamed
publication does not match the old subscription. `configure_video` updates video
demand; `configure_audio` sets audio playout delay. Neither renames the interest,
and using the wrong media kind is refused. Shared audio interests use adaptive
playout; changing to a non-adaptive delay requires exclusive ownership. The
stock native profile rejects fixed audio delay; adaptive is the default. Closing the subscription ends its pending and active
observations. Do not mix owned subscriptions with raw desired state for the
same media kind.

For explicit collection audio reception, retain `Agent::select_audio` with the
shared Core `AudioSubscription` policy. Set `automatic: true` to receive all
remote audio; use its pinned IDs, named selections and delay maps for narrower
policies. `Subscription::configure_audio_policy` replaces that owner's policy.
It can coexist with named adaptive subscriptions through the same Core receive
reconciler. Non-adaptive policy delays require exclusive audio ownership.
Reception still requires actual negotiated receiver facts.

Use `Subscription::observe_source(source_id, mode)` for an exact opaque source ID
from installed `Snapshot.audio_mapping` entries. This observes existing policy
demand; it does not pin the source or create receive demand. Each returned
observation ends permanently on source or receiver retirement. A fresh matching
physical receiver requires a fresh observation, never a different source under
the old handle. Removing a source from the policy ends that owner's observations
without disturbing another owner's demand. Use `observe(mode)` only with a
named parent; a collection policy does not identify one implicit source.

Generated callers use `participant(id).video(label).subscribe(core.VideoObservation)`
or `participant(id).audio(label).subscribe(core.AudioSubscriptionConfig)` and the
returned typed subscription's `observe(mode)`. Video preferences contain only
display demand, not a receiver slot or publication ID. Explicitly visible video
with height zero has no height ceiling; set `visible: false` to withdraw display
demand. `configure(record)` updates each owner's configuration. Collection audio
uses `select_audio(core.AudioDemand)` and a distinct `AudioSelection` with
`configure` and `observe_source`; it is not audible playback. Headless native
APIs do not fabricate capture devices or playback controls.

### Media delivery and retirement

The raw `Agent::remote` and foreign `remote_video`/`remote_audio` methods are
removed. Acquire observations through an owned subscription instead.

- Video-only `ReceiveMode::EncodedAndDecoded` observes native I420 and ordered encoded AUs on the same receiver without replacing its decoder. Native receiver-lifetime restrictions are surfaced as refusals; changing or reattaching modes may require a new generation.
- Select `ReceiveMode::Decoded` for native receive buffering, built-in decoding and playout output. Encoded video passively observes native-ordered complete access units and forwards originals unchanged; those observations alone do not establish built-in decoding or UI presentation.
- Rust-only `Config::decoded_video_observer` optionally observes synchronous owned I420/error facts before the unchanged polling retention queue. Do not block or synchronously control the SDK in this callback. Its native generation/receiver identity, host callback time and SDK global native callback microseconds are observational facts, not display completion. Map clock domains explicitly; polling loss and callback failure are separate. Default and generated-language configuration is unchanged. See [qualification scope](QUALIFICATION.md).
- `RemoteMedia::next` returns owned track/frame updates, native sink drops, observer lag or terminal closure. Physical receiver retirement closes its observations even when the session generation is unchanged. Use the retained named subscription to acquire a fresh observation; Core owns readiness across reconstruction. Each physical receiver remains bound to one exact source incarnation. A replacement source requires a fresh physical receiver and observation. Each receiver has at most 32 observers, with four queued updates per observation channel. Closing a stream invalidates and releases queued observations; values already returned remain owned and valid.

`NativeReceiverInfo` describes the native generation, receiver, RTC track and transceiver MID/media index, separately from Core's logical `RemoteTrack` record. `RemoteMedia::source_track_id` returns the Core-authorized opaque publication ID for an owned observation; it is not the RTC track ID. Catalog publication/selector state is a separate core snapshot. An owned PCM observation associates its exact receiver track with that publication; individual PCM blocks do not carry invented publication, SSRC or RTP timestamp fields. Optional frame metadata is populated only when the native callback supplies it.

Snapshot, event and producer-feedback observations are bounded and report `Lagged`/`Closed` explicitly. Native statistics are owned SDK report JSON plus its generation and native timestamp, not synthetic transport counters.

`disconnect` requests graceful core disconnection and waits for its disconnected/native-retired state, preserving logical publications, independent sources and subscriptions for a later `connect`. `close` is terminal and idempotent: it immediately rejects further work, lets Core complete disconnection and bounded signaling cleanup, then retires all logical resources and completes pending observations. `is_closed` reports terminal intent; a closed Agent cannot reconnect. Rust-only `abort` immediately disposes the actor without signaling, for controlled failure injection and emergency cancellation. It is not part of the generated common interface. Dropping the final Agent handle also requests immediate disposal; retained media and observation handles do not retain the RTC graph.

Observation APIs must serve a concrete production consumer and action. Before
expanding them, show why existing source, remote, snapshot/event or statistics
contracts are inadequate, choose the minimum stable semantics and justify
compatibility, portability, privacy and maintenance costs. Test summaries and
public wrappers around private state are not sufficient reasons. Keep private
structural invariants inline in production owners; principled simulation fault
injection does not turn its bookkeeping into an application observation.

## Binary data

`Agent::data(label, delivery)` and `Agent::participant(id)?.data(label, delivery)` return inert logical handles. Participant IDs are application external IDs, not native receiver coordinates. Lookup validates the key without acquiring transport demand. Equivalent handles do not need to be the same object.

A local handle's async `send(bytes)` uses Core's lazy Agent-lifetime producer admission. It requires connected intent and resolves on transport admission, not remote delivery or durable acknowledgment. Core settlement failures remain typed as `NativeError::DataSend`; resource refusals, including disconnected intent, use `NativeError::Resource` with a core-owned reason. Topic admission errors, including oversized payloads and full send queues, use `NativeError::Topic` with their structured core-owned reason.

Call `receive` on a local handle for room-wide incoming data or on a participant's handle for participant-bound data. Each call owns an independent bounded `DataReceiver`. Its async `next` returns `DataUpdate::Event` containing a core `DataEvent::Message` or `Gap`, or terminal `DataUpdate::Closed`. Message bytes are owned. No Agent-global event filtering or manual topic registration is required. Ordered reception starts at a live frontier, not historical replay; local send admission does not prove that a remote cursor is ready.

Core owns buffering and delivery policy: latest coalesces by publisher track, lossy evicts at capacity, and ordered delivery represents loss inline as gaps or terminates with a typed receive-overflow error when it cannot preserve that representation. Channel failures also terminate the affected cursor. After a terminal read error, subsequent reads return `Closed`.

Closing a receiver releases only its demand and completes its pending read. Dropping its final handle also releases that demand. Disconnect preserves logical receivers for a later connection; terminal Agent close completes all pending reads. Retained logical data handles and cursors do not keep the Agent's native graph alive.

## Controlled execution

`Agent::managed(config, host, runtime_factory)` returns the same agent API and a non-Send actor future. The outer driver launches that future on its controlled world's OS thread and supplies a managed runtime for each generation. It owns one process-exclusive `ControlledWorld`, native packet delivery, pumping and deadline/time advancement. Threaded libwebrtc graphs must not coexist with that controlled world. There is no alternate simulated media backend or decoder injection.

## Contributor checks

Use the commands in [Build and publish](BUILDING.md#check-the-build). Binding generation reads the native shared library and emits the native and core namespaces. Core-owned portable types remain in core. Generated files are build artifacts; change Rust definitions and run the owner generator, never edit generated output.

Receive preparation uses the existing per-peer executor through successful
local-offer and remote-answer completion before Core submits the complete
binding for server admission. Completion is correlated to the current generation,
operation and owner; cancellation retains applied history and safe cleanup.
Packet arrival, statistics and exhaustive receiver-parameter readback are not
readiness gates. See [Core's receive flow](../pulsebeam-agent-core/README.md#prepared-reception)
for policy and completion fences.

Implementation owners are `native_actor` (effects and lifecycle), `native_peer` (one generation), `native_source`/`native_receiver` (native resources), `native_media` (owned values), and `ffi` (thin foreign boundary). `media` retains an application-side Annex-B fixture slicer. The former str0m transport, RTP packetization, jitter and socket-pumping modules are removed.
