# Routing

## Overview

Think of the controller as a compiler and each shard as a small packet
executor. The controller turns room state into a concrete forwarding plan once;
the shard then executes that plan for every packet without scanning the room or
asking another shard what to do.

```text
                         CONTROL PLANE (slow path)

  rooms, participants, tracks, subscriptions
                         │
                         ▼
                    controller
             chooses owners and recipients
                         │
                         ▼
               compiled forwarding plan
              published as one coherent view
                         │
          ┌──────────────┼──────────────┐
          ▼              ▼              ▼
       shard 0        shard 1        shard 2
       owns ...       owns ...       owns ...


                          DATA PLANE (fast path)

  client ── UDP / ICE-TCP ──► classifier
                              eBPF, userspace or simulator
                                      │
                         route = shard + slot + epoch
                                      │
                ┌─────────────────────┴─────────────────────┐
                │                                           │
          first packet                               media / data
                │                                           │
                ▼                                           ▼
       admitting shard ──► transport owner           publisher shard
                │              │                           │
                │              └─ authenticate             │ read plan
                │                    │                     │
                │                    ▼                ┌────┴────┐
                │               controller            │         │
                │                    │             local      remote
                └── demux auth ◄─────┘                │         │
                                                     ▼         ▼
                                                direct     envelope(route)
                                                                  │
                                                                  ▼
                                                           recipient shard
                                                                  │
                                                    validate epoch, find key
                                                                  │
                                                                  ▼
                                                         execute media/data
```

### The data structures behind the picture

The controller starts with names that are useful for room logic. A
`ParticipantId`, `TrackId`, `RoomId` or data `Topic` can be looked up and
compared, but none of them belongs in the packet path. The controller compiles
those names into this shape:

```text
  controller state                         shard-owned published view
  ────────────────                         ──────────────────────────

  Publication {                             ShardView {
    id: TrackId                                generation
    publisher: ParticipantId                   transports: slot -> binding
    publisher_shard                            routes:     slot -> binding
    origin_key                                 video plans: local track key -> plan
    destinations:                              audio plans: local track key -> plan
      shard -> Destination                     data plans:  local stream key -> plan
    reverse_route                              }
  }

  Destination {
    Discovery { video_key }
    or
    Forwarding {
      key: shard-local runtime key
      route: RouteHandle
    }
  }

  ForwardingPlan<D> {
    recipients:   [(ParticipantKey, D)]
    remote_routes: [RouteHandle]
    reverse_route: Option<RouteHandle>
  }
```

`Publication` is the controller's catalog record. It knows the semantic
identity of a publication and, for each receiving shard, the local runtime key
and, when forwarding is installed, its route. A video destination can briefly
be `Discovery` while the shard learns about a track; the controller replaces it
with `Forwarding` before packets are sent. `Destination` is one record instead
of separate maps for audio, video and data, which keeps the shard identity and
the local key together.

`ShardView` is the compiled input to the data plane. Its route and transport
tables are indexed by the route's slot. Its forwarding images are indexed by
the local arena keys. A `ForwardingPlan<D>` says exactly what to do for one
published stream: deliver to these local participants, send one copy to these
remote shards, and use this reverse route for feedback. It does not contain a
track name or subscription pattern.

The generic `D` is the one detail that differs by payload:

```text
  video              D = DownstreamSlotKey
  audio              D = ()                    (the audio plan selects the slot)
  unreliable data    D = ChannelId
  reliable data      D = ChannelId
```

The data lanes still have separate stream-key arenas and separate plan images.
The type and the arena therefore say which lane a packet belongs to; there is
no second mutable `lane` flag that could disagree with its destination key.

The route table makes the last step just as explicit:

```text
  routes[route.slot] = RouteBinding {
    handle: RouteHandle,
    action: RouteAction,
  }

  RouteAction =
      Video       { local_track }
    | Audio       { track }
    | Unreliable  { stream }
    | Reliable    { stream }
    | Reverse     { target }
```

The route handle checks that the slot and epoch are still the expected
incarnation. The action already contains the shard-local key, so successful
resolution is the final lookup: there is no `TrackId` or topic map after it.
Mutable packet history, such as the last link sequence and loss counters, is
kept separately in shard-local route runtime state. The published view is the
decision; the runtime entry is only the accounting needed while executing it.

On the wire, the two route families are deliberately different types even
though they use the same packed layout:

```text
  PackedRoute:       [ shard: 12 bits | slot: 20 bits ]

  TransportHandle:   TransportRoute + epoch
                     client ICE / DTLS / SRTP state

  RouteHandle:       RouteId + epoch
                     inter-shard or inter-node endpoint state

  Envelope:          version | type | epoch | route | extension
```

The shard bits tell the classifier where to send the packet. The slot is a
dense local index, and the epoch identifies which occupant of that slot the
packet was meant for. A delayed packet for an old occupant therefore fails
closed instead of reaching a newly connected participant. Keeping
`TransportRoute` and `RouteId` as distinct types prevents a client transport
address from being accidentally used as an endpoint address.

The first packet is special because there is no established flow affinity yet.
The ICE ufrag carries the transport route. The packet may initially arrive at
an admitting shard selected by the socket hash; the route identifies the
transport owner that performs the WebRTC authentication. When authentication
succeeds, the owner reports it through the controller. The controller marks
the demux entry on the admitting shard and installs affinity for subsequent
packets. The authentication signal therefore returns to the shard that owns
the demux entry instead of being recorded only where authentication happened.

After that, the fast path is deliberately boring. The publisher's shard reads
the already-compiled plan. A local recipient is called directly. A remote
recipient gets a fixed envelope containing its route, so the same classifier
can steer it to the destination shard. That shard checks the route's epoch,
turns the route into a small local table index, and executes the selected
video, audio, unreliable-data, reliable-data or feedback action. Packets never
carry participant or track names, and the controller never compiles the
publisher as its own recipient.

When room state changes, only the control path gets complicated: the controller
recomputes affected plans, publishes a new generation, and sends the resulting
deltas to the shards. A packet racing an unapplied or retired route is dropped
instead of making the data path wait. Moving a destination creates a new route
rather than changing the shard encoded in a live one, so stale packets remain
harmless.

How a packet finds the code that owns it.

This document is the *model*: the invariants, the reasoning, and the wire
contract. It deliberately does not describe types, function names, or module
layout — those move, and a document that tracks them is wrong within a release.
Where you need the current shape, the code is named at the end.

---

## The problem

A shard owns its state and reaches other shards only by message
(`docs/thread-per-core.md`). So every packet has to arrive at exactly one
shard — the one holding the state for it — and the decision has to be cheap
enough to make in three places that cannot coordinate:

- **the kernel**, in an eBPF program with no allocation and bounded loops;
- **userspace**, when eBPF is unavailable or the kernel guessed wrong;
- **the simulator**, which must reach the same answer or it is testing fiction.

Three implementations of one decision is normally how you get three behaviours.
Avoiding that is what most of this design is for.

## The one idea

> **Routes are compiled addresses, not names.**

`ParticipantId`, `TrackId`, `RoomId`, `Topic` — the identities the application
reasons about — never appear on the wire in the packet path. What travels is a
fixed-width integer that says *where to deliver*, chosen so that reading it
requires no parsing, no hashing, and no table.

Names are for humans and the control plane. Addresses are for packets.

Everything below follows from that, plus one ownership rule:

> **A route's slot is allocated by the shard that will resolve it, and the
> address carries which shard that is.**

The slot indexes the destination's own table, so resolution is an array index.
The shard bits mean anyone holding the address knows where to send it without
consulting anything.

---

## Two routing domains

They share a representation and must not share a namespace.

**Client transport.** Identifies which shard owns a client's WebRTC transport
— its ICE, DTLS and SRTP state. One per connection.

**Endpoint routes.** Identify a destination *within* the node: a forwarded
track, an audio stream, a data-lane stream, a feedback path. Many per
connection.

They are separate allocator namespaces even though the packed layout is
identical, and separate types in code so one can never be passed where the
other is meant. Same bits, different meaning — a distinction the type system
should carry, not a comment.

---

## The address

```text
 31                    20 19                     0
+------------------------+------------------------+
| shard                  | slot                   |
| 12 bits                | 20 bits                |
+------------------------+------------------------+
```

4096 shards, ~1M slots each.

**Why the shard is in the address.** `SO_REUSEPORT` picks a receiving socket by
hashing the 4-tuple, which knows nothing about where a route lives. Landing on
the wrong shard is therefore *ordinary*, not an error, and happens constantly.
Redirecting has to be cheap: read 12 bits at a fixed offset. The alternative —
looking a name up in a shared map — reintroduces exactly the cross-shard shared
state the architecture exists to avoid, and does it on the hot path.

**Why 12 bits when nobody runs 4096 shards.** Protocol headroom. The wire
format should not have to change because the execution model grows a level or
the core count doubles. Today one shard field means one worker; nothing depends
on that staying true.

**Why 20 bits of slot.** Large enough that a table can be preallocated and
addressed densely, so resolving is an index rather than a lookup. It also has
to absorb slot *quarantine* (below) under a reconnect storm, which is what
actually sets the floor.

**Why one layout for both domains.** Because the kernel reads the shard field
without knowing which domain it is looking at. One layout means one extraction,
shared by the eBPF program, the userspace demuxer and the simulator. That
sharing is the point: it is the mechanism that keeps three implementations
honest, and it is worth more than the flexibility of two formats.

---

## Epochs: why a recycled slot is safe

Slots are reused. A datagram delayed in the network can arrive after its slot
was retired and handed to something else, and it would then be delivered to the
wrong destination — with a valid-looking address.

Two defences, in order of importance:

**The epoch.** Every route reference is `(address, epoch)`, and the destination
rejects a mismatch on arrival. The epoch increments each time a slot is reused,
so a stale packet fails the check rather than being misdelivered. This is the
real guard.

**Quarantine.** A retired slot waits before it can be reissued. This is the
second line, and it exists to make the epoch's guarantee trivially true rather
than merely probable: a slot cannot complete a full epoch wrap within any
plausible packet lifetime.

The quarantine window is a derived number, not a taste. Long quarantine is not
free: slot consumption under a reconnect storm is
`concurrent + installs_per_sec × quarantine`, so past a certain point it is the
*quarantine*, not the traffic, that exhausts the table. The chosen value keeps
three orders of magnitude of margin against a maximum segment lifetime while
costing far less of the working set than the intuitive answer would.

**A route never moves.** If a destination migrates to another shard, a new
route is minted and the old one retired. Rewriting a live route's shard field
would make in-flight packets undeliverable and the epoch meaningless.

---

## Who allocates, who owns

These are different jobs and belong to different components.

**The control plane allocates.** It sees every subscribe and unsubscribe,
already knows which shard each participant is on, and is the only thing
permitted to mint a route. So it chooses the destination, allocates the slot
and epoch, and publishes a delta that installs the compiled endpoint.

**The shard owns and executes.** It applies the delta on its next tick,
validates the epoch on every arrival, and holds the live state. It does not
allocate, and it does not decide who receives what.

The important consequence is what this **removes**. When a shard owned that
decision, it counted its own subscribers, concluded it needed a route, and —
being unable to mint one — had to *ask*. That request needed an id, a pending
map, and a completion handler, and every one of those was scaffolding around a
decision sitting on the wrong side of a boundary. Moving the decision deleted
the protocol.

Shards learn by **reading a published view**, not by receiving an answer.
Changes are staged against a generation and published atomically, so a shard
never observes half a decision.

**Deltas race packets, and that is allowed.** A packet naming a route whose
delta has not been applied is dropped, deliberately, rather than queued or
waited on. Blocking the data path on control-plane progress would couple them;
the drop is observable through counters instead.

---

## Getting in: two paths, chosen by whether it is STUN

**Bootstrap.** The first packet of a connection has no prior state to be
steered by, so it carries its own destination: the ICE ufrag encodes the
cluster, the node, the transport route, and its epoch. The classifier decodes
it and steers on that alone.

This is why the ufrag carries cluster and node while the inter-node envelope
does not — the ufrag is the one place with no context, and it is chosen by us,
so it is free to carry what is needed.

**Established.** Anything that is not STUN belongs to a flow that already
exists, and steering comes from flow affinity — a bounded cache keyed by
source address.

### The ufrag is a hint, not a credential

Written out in full because it reads like a hole and keeps being re-litigated.
Anyone can forge a ufrag: it is unauthenticated, it names a route directly, and
a bootstrap packet carrying one **does** put an entry in the address cache
before anything has been verified. Three properties make that harmless, and they
have to be read together.

**Admission grants nothing.** Steering decides which shard looks at a packet,
not whether the packet is honoured. The owning shard still resolves the route in
its published view, and str0m still runs ICE, DTLS and SRTP over it. A forged
ufrag buys parser work and a cache slot; it cannot deliver a byte, create a
participant, or reach anyone else's media.

**A flood cannot displace a live flow.** The cache **evicts rather than
refuses** — refusing when full would let an attacker lock out new legitimate
flows — and it evicts least-recently-used. Every cache *hit* refreshes the
entry's timestamp, so a participant sending media holds the most recently used
entry for its route while forged entries are touched once at admission. Eviction
picks the minimum, so a flood evicts its own oldest entry. It churns its own
ring. A live call keeps its place precisely because it is live.

**Authenticated entries are protected after an owner acknowledgment.**
`SO_REUSEPORT` still picks the *admitting* shard by hashing the 4-tuple, while
the shard that authenticates is the route's *owner*. The owner reports the
original source shard through the controller; the controller installs eBPF
flow affinity to the owner and sends the source shard an explicit
authentication command. Until that command arrives, the entry remains usable
for the handshake but is eligible for eviction. Once marked, it is preferred
over unauthenticated entries during both per-route and global eviction.

The `# Security hardening` comment on the demuxer states the same argument
beside the code that implements it. If these two ever disagree, the code is
right and one of the documents has a bug.

**NAT rebinding needs no special case.** A changed tuple produces fresh ICE
connectivity checks, which are STUN, which carry the ufrag, which re-derives
the route. Recovery is the bootstrap path, not a mechanism of its own.

**ICE-TCP transfers ownership once.** The same ufrag identifies the owning
shard, but a TCP connection is handed over whole rather than steered per
packet. Different mechanics, same ownership rule: *the transport route names
the shard that owns the transport.*

---

## Getting across: the envelope

Inter-node and cross-shard traffic is prefixed with a fixed 16-byte header:

```text
version (1) | type (1) | epoch (2) | route (4) | extension (8)
```

Fixed size and fixed offsets are the whole design. The route sits at a constant
offset so a steering program can read it with a bounds-checked load and no
parsing — which is what makes the same extraction usable from eBPF, userspace
and the simulator.

- **version** gates format evolution.
- **type** selects the payload family, which determines how the extension is
  interpreted. The set is small and lives in one place in code; treat that as
  the registry.
- **route** is the address above.
- **extension** is 64 bits of payload-specific metadata — for media, the link
  sequence and a compact playout timestamp.

**Why the envelope has no node or cluster id.** By the time a packet is in this
format it has already been delivered to the right node; carrying that again
would be bytes on every packet to restate something already decided. The ufrag
carries them precisely because bootstrap has not made that decision yet.

**Why the extension is fixed-width.** A length-prefixed or optional field means
a parse, a bounds check, and a branch on the steering path. Eight bytes that
are sometimes unused is cheaper than a variable header, and it keeps the offset
of everything before it constant.

**Why there is no flags field and no universal sequence number.** Both would be
speculative. Flags accrete meanings that belong in `type`; a universal sequence
number imposes a sequencing model on payload families that do not want one.
Sequencing lives in the extension for the families that need it.

---

## Arriving: what a route resolves to

This is the half that justifies the rest. Resolution yields **an action
carrying a dense key**, not a name to be looked up:

- forwarded video, naming the destination's own fanout handle;
- audio, naming the destination's compiled audio plan;
- a realtime data stream, and separately a reliable one;
- a reverse path, for feedback travelling back toward a publisher.

The keys are indexes into the destination's own tables. Dispatch uses them
directly. Had the route carried a `TrackId`, resolution would end in a hash
lookup on every packet — which is the cost the address model exists to avoid,
reintroduced at the last step.

**The two data lanes are distinct actions, not one action with a lane field.**
They resolve through different arenas, so the variant *is* the lane; a lane
field would be a second source of truth that could disagree with the key.

**The reverse path is shared, not per-sender.** One reverse route exists per
published stream, used by every subscribing shard, unlike media routes which
are per destination. Traffic on it is idempotent requests — a sender repeats
them if it still needs them — so there is no per-link bookkeeping that a
per-sender route would protect. And the arithmetic matters: `streams × shards`
would make the reverse path the largest consumer of a 32-bit address space, to
buy nothing.

---

## What is deliberately excluded

- **Stable identities in the packet path.** No `ParticipantId`, `TrackId`,
  `RoomId` or `Topic` on the wire. They are variable-width, meaningful, and
  would require a lookup.
- **A per-route kernel map.** Steering derives the shard arithmetically from
  the packet. A map would need updating on every route change, from the control
  plane, in lockstep with the data path.
- **Client ingress through the shard mesh.** Packets are steered before
  userspace. Forwarding client UDP between shards over the mailbox mesh would
  make the mesh a data path.
- **Virtual shards.** One shard field means one worker. The headroom exists if
  that changes; the indirection does not exist until it is needed.
- **Blocking on control-plane progress.** See the delta race above.

---

## Invariants

A change that breaks one of these is a protocol break, not a refactor.

1. The address encodes the shard that owns the destination.
2. A slot is allocated by the shard that resolves it.
3. Steering derives the shard from the packet alone — no lookup, no allocation,
   bounded work.
4. One packed layout, one extraction, shared by kernel, userspace and simulator.
5. Transport and endpoint routes never share an allocator namespace.
6. Every route reference carries an epoch, and the destination validates it.
7. A retired slot is quarantined before reuse.
8. A route never changes shard; migration mints a new one.
9. Stable application identities never appear in the packet path.
10. The control plane allocates; the owning shard installs, validates and
    executes.
11. Resolution yields a dense key, not a name.
12. Every inter-node message uses the same fixed-size envelope with the route
    at a constant offset.

---

## The shape, end to end

```text
  names                          addresses
  ─────                          ─────────
  ParticipantId  ─┐
  TrackId         ├── control plane ──► allocate (shard, slot, epoch)
  RoomId / Topic ─┘        │                        │
                           │                        ├─► transport route ──► ICE ufrag
                           │                        └─► endpoint route  ──► envelope
                           │                                                   │
                           ▼                                                   ▼
                    published view                                     steering: read
                    (staged, atomic)                                   shard from packet
                           │                                                   │
                           └──────────────► owning shard ◄─────────────────────┘
                                                 │
                                      validate epoch, index slot
                                                 │
                                                 ▼
                                      action + dense key ──► dispatch
```

---

## Where this lives

Named so you can find it, not so this document tracks it:

- the packed layout, epoch and quarantine rules, and the envelope encoder are
  in the routing crate shared with the eBPF program — that crate is the
  authority for anything on the wire;
- the classifier that decides bootstrap-vs-established is there too, and is
  called by the kernel program, the userspace demuxer, and the simulator's
  steering adapter;
- route allocation, the lifecycle transaction and the published view belong to
  the control plane;
- resolution, epoch validation and dispatch belong to the shard.

If this document and the code disagree about a number, the code is right and
this file has a bug — but if they disagree about an *invariant*, stop, because
one of them is a defect.
