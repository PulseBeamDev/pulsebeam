# PulseBeam Agent Core

SANS-I/O PulseBeam client agent.

`agent-core` contains the platform-independent protocol and state machines shared by every environment. It performs no I/O itself.

## Owns

* Declarative client desired state
* Connection and reconnection state machines
* Signaling protocol
* Publication and subscription state
* Topic semantics
* Timers and retry decisions
* Effects describing required I/O
* Events representing completed I/O
* Snapshots and notifications
* Production diagnostics and per-agent filtering through the `log` facade
* Shared UniFFI-compatible domain types

## Receive policy support

Use adaptive audio playout with the stock native and browser WebRTC profiles.
They do not negotiate the audio RTP playout-delay extension. The native resource
receiver rejects fixed audio with `FailureCode::Unsupported`; it does not use a
receiver-local minimum-delay setter as a substitute for the full
`Fixed { min_ms, max_ms }` interval.

Native signaling evaluates each resource independently. At most one sent
ordinary proposal per resource may remain unevaluated; newer unsent desire is
coalesced until that evaluation arrives. Other resources progress independently,
and retirement can bypass the gate. Evaluation does not wait for physical
execution to become installed.

A rejected target does not replace its previously accepted resource or roll back
unrelated resources. Application-owned desired state is not rewritten. Wire
representation alone does not establish a backend's support for a receive policy.

## RTC bootstrap

Core requests the Native control channel on externally negotiated physical SCTP
stream 0, reliable and ordered. Both platform hosts apply the specification and
check the actual channel configuration before returning offer resources. Native
topic payload channels also use externally negotiated, nonzero streams with
resource-owned bindings. Core can reuse a payload stream within the same
connection only after resource cutoff and a backend close completion proving
both reset directions on the original healthy association, settled sends, and
fenced old-lifetime delivery. A failed or unproven reset never permits reuse.
`DataChannelSpec.negotiated_id` is a physical stream ID, separate from the
logical `ChannelId` used for host callbacks.

## Server-directed reception

Core owns receive policy; platform adapters execute it through the existing
per-peer SDP executor. Bootstrap HTTP capabilities describe server support, not
an endpoint's decoder capabilities or an installed receiver.

1. `OfferReceiveMedia` creates or adopts the requested physical receiver. After
   applying the local offer, the host uses `Context::receive_offer` to observe its
   MID, codec/PT choices, extension mappings and RTCP modes, then returns
   `MediaOffered`. The subscription carries that offer, not client-assigned
   receive SSRCs or CNAME.
2. Core retains the same operation while waiting for the server's immutable
   binding on the owned resource. `ApplyReceivePlan` supplies that binding for
   `Context::prepare_receive_plan`; the host applies the resulting answer.
3. After actual remote-description completion, the host calls
   `Context::observe_receive_answer` while the candidate is pending and checks
   actual receiver parameters and the live receiver/track. Finish the candidate
   with its actual apply outcome even if observation fails, preserving applied
   identity history. Missing readback is not installation evidence.
4. A matching completion produces an independent execution report. Core exposes
   receiver mappings and observation targets only when local prepared facts match
   the plan and the server confirms installation. Neither cached bootstrap facts
   nor server status alone establishes readiness.

Generation, operation, resource-reference and source-lifetime fences apply
throughout. Cancellation repairs the existing operation; hosts retain owned
transceivers and tracks until stop/description repair is confirmed. Scoped
preparation failure does not install the receiver or rewrite unrelated desire.
See the [wire receive contract](../../crates/pulsebeam-proto/README.md#media-receive-plans-and-execution).

## Track resources

`TrackSpec` identifies media by kind and label, and data by label and delivery
mode. Constructing or comparing a specification does not reserve transport
capacity. `TrackResources` accounts for explicit publication, video presentation,
audio playback, named or policy-based audio interests, and data receive owners independently of
connection generations.
Give each declaration or stream lifetime a fresh `new_owner()` token, including
when replaying a committed declaration. Releasing an owner cannot release a
replacement owner's resource.

`Agent::resource` applies owned declarations through the existing reconciler,
independently of the revisioned slot-based desired state. Resource-owned sender
slots become reusable after retirement. Relabeling a publication replaces its
owned resource and physical binding rather than retaining a permanent
slot-to-label association. Slot-based intent cannot replace a live owned
publication or video presentation. Owned audio interests cannot be mixed with raw
audio desired state. Core defaults to no automatic audio reception; hosts that
intend global audio must opt in explicitly. `ClaimAudio` remains the separate
global playback reservation, while `ClaimAudioSubscription` declares a named
source. Several owners of the same audio source share adaptive playout. A
non-adaptive delay requires exclusive ownership; shared owners must be released
before changing it. Delays are not merged.

`ClaimAudioPolicy` owns the existing `AudioSubscription` selection policy,
including explicit automatic, pinned and named selections. `ConfigureAudioPolicy`
replaces that owner's policy. Policy and named owners project a deduplicated
union into the same receiver reconciler. Non-adaptive policy delays require
exclusive audio ownership, since opaque IDs and names can overlap after
discovery. No observation contributes receive demand.

A policy owner uses `BeginAudioSourceObservation` with an exact opaque source ID,
not a receiver slot. Core checks that owner's selection authority and targets
only an installed receiver for that source. Removing the source from the owner's
policy closes its observations even if another owner still demands it. Confirmed
source end closes both pending and delivered exact-source requests; discovery
absence alone is not source end. The parent policy survives physical retirement.

A named video or audio owner can issue `BeginMediaObservation` requests independently of
its presentation settings. Core emits `MediaObservationControl` effects and
accepts authorized, once-only completions for the current request and target.
Pending acquisition can wait through absence. A completed physical observation
is terminal when its source or receiver scope retires; it never follows a
replacement. The parent owner remains available for a fresh request. Targets
carry the opaque publication TrackId, durable receiver-owned reference,
connection generation and actual slot binding, not the transient allocation
operation. Unconfirmed settings alone do not invalidate an installed target.
Hosts execute attachment, report completion or physical termination, and release
canceled requests. Frames and platform objects stay outside Core.

Video visibility is explicit: `visible: false` removes display demand. A visible
observation with height zero has no height ceiling. When same-name consumers
share video, an unbounded height request remains unbounded rather than taking
another consumer's finite ceiling.

The resource model projects publication intents, shared video demand, owned audio
demand, and topic
registrations. Feed its receive queues only validated,
deduplicated `TopicNotification` values from the topic transport. Queues preserve
opaque bytes, participant filtering, delivery-mode bounds, and inline ordered
gaps. Hosts own browser objects, pending reads, and physical teardown, not a
second copy of this policy.

`Agent::send_data` admits owned binary payloads and retains producer interest for
the Agent lifetime. Binding and dispatch wait is bounded to ten seconds, with at
most 256 pending sends per publisher and a 65,536-byte payload limit. Dispatch
cancels the admission deadline; host acceptance or failure then settles the
returned ticket through `Notification::DataSendFinished`. Success is not a remote
receipt acknowledgment. Latest
supersession, disconnect, terminal or channel failure, binding replacement,
timeout, and host rejection settle affected tickets without retargeting bytes.
The legacy native `SendTopic` command retains its queue-admission semantics.

Report host failures through `Agent::host_failed` rather than overriding a
snapshot. Core owns failure notification, pending-send rejection, and explicit
`connect` retry through the same connection state machine.

## Does not own

* HTTP execution
* WebRTC implementation
* Timers or async runtime
* Media devices
* Threads or executors
* Browser or native platform APIs
* Logger installation or output sinks

```text
desired state + events
        ↓
     agent-core
        ↓
 effects + snapshots
```

Environment runtimes execute effects and feed resulting events back into the core.

Core always emits UniFFI metadata for its portable records and enums so native
and web runtimes cannot compile against a reduced contract. Runtime crates
consume those types as external UniFFI types instead of defining platform
copies. TypeScript generation settings live in this crate's `uniffi.toml` so
core-owned byte arrays and strictness follow the core namespace.

## Reconcile local SDP without server renegotiation

`client_sdp` is an independent, sans-I/O local-offerer utility. Create one
`Context` from the successfully applied bootstrap offer/answer and an explicit
server `Capabilities` envelope. SCTP-only bootstrap is supported: capabilities
are never inferred from a data-only answer. Supply a connection-specific SSRC
seed and `Limits`; no randomness or identity set is required from the host.

The adapter serializes operations and applies answers to its PeerConnection.
This native-independent helper shows the generic prepare/apply/accept boundary.
Managed reception uses the [server-directed flow](#server-directed-reception),
not standalone `Preparation.reception` identity allocation:

```rust
use pulsebeam_agent_core::client_sdp::{
    ApplyOutcome, Binding, Bindings, Capabilities, Context, Encoding, Error,
    Limits, Preparation, RtcpParameters,
};

fn prepare_media(
    bootstrap_offer: &str,
    applied_bootstrap_answer: &str,
    capabilities: Capabilities,
    seed: u32,
    current_offer: &str,
    requests: &[Preparation],
    apply_answer: impl FnOnce(&str) -> ApplyOutcome,
) -> Result<(Context, Option<Bindings>), Error> {
    let mut context = Context::new(
        bootstrap_offer, applied_bootstrap_answer, capabilities, seed,
        Limits::default(),
    )?;
    let candidate = context.prepare(current_offer, requests)?;
    let outcome = apply_answer(candidate.answer());
    context.finish(candidate.id(), outcome)?;
    let bindings = context.bindings().cloned();
    Ok((context, bindings))
}

fn finalize_sender(
    context: &mut Context,
    mid: &str,
    actual_encodings: &[Encoding],
    actual_rtcp: &RtcpParameters,
) -> Result<Option<Bindings>, Error> {
    let facts = context.observe_sender(mid, actual_encodings, actual_rtcp)?;
    let complete = facts.publications.iter().chain(&facts.receptions)
        .all(Binding::complete);
    Ok(complete.then(|| facts.clone()))
}
```

Keep the returned context for subsequent offers. `inspect` exposes offered
MID, codec/FMTP/PT/RTX, directional extension, SSRC/FID and RID facts; callers do
not need another SDP parser or MID map. A preparation correlates publication
and reception facts to opaque caller keys, with optional logical grouping.
If bootstrap already contains media, call `adopt_bootstrap_media(&requests)` once
before preparing subsequent offers to attach caller keys to its applied RTP
facts. This does not apply another answer. SCTP-only bootstrap needs no adoption.
Unchanged sections are reconciled automatically. Codec replacement requires an
explicitly prepared fresh compatible section. Inactivation retains identities;
stopped-MID reuse requires fresh identities. A stopped section can disappear
when the current offer recycles its m-line under a new MID; its reserved
identities remain unavailable. Standalone receive identity allocation avoids
collisions across the context's bounded history, including cancelled candidates.
Managed receivers instead retain the server-selected identities across unrelated
offers and reject retargeting or removal of a selected extension.

Individual descriptions and generated answers are limited to 1 MiB, with
additional line bounds. There is no separate whole-description section or
aggregate encoding count ceiling. Preparation/adoption requests must fit the
actual description's media count; sender observations must match the accepted
encoding count before copying. The executor allows at most 32 unfinished operations and 2 MiB of retained
request payloads. It charges per-work headers, actual request vector capacity
and owned string capacities. Backing is checked before building validation
indexes; allocation and retirement batches have no separate item-count ceiling.
Requests still lower against actual sections and slots in the bounded SDP.
Host staging, platform allocations and allocator overhead are outside this
payload allowance; it is not an RSS bound or proof of physical platform scale.

`Limits::max_identity_bytes` bounds retained identity payloads (1 MiB by default),
replacing the former identity count quota and validation maximum. It charges
issued-ID/retired-flag tuple payloads and sender MID/RID/primary-or-repair ownership
records, including owned string capacities. Allocator/tree-node overhead and
temporary staging are excluded; this is not a total-memory or RSS bound. The
retirement flag is prepaid at admission. Terminal retirement releases sender
ownership payloads but never forgets the issued ID or terminal veto. Sender
observation batches preflight incremental guard and ownership charges before
committing any reservations or accepted facts. Exhaustion rejects new work
without evicting safety history; failed/cancelled candidates retain issued IDs.

An RID-only publication is not ready for admission: `Binding::complete()` stays
false until the host passes actual sender encodings to
`context.observe_sender(mid, &encodings, &rtcp)` after successful application.
Absent runtime RIDs remain absent. Actual SSRC-only encodings can complete a
binding; slot names and encoding positions are not RIDs. MID, primary SSRC,
negotiated repair SSRC, and RTP/RTCP facts must still agree.
Supply actual sender RTCP observations with the encodings. An unexposed runtime
CNAME retains the value from applied SDP; if neither source supplies it, the
binding stays incomplete. Conflicting CNAME or mode observations fail atomically. CNAME is
limited to the SDES field's 255 bytes. A binding needs one common CNAME across its
primary and repair sources; unsupported mixed CNAMEs reject rather than merge.
Standalone fresh local answers select compound RTCP. Authoritative receive
plans select an offered, supported mode. Applied bootstrap reduced-size mode is
retained only when offered, and continuing answers preserve the applied CNAME
and mode. Submit only complete publication bindings; receivers report observed
application of the server's binding. Runtime bitrate/framerate, activation and
scalability observations remain separate from negotiated identities. Mutable
receive preferences do not retarget a physical receiver: a replacement source
incarnation requires a fresh receiver and authoritative plan.

Report `NotApplied` for failure or cancellation before application.
`AppliedButCancelled` preserves actual SDP history while suppressing declaration;
the adapter owns physical cleanup and stale-completion fencing. Reconnect uses
a new context. Pending candidates are limited to one. Errors do not include SDP
or ICE credentials.

The supported subset is one bundled ICE/DTLS transport, RTCP mux, an existing
SCTP association, and explicitly selected offered codec/extension envelopes.
ICE restart, DTLS identity/role changes, SCTP replacement and simulcast RID
alternatives require higher-layer recovery or a different negotiator. This
utility neither executes PeerConnection operations nor integrates a production
signaling protocol or media-routing policy.

## Verification

Run `./bazel test //agents/pulsebeam-agent-core:unit_tests` while iterating.
The repository gates are `./bazel test //:check` and `./bazel test //:test`.

## Ordered subscription recovery

A temporary empty discovery view during reconnect does not establish
publication retirement. Transport replacement clears queued payloads while
retaining frontiers for still-owned logical subscriptions. Unsubscribe and
session close release frontiers. New source IDs start live without inheriting
another source's cursor; ordered gaps keep their bounded-history semantics.

Native StateBatch does not implement the legacy Catalog revision or
`SubscriptionSeed` completion protocol. Its `initial_complete` marker describes
initial discovery enumeration, not a source-lifetime or recovery barrier.
Core replays retained registration/source frontiers on fresh payload bindings,
without waiting for discovery to replay. Only current eligible owned source
confirmations make those channels routable and permit recovery probes. Physical
detach retains an owned frontier; authoritative source end releases it. An
unknown-source timeout is not source termination.

Applications do not manage internal ownership generations or wire cursors.
