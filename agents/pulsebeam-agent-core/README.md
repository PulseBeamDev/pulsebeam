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

## Track resources

`TrackSpec` identifies media by kind and label, and data by label and delivery
mode. Constructing or comparing a specification does not reserve transport
capacity. `TrackResources` accounts for explicit publication, video presentation,
audio playback, and data receive owners independently of connection generations.
Give each declaration or stream lifetime a fresh `new_owner()` token, including
when replaying a committed declaration. Releasing an owner cannot release a
replacement owner's resource.

`Agent::resource` applies owned declarations through the existing reconciler,
independently of the revisioned slot-based desired state. Resource-owned sender
slots become reusable on release; legacy native slot-label bindings retain their
existing lifetime. Slot-based intent cannot replace a live owned publication or
video presentation.

The resource model projects publication intents, shared video demand, and topic
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
This native-independent helper shows the prepare/apply/accept boundary:

```rust
use pulsebeam_agent_core::client_sdp::{
    ApplyOutcome, Bindings, Capabilities, Context, Error, Limits, Preparation,
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
```

Keep the returned context for subsequent offers. `inspect` exposes offered
MID, codec/FMTP/PT/RTX, directional extension, SSRC/FID and RID facts; callers do
not need another SDP parser or MID map. A preparation correlates publication
and reception facts to opaque caller keys, with optional logical grouping.
Unchanged sections are reconciled automatically. Codec replacement requires an
explicitly prepared fresh compatible section. Inactivation retains identities;
stopped-MID reuse requires fresh identities. Receive identities are allocated
without collisions across the context's bounded history, including cancelled
candidates.

An RID-only publication is not ready for admission: `Binding::complete()` stays
false until the host passes actual sender encodings to
`context.observe_sender(mid, &encodings)` after successful application. Submit
only complete publication bindings to the server's typed API. Runtime
bitrate/framerate, activation and scalability observations remain separate from
negotiated identities. Source or quality selection behind a compatible receive
binding is caller policy and does not require SDP.

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

## Native cutover checkpoint

Native ordered recovery across reconnects remains blocked, despite passing Core
component tests. See the [ordered-recovery checkpoint](ORDERED_RECOVERY_CHECKPOINT.md)
for the partial repair, exact reproducer, evidence, and unresolved catalog lifecycle
boundary. This is not final native MVP qualification.
