# PulseBeam Agent Native

Native Rust and generated foreign APIs for caller-fed media and PulseBeam session control. The shared actor executes `agent-core` effects through libwebrtc; applications own capture, device permissions and rendering.

This is an unpublished, not fully qualified Linux x86_64 integration. The generated Node and Python archives target glibc 2.39 or newer, Node 22 and Python 3.12 respectively. Other platforms, Node versions and Python versions are not qualified. Bazel builds the [local WebRTC bindings](../../crates/pulsebeam-libwebrtc-sys/README.md) from pinned sources. See [support and verification scope](QUALIFICATION.md) for proof owners and limits. Swift/Kotlin binding generation does not qualify mobile runtime execution; hardware qualification and release publication are separate.

For source builds, generated archives and release requirements, see [Build and publish](BUILDING.md). Standalone Cargo builds are not supported yet.

## Install the generated archives

The packages are not published to npm or PyPI. Obtain the archives from a trusted build before installing them. Use a Linux x86_64 host with glibc 2.39 or newer.

With Node 22, install the npm archive in your application. npm resolves its pinned `@ubjs` dependencies from the registry:

```sh
npm install --ignore-scripts /path/to/node_package.tgz
node --input-type=module -e 'import { Agent } from "@pulsebeam/agent/native"; console.log(typeof Agent)'
```

With Python 3.12, install the wheel in your virtual environment:

```sh
python -m pip install /path/to/pulsebeam_agent-0.3.2-py3-none-manylinux_2_39_x86_64.whl
python -c 'from pulsebeam_agent import pulsebeam_agent_native as native; print(native.Agent)'
```

The commands print the generated Agent class, confirming that the package and native library load. They do not establish a session. Use an application-issued session endpoint and participant token to connect; applications must keep signing secrets on their backend. The Node namespaces are `@pulsebeam/agent/core` and `@pulsebeam/agent/native`; Python uses `pulsebeam_agent.pulsebeam_agent_core` and `pulsebeam_agent.pulsebeam_agent_native`.

## Configure and connect

Rust callers supply a core `AgentConfig`, optional native `MediaConfig`, and an HTTP `Host`, then call `Agent::spawn`. This selects `ThreadedRuntime` with normal native sockets. Construction validates configuration without connecting. Call `agent.join("conference-42".into())?` to select a room and request admission. The participant token must grant Join for that room. Use owned publications and subscriptions for media. Core remains the owner of topology, authorization, topics and recovery policy.

Generated APIs construct synchronously with `new Agent(AgentConfig)` in Node and `Agent(config)` in Python. The returned Agent already exposes its real Core state, starts idle with no room intent, and performs no join, request, device acquisition or RTC setup before an explicit operation. Call `agent.join("conference-42")` in Node or Python to request admission. The call returns without waiting for admission or media readiness. Native-only encoder/input choices use the distinct synchronous `Agent.createWithMedia(NativeAgentConfig)` constructor in Node and `Agent.create_with_media(config)` in Python. Native configuration contains `session` and `media`, not application-owned UDP/TCP sockets. The generated interface exposes owned logical resources, not raw desired-state replacement or publication-bound submission ports. Controlled Rust hosts can still execute explicit low-level transport fixtures.

Factory input choices are independent for audio and video:

| Choice | Input |
| --- | --- |
| `AudioInput::Pcm` (default) | Owned PCM samples, native Opus encoding |
| `AudioInput::EncodedOpus { channels }` | Owned Opus packets matching the declared mono/stereo source; factory input is fixed to this mode |
| `VideoInput::I420` (default) | Owned CPU I420 pixels, stock native encoding |
| `VideoInput::EncodedH264` | Owned Annex-B H.264 access units |

Each video publication uses one encoding, including when `MediaConfig.video_publications`
sets a profile for a local slot. Multiple encodings are rejected. For independent
streams, create separate named publications such as `camera` and `screen`.

A single H.264 stream can use L1T1 or L1T3. Encoded L1T3 input requires configured
and negotiated dependency descriptors. Supply the genuine temporal index (0, 1,
or 2) and base-layer sync flag for each unit; keyframes use index zero. For L1T1,
omit the temporal index or set it to zero. Both runtimes use the stock H.264
decoder, including when the local input is already encoded.

## Discover one bounded live page

Use `agent.room(name).watch()` to own a live catalog page independently of media
subscriptions. Generated Node and Python expose `Watch`, `WatchState`,
`WatchUpdates`, `WatchConfig` and `WatchStatus` from the Native/Core namespaces.
The default limit is 100 participants, maximum 500; an Agent can own eight
pending/live watches. Supply a `WatchConfig` to select the immutable limit and
cursor. State starts Loading, not as a successful empty page.

A Live state's page contains participant IDs, application external IDs and
publication metadata, including participants with no publications. In generated
Python, read `state.participants`, `state.has_more`, `state.next_cursor` and
`state.cursor_expires_at_unix_seconds`; Node uses the corresponding camelCase methods.
Retained page values stay stable across updates. The SDK retains one current page
per watch, not a history or every continuation.

Use the next cursor only with the same room and limit by closing/replacing the
watch. Continuations expire after 60 seconds and confer no authority. Invalid,
expired or wrong-scope continuations fail explicitly; restart without a cursor
for the first page. Check Failed/error instead of displaying an empty roster.

Discover authorizes metadata independently of Subscribe. Select a page entry's
external ID and label for pinned participant/track lookup; creating a reference
receives nothing. Known-address subscriptions do not require Discover. Skip self
when creating receive owners. Public session snapshots no longer contain a global
catalog or receiver mapping.

`updates()` is current-first and coalesced, with up to 32 independent cursors and
one pending `next()` per cursor. Close each cursor separately. Watch close is
synchronous in generated APIs and releases its reservation without closing
independent subscriptions. A healthy close publishes Closed and clears the page;
closing a failed watch preserves its terminal failure.

## Own sources and observations

### Named publications and independent inputs

`video(label)` and `audio(label)` return inert typed logical handles. Lookup uses
Core-owned key validation without acquiring publication or receive demand.
Equivalent wrappers identify the same resource; they do not bypass exclusive
publication ownership.

Create an independently owned native input, then lend it to a publication. With a
spawned Rust `agent`:

```rust
use pulsebeam_agent_native::agent_core::ffi::{PublicationConfig, VideoPublishOptions};

let camera = agent.create_video_input(Default::default()).await?;
let publication = agent.video("camera".into())?
    .publish(VideoPublishOptions::default())?;
let pipe = camera.pipe_to(&publication)?;
agent.join("conference-42".into())?;
publication.ready().await?;
// Capture owned I420 frames and submit them through camera.submit(frame).await.
publication.configure(PublicationConfig { enabled: false }).await?;
pipe.close().await?;
publication.close().await?;
camera.close().await?;
```

Generated `publish(options?)` returns a `VideoPublication` or `AudioPublication`
synchronously without requiring input or waiting for admission. Omitting options
enables the publication by default. Python uses
`publication = agent.video(label).publish()`, followed by
`pipe = source.pipe_to(publication)` to attach an existing concrete
`VideoSource` or `AudioSource`. Audio inputs use `create_audio_input`; native video inputs take
`VideoSourceConfig`. No source objects are nested in configuration records.

Video defaults to H.264. To publish AV1, use I420 input and set `codec` when
calling `publish`: `VideoCodec::Av1` in Rust, `VideoCodec.Av1` in Node, or
`VideoCodec.AV1` in Python. The codec is fixed for that publication. To change
it, close the publication and create a new one; you can reuse the I420 source.
After connection, observe `admission`: a missing compatible sender is reported
as `Rejected` with `Unsupported`, not as an H.264 fallback. On success,
`effective_format` (`effectiveFormat` in Node) reports the negotiated format.

The controlled Linux Native runtime verifies stock AV1 encoding and decoding
through the public API. `EncodedH264` input does not support AV1 publication.
Mobile, hardware codec, and other native runtime combinations are not qualified
for AV1 by these checks.

Retain the pipe for as long as the publication borrows the source. Closing the
pipe withdraws that borrow, `configure` changes enabled state, and closing the
publication releases publication ownership. None stops an independently owned producer. Closing or
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
native refusal or closure explicitly. Submit one H.264 access unit per frame.

### Remote observations

Retain a named subscription instead of selecting a receiver slot. With a spawned `agent`, subscribe to the publisher's external participant
ID and immutable publication label. Create the reference with the explicit room
name supplied to `join`; the examples use `"room"`. References remain pinned
when the Agent joins another room. Creating a room or participant reference
performs no network operation. Receiving from another room requires a Subscribe grant for that room and source:

```rust
use pulsebeam_agent_native::{
    agent_core::VideoObservation, ReceiveMode,
};

let subscription = agent.room("room".into())?.participant("publisher".into())?.video("camera".into())?
    .subscribe(VideoObservation { visible: true, height: 720, ..Default::default() }).await?;
let observation = subscription.observe(ReceiveMode::Decoded).await?;
// Consume owned updates with observation.next().await.
observation.close().await;
subscription.close().await?;
```

For audio, use the same imports and spawned agent:

```rust
let subscription = agent.room("room".into())?.participant("publisher".into())?.audio("microphone".into())?
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
demand without renaming the interest. Audio uses the native receiver's default
buffering. Closing the subscription ends its pending and active observations. Do not mix owned subscriptions with raw desired state for the
same media kind.

To discover audio sources, use a bounded room watch with Discover permission.
Then subscribe to each chosen participant and track with Subscribe permission.

Generated callers use `room(name).participant(id).video(label).subscribe(core.VideoObservation)`
or `room(name).participant(id).audio(label).subscribe()` and the
returned typed subscription's `observe(mode)`. Video preferences contain only
display demand, not a receiver slot or publication ID. Explicitly visible video
with height zero has no height ceiling; set `visible: false` to withdraw display
demand. `configure(record)` updates a video owner's preferences. Generated bindings
require an exact audio source for each subscription. Headless native APIs do not
fabricate capture devices or playback controls.

### Media delivery and retirement

The raw `Agent::remote` and foreign `remote_video`/`remote_audio` methods are
removed. Acquire observations through an owned subscription instead.

- Video-only `ReceiveMode::EncodedAndDecoded` observes native I420 and ordered encoded AUs on the same receiver without replacing its decoder. Native receiver-lifetime restrictions are surfaced as refusals; changing or reattaching modes may require a new generation.
- Select `ReceiveMode::Decoded` for native receive buffering, built-in decoding and playout output. Encoded video passively observes native-ordered complete access units and forwards originals unchanged; those observations alone do not establish built-in decoding or UI presentation.
- Rust-only `Config::decoded_video_observer` optionally observes synchronous owned I420/error facts before the unchanged polling retention queue. Do not block or synchronously control the SDK in this callback. Its native generation/receiver identity, host callback time and SDK global native callback microseconds are observational facts, not display completion. Map clock domains explicitly; polling loss and callback failure are separate. Default and generated-language configuration is unchanged. See [qualification scope](QUALIFICATION.md).
- `RemoteMedia::next` returns owned track/frame updates, native sink drops, observer lag or terminal closure. Physical receiver retirement closes its observations even when the session generation is unchanged. Use the retained named subscription to acquire a fresh observation; Core owns readiness across reconstruction. Each physical receiver remains bound to one exact source incarnation. A replacement source requires a fresh physical receiver and observation. Each receiver has at most 32 observers, with four queued updates per observation channel. Closing a stream invalidates and releases queued observations; values already returned remain owned and valid.

`NativeReceiverInfo` describes the native generation, receiver, RTC track and transceiver MID/media index, separately from Core's logical `RemoteTrack` record. `RemoteMedia::source_track_id` returns the Core-authorized opaque publication ID for an owned observation; it is not the RTC track ID. Publication/selector state is observed through owned resources; catalog metadata is obtained through an owned room Watch, not a public session snapshot. An owned PCM observation associates its exact receiver track with that publication; individual PCM blocks do not carry invented publication, SSRC or RTP timestamp fields. Optional frame metadata is populated only when the native callback supplies it.

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

`Agent::data(label, delivery)` and `Agent::room(name)?.participant(id)?.data(label, delivery)` return inert logical handles. Participant IDs are application external IDs, not native receiver coordinates. Lookup validates the key without acquiring transport demand. Equivalent handles do not need to be the same object.

Call `publish()` on a local handle to synchronously reserve an owned `DataPublication`. Send bytes through that publication's async `send(bytes)` and close it when publishing ends. Sending requires connected intent and resolves on transport admission, not remote delivery or durable acknowledgment. Generated operation failures expose the shared `ErrorInfo` classification.

Call `subscribe()` on a participant's data handle for that exact room, participant and label. The call returns an owned subscription immediately, even before joining or publication. To consume several matching publications, subscribe to a room data set. Exact readers and sets inherit each publication's delivery mode. Local data handles only publish. Each call owns an independent bounded subscription (`DataReceiver` in Rust, `DataSubscription` in generated bindings). Its async `next` returns `DataUpdate::Event` containing a core `DataEvent::Message` or `Gap`, or terminal `DataUpdate::Closed`. Message bytes are owned. No Agent-global event filtering or manual topic registration is required. Ordered reception starts at a live frontier, not historical replay; local send admission does not prove that a remote cursor is ready.

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
