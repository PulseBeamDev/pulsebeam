# Routing

## The model

The controller is a compiler. A shard is a packet executor.

The controller owns semantic state: rooms, participants, published tracks and
subscriptions. It resolves that state into shard-local keys, route handles and
one forwarding plan per resident track. The shard installs those values and
executes them without evaluating room policy, subscription selectors or media
kind.

```text
                         control plane

  publish Track ─┐
                 ├─► indexed topology ─► reconcile one Track
  subscribe ─────┘                              │
                                               ▼
                           per-shard ordered generation
                      lifecycle + participant effects + plans
                                               │
                 ┌─────────────────────────────┼─────────────────────────┐
                 ▼                             ▼                         ▼
              shard 0                       shard 1                   shard 2


                           data plane

  packet + TrackKey
          │
          ▼
  TrackPlan { local participants, remote routes, reverse route }
          │
          ├─► local ParticipantKey ─► participant
          │
          └─► one envelope per destination shard
                                      │
                                      ▼
                            RouteAction::Forward
                                      │
                                      ▼
                              destination TrackKey
                                      │
                                      ▼
                         destination-local TrackPlan
```

The invariant is:

> Routing is unified at Track and Participant level. Audio, video and data may
> have different publication and subscription policy, but they do not have
> different shard routing machinery.

## Publish Track, subscribe Track

Every routable publication has one `TrackIdentity`:

```text
TrackIdentity = RoomId + publisher ParticipantId + TrackId
```

`TrackKind` remains semantic information for signaling and participant-local
processing. The control plane uses it to select tracks; it is not compiled into
a shard route action or a forwarding-plan variant.

Subscriptions are selectors over the same track catalog:

```text
TrackSelector {
    track:     exact | any
    publisher: exact | any
    kind:      exact | any
    label:     exact | any
}
```

The room is the enclosing catalog, not a selector field. A selector can only
match publications in that room, so cross-room matching is impossible by
construction. Constraints are conjunctive and use the same indexed matcher for
every media kind.

Matching and activation are separate relations:

```text
room-local Track catalog
        │ selector matching
        ▼
participant candidates
        │ all-matches policy or participant allocator
        ▼
active participant ↔ Track bindings
        │ compile
        ▼
TrackPlan
```

This expresses the current policies without creating separate selector or
routing systems:

- audio receivers select `kind = Audio` and activate every match;
- data receivers subscribe by a topic label, optionally restricted to one
  publisher, with `kind = Data`, and activate every match;
- video receivers select `kind = Video`; every match becomes a candidate, while
  each participant's allocator chooses which tracks and layers its negotiated
  slots actively consume.

An audio wildcard cannot match a data publication because the selector includes
`TrackKind::Audio`. A data topic selector cannot accidentally match audio or
video because only data publications carry a matching publication label.

Selectors belong entirely to the controller. A shard never sees a wildcard,
topic, room id or stable participant id while forwarding a packet.

## The unified compiled plan

For each track, the controller groups candidates and active bindings by their
owning shard. It allocates one shard-local `TrackKey` on the publisher shard and
reserves one destination `TrackKey` plus `RouteHandle` on every remote shard
that has candidates. A candidate-only destination remains dormant: it has no
forward route, runtime, plan or origin remote entry. Those are installed only
while the shard has at least one active binding.

Every resident copy uses the same plan:

```text
TrackPlan {
    local:         [ParticipantKey]
    remote:        [RouteHandle]
    reverse_route: optional RouteHandle
}
```

- `local` is the exact participant fanout on this shard.
- `remote` contains one route per destination shard, not one route per remote
  participant. The destination plan performs the second-stage local fanout.
- `reverse_route` addresses feedback or reliable control toward the publisher.

There is no generic destination type and no audio, video, realtime-data or
reliable-data plan image. A shard has one `SecondaryMap<TrackKey, TrackPlan>`.
It also has only two endpoint actions:

```text
RouteAction::Forward { target: TrackKey }
RouteAction::Reverse { target: TrackKey }
```

`Forward` maps an arriving envelope to the destination shard's local track
key. `Reverse` maps feedback to the publisher-side track key, whose runtime
already identifies the publishing participant. Both actions are `Copy` values
containing dense keys; neither ends in a hash lookup by a stable name.

The participant boundary owns the remaining semantics. A routed packet is
delivered as a track packet plus `TrackKey`. Participant-local state maps that
key to negotiated audio/video slots or data channels and performs allocation,
codec handling and reliability work. The shard does not need those decisions
to fan out the track.

## Forwarding

### Local origin

When a participant emits a packet, the shard pipeline already carries its
compiled `TrackKey`.

1. Resolve the track runtime and `TrackPlan` by `TrackKey`.
2. Deliver the packet to every `ParticipantKey` in `plan.local`.
3. If the packet originated locally, emit one copy for every route in
   `plan.remote`.

Each remote copy has a fixed envelope and an owned payload. The envelope route
selects the destination shard. The payload contains the destination-independent
track packet; the receiving shard replaces its source key with the key compiled
into the destination route.

### Remote arrival

The destination shard:

1. indexes the route table by slot and validates the epoch;
2. resolves `RouteAction::Forward { target }`;
3. resolves the destination-local plan by `target`;
4. fans out only to `plan.local`.

A remotely received packet is never forwarded remotely again. Only a local
origin executes `plan.remote`, preventing loops and duplicate multi-hop fanout.

The route runtime keeps mutable hop-local accounting beside the route action:
sequence expansion, loss, reorder and duplicate counters. This state is owned
by the destination shard and is not part of the controller's plan.

### Reverse traffic

Keyframe requests and reliable data acknowledgements use the same reverse
shape. The route identifies the publisher-side `TrackKey`; the small body
carries only information that cannot be derived from the track, such as an
encoding index or acknowledgement bytes.

Reverse messages are best-effort and idempotent. A consumer repeats a request
while it still needs the result. One reverse route can therefore be shared by
all destination shards instead of allocating one per sender.

## Publishing plans to shards

Routing plans are not shared-memory snapshots. The controller sends owned
`ShardUpdate` values through a bounded mailbox. Each update has a strictly
increasing generation and contains three kinds of work:

```text
ShardUpdate {
    participant_effects
    lifecycle
    plans
}
```

Application is ordered for safety:

1. insert participants and install new transports, track runtimes and routes;
2. apply participant effects;
3. replace or remove track plans in bounded chunks;
4. retire old routes, transports, participants and track runtimes;
5. advance the shard generation.

This is an ordered generation transaction, not an atomic whole-shard swap.
Plan entries may become visible incrementally. Installs happen before any plan
can reference them, and retirements happen only after the plan phase, so a
partially applied generation can drop a racing packet but cannot reinterpret it
as another participant or route incarnation.

The controller never waits for the packet path. If the update mailbox is full,
the writer retains ordered generations in its own backlog and retries. The
shard applies only a bounded amount of plan work per tick. Forwarding never
takes a control-plane lock, waits for a generation or helps compile topology.

Large lifecycle batches must remain exceptional. Plan replacement is chunked,
but lifecycle operations in one update are currently applied as a batch; a
change that can generate an unbounded lifecycle vector must split it before
publication or extend the shard's lifecycle cursor. This is a forwarding-tail
requirement, not merely a control-plane throughput concern.

## Complexity contracts

Let:

- `M` be the number of subscriptions matching one track;
- `L` be the number of local recipients in a resident plan;
- `D` be the number of remote destination shards for the origin plan;
- `C` be the number of plan entries changed by one topology event.

The forwarding path is:

| Operation | Cost | Reason |
| --- | ---: | --- |
| resolve `TrackKey` | O(1) | dense slot-map lookup |
| resolve route and epoch | O(1) | dense route-table index |
| local fanout | O(L) | one delivery per required recipient |
| remote fanout | O(D) | one copy per required destination shard |
| topology interpretation | O(0) | absent from the shard hot path |

`O(L + D)` is output-sensitive and unavoidable: forwarding to a recipient or a
destination requires producing that output. Work unrelated to the track's
actual fanout is not allowed on this path.

The control plane uses room-local hash indexes for exact track ids, publishers,
kinds and publication labels. Matching a known track is expected O(M), plus the
work required to compile and materialize its candidates and active bindings.
Reason sets deduplicate overlapping subscriptions. Route and track-key
allocation are amortized O(1).

An operation may enumerate a room only when the requested result itself is
room-wide, such as applying a new kind selector to existing publications.
Global scans whose output is not global are defects. In particular, cleanup and
secondary indexes must evolve so track removal, subscription removal and
destination allocation do not scan all rooms, all subscriptions or all
allocations as the node grows.

On the shard, applying C plan changes costs O(C) in total and is spread across
ticks. Lifecycle work has the batching caveat above. The latency requirement is
not that churn completes in O(1); it is that churn cannot monopolize a shard
long enough to damage forwarding P99.99 residence time.

The retired shard microbenchmark is intentionally not a routing benchmark.
Forwarding performance must be measured through a production node and real
agents, including socket, signaling, negotiation, delivery validation, and
route churn. Those measurements are informational and environment-dependent;
their setup and delivery checks are correctness failures.

## Addresses and stale-route safety

Stable names are control-plane identities. Packets use compiled addresses.

```text
31                    20 19                     0
+-----------------------+------------------------+
| shard: 12 bits        | slot: 20 bits          |
+-----------------------+------------------------+
```

The packed layout supports 4096 shard ids and 1,048,576 slots per shard. Two
semantic route families wrap it and use separate allocator namespaces:

- `TransportHandle = TransportRoute + epoch` addresses client ICE, DTLS and
  SRTP state;
- `RouteHandle = RouteId + epoch` addresses a forwarded or reverse track
  endpoint.

The types are intentionally not interchangeable even though their packed bits
match.

Every live address includes a 16-bit epoch. A destination accepts a handle only
when both slot and epoch match. Retired slots are quarantined before reuse, and
their epoch advances when reused. A delayed packet therefore fails closed
instead of reaching the next occupant. A route never changes shard; migration
allocates a new handle and retires the old one.

Endpoint routes are allocated by the control plane from the destination
shard's namespace. The destination owns the table entry and packet accounting.
The shard does not allocate routes or ask for one while processing a packet.

## Client ingress

The transport route is encoded in the ICE ufrag. A first STUN packet or a flow
miss may land on an admitting shard selected by `SO_REUSEPORT`; userspace
decodes the ufrag and forwards the packet once to the transport owner when
needed. The owner still performs ICE, DTLS and SRTP authentication. A ufrag is
a steering hint, not a credential.

After authentication, the controller installs a 5-tuple-to-shard entry in the
eBPF `FLOWS` map when eBPF steering is available. Established packets then land
on the owner directly. If the eBPF object is unavailable, the userspace path
remains authoritative and correct.

The owner reports authentication with the shard that admitted the flow. The
controller installs kernel affinity and sends that admitting shard an explicit
authentication command so its userspace demux cache protects the established
entry. NAT rebinding returns through the same STUN bootstrap path. ICE-TCP uses
the ufrag to hand the whole accepted connection to its owner once.

## The envelope

Cross-shard media and reverse traffic use one fixed 16-byte header:

```text
version (1) | type (1) | epoch (2) | route (4) | extension (8)
```

The route is at a fixed offset so steering needs only a bounded load. For media,
the extension carries a hop-local sequence and compact playout timestamp.
Feedback uses the same header and interprets the body through the envelope type.

In-process shard transport currently carries the typed envelope and owned
payload through bounded mailboxes. The same envelope is the cross-node wire
contract; semantic ids are not added when transport moves off-node.

## Invariants

A change that breaks one of these is an architectural regression even when its
tests pass:

1. The controller owns rooms, track identities, selectors and placement.
2. The shard routes only by `TrackKey`, `ParticipantKey` and route handle.
3. Every track kind uses the same `TrackPlan` and `Forward` action.
4. A plan contains exact outputs, never selectors or stable application ids.
5. A remote origin sends one copy per destination shard; that shard performs
   local fanout and never forwards the packet again.
6. Forwarding does no control-plane work, blocking or shared mutable access.
7. Plan application is bounded; installs precede plans and retirements follow
   them.
8. Transport and endpoint routes use separate namespaces.
9. Every route reference carries an epoch and the destination validates it.
10. A route never changes shard; migration mints a new route.
11. The packed route and envelope offsets are shared by production and
    simulation implementations.
12. Any O(N) control-plane operation is output-sensitive or has an explicit,
    documented reason.

## Code map

- `control/topology.rs` owns track identities, selectors and indexed matching.
- `control/controller.rs` reconciles a track and compiles per-shard plans.
- `shard_update.rs` defines the unified plan and ordered update message.
- `shard/core.rs` applies updates and owns the packet loop.
- `shard/router.rs` executes local, remote and reverse track forwarding.
- `route.rs` defines packed routes, epochs, actions and the envelope.
- `participant` owns audio/video/data interpretation after track delivery.

The wire constants in code are authoritative. The invariants in this document
are the architecture; a disagreement there must be resolved deliberately.
