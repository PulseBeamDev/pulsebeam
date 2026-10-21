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

## Verification

Run `./bazel test //agents/pulsebeam-agent-core:unit_tests` while iterating.
The repository gates are `./bazel test //:check` and `./bazel test //:test`.
