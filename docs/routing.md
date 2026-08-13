# PulseBeam Routing Protocol Architecture

## Status

This document defines the routing architecture for PulseBeam's distributed WebRTC SFU.

It focuses specifically on:

- the routing key model,
- ICE ufrag routing,
- inter-node routing,
- eBPF / `SO_REUSEPORT` steering,
- route allocation and stale-route protection,
- and the fixed inter-node Envelope format.

The central idea is:

> **PulseBeam routes packets using compact compiled addresses, not stable application identities.**

The routing key is designed to be cheap enough for both userspace and eBPF to interpret directly.

---

# 1. Core routing model

PulseBeam has two distinct routing domains:

1. **Client transport routing**
2. **Distributed data-plane endpoint routing**

They use different semantic key types, but they share the same packed physical routing primitive.

```text
                         PackedRoute(u32)
                         ShardId | Slot
                         /          \
                        /            \
                       ▼              ▼
             TransportRoute         RouteId
             client transport    distributed endpoint
```

The important distinction is semantic:

- `TransportRoute` routes a WebRTC transport / ICE association.
- `RouteId` routes a distributed endpoint such as a media stream or SCTP endpoint.

They may share the same 32-bit wire layout without being interchangeable types.

---

# 2. Shard ownership model

Today, `ShardId` maps directly to the physical shard worker.

```text
ShardId 0 → Worker 0
ShardId 1 → Worker 1
ShardId 2 → Worker 2
...
```

A shard is the unit of:

- WebRTC state ownership,
- `str0m` execution,
- RTP/RTCP processing,
- SCTP/DataChannel processing,
- timers,
- pacing,
- UDP socket ownership,
- and established ICE-TCP connection ownership.

The kernel steers packets to the owning shard before shard-local userspace
state is consulted. Client UDP packets are not forwarded between shards
through the in-process mailbox mesh.

The route format encodes the `ShardId` directly so packet steering can identify the owning worker without a userspace lookup.

---

# 3. Why encode ShardId in the route

The route must be usable by the network steering layer before shard-local userspace state is consulted.

For UDP, PulseBeam wants the kernel to steer a packet directly to the worker that owns the destination.

Instead of:

```text
RouteId
   ↓
BPF map:
RouteId → ShardId
   ↓
socket
```

PulseBeam uses:

```text
RouteId
   ↓
extract ShardId
   ↓
reuseport socket
```

The route therefore acts as a compact compiled execution address.

This avoids maintaining a dynamic eBPF routing map for every individual route.

It also removes the ingress ownership race. The control plane allocates and
installs the transport route before returning ICE credentials. eBPF uses the
encoded shard to deliver the first STUN packet directly to the owning socket;
no receiving shard needs to discover the owner and enqueue the packet to a
different shard.

---

# 4. Packed route layout

The recommended packed route is:

```text
Route: u32

31                    20 19                    0
+-----------------------+-----------------------+
| ShardId               | Slot                  |
| 12 bits               | 20 bits               |
+-----------------------+-----------------------+
```

This provides:

```text
4096 shard IDs
1,048,576 slots per shard
```

The packed value can be decoded with:

```rust
const SLOT_BITS: u32 = 20;
const SLOT_MASK: u32 = (1 << SLOT_BITS) - 1;

shard = route >> SLOT_BITS;
slot = route & SLOT_MASK;
```

The exact constants should be centralized in one routing primitive.

---

# 5. Why keep 12 bits for ShardId

PulseBeam does not currently need anywhere near 4096 physical shards.

The current execution model is expected to use a much smaller number of workers, typically related to the available CPU cores.

The 12-bit allocation is retained deliberately as protocol headroom.

The wire format should not need to change merely because future PulseBeam versions use a larger shard namespace or introduce a different execution topology.

Today, however, the semantics are simple:

> **`ShardId` identifies the actual shard worker that owns the route.**

No virtual-shard abstraction is required by this specification.

---

# 6. Common packed primitive

The packed representation can be shared internally:

```rust
pub struct PackedRoute(u32);
```

Conceptually:

```rust
impl PackedRoute {
    pub fn shard(self) -> ShardId;
    pub fn slot(self) -> u32;
}
```

Semantic wrappers should remain distinct:

```rust
pub struct TransportRoute(PackedRoute);
pub struct RouteId(PackedRoute);
```

This gives both benefits:

- eBPF sees one consistent bit layout,
- Rust does not let transport and stream routes get confused accidentally.

---

# 7. ShardId

Recommended semantic type:

```rust
pub struct ShardId(u16);
```

Only 12 bits are valid on the wire.

Today:

```text
ShardId == physical shard worker
```

The routing protocol does not define any additional abstraction above that.

---

# 8. Route epoch

Every packed route is qualified by an epoch.

The safe route handle is:

```text
(epoch, route)
```

not the 32-bit route alone.

For example:

```text
old route:
    epoch = 41
    route = shard 7 | slot 100

destroy route

new route:
    epoch = 42
    route = shard 7 | slot 100
```

A delayed packet containing:

```text
epoch = 41
route = same packed value
```

must be rejected as stale.

This allows dense slot reuse.

---

# 9. Two semantic route types

## `TransportRoute`

Purpose:

> Route a client WebRTC transport to the shard that owns its `str0m` / PeerConnection state.

Lifetime:

```text
approximately one ICE / WebRTC transport incarnation
```

Wire use:

```text
ICE ufrag
```

The packed slot refers to transport/participant-side state.

---

## `RouteId`

Purpose:

> Route a distributed PulseBeam data-plane message to a typed endpoint.

Lifetime:

```text
approximately one routed endpoint incarnation
```

Wire use:

```text
inter-node Envelope
```

For media, a route is typically per stream/track.

For SCTP, the route may target participant-level SCTP state.

---

# 10. Stable identity is separate

The routing protocol does not encode:

```text
RoomId
ParticipantId
TrackId
```

Those are control-plane identities.

Example:

```text
TrackId("alice-camera")
      ↓ control-plane compilation
RouteHandle {
    epoch: 9,
    route: Shard 17 | Slot 45
}
```

The route says where the endpoint is now.

The `TrackId` says what the endpoint logically represents.

---

# 11. ICE ufrag routing

The ICE ufrag bootstraps client transport routing.

A STUN Binding Request contains the negotiated ICE username.

PulseBeam encodes the physical transport route directly into the server ufrag.

This allows the packet path to derive:

```text
cluster
node
ShardId
transport slot
epoch
```

without looking up a `ParticipantId`.

---

# 12. ICE ufrag wire layout

The ufrag contains exactly 80 bits / 10 raw bytes.

It encodes to exactly 16 Crockford Base32 characters.

```text
80 bits / 5 = 16 characters
```

Wire layout:

```text
 byte 0        byte 1       bytes 2-3       bytes 4-7          bytes 8-9
┌────────────┬────────────┬───────────────┬──────────────────┬────────────┐
│ ver(4)     │ cluster_lo │ node_id       │ transport_route  │ epoch      │
│ clust_hi(4)│            │               │                  │            │
└────────────┴────────────┴───────────────┴──────────────────┴────────────┘
```

Bit allocation:

```text
version             4 bits
cluster_id         12 bits
node_id            16 bits
transport_route    32 bits
epoch              16 bits
--------------------------
total              80 bits
```

---

# 13. Ufrag field semantics

## version

```text
4 bits
```

Identifies the ufrag layout version.

A version bump is only needed when the 80-bit layout changes incompatibly.

---

## cluster_id

```text
12 bits
0..4095
```

Identifies the PulseBeam cluster.

Single-cluster deployments can use:

```text
cluster_id = 0
```

---

## node_id

```text
16 bits
0..65535
```

Identifies the destination node within the cluster.

Single-node deployments can use:

```text
node_id = 0
```

---

## transport_route

```text
32 bits
```

Packed as:

```text
ShardId(12) | transport slot(20)
```

This tells the destination node which shard owns the transport.

---

## epoch

```text
16 bits
```

Qualifies the transport route incarnation.

The pair:

```text
(epoch, TransportRoute)
```

is the valid transport address.

---

# 14. Recommended ufrag Rust type

```rust
pub struct IceUfrag {
    pub cluster_id: u16,
    pub node_id: u16,
    pub transport: TransportRoute,
    pub epoch: u16,
}
```

This avoids conflating the transport route with the inter-node `RouteId`.

---

# 15. ICE ufrag encoding

The 10-byte raw representation is:

```text
raw[0]:
    high nibble = version
    low nibble  = cluster_id[11:8]

raw[1]:
    cluster_id[7:0]

raw[2..4]:
    node_id big endian

raw[4..8]:
    packed TransportRoute big endian

raw[8..10]:
    epoch big endian
```

Then:

```text
raw bytes
   ↓
Crockford Base32
   ↓
16-character ICE ufrag
```

---

# 16. Client UDP routing with eBPF

The eBPF routing model has two phases.

## Bootstrap traffic

Initial STUN carries the ufrag.

Conceptually:

```text
UDP packet
   ↓
SK_REUSEPORT eBPF
   ↓
is STUN?
   ↓
find USERNAME attribute
   ↓
extract PulseBeam ufrag
   ↓
decode transport_route
   ↓
ShardId
   ↓
select shard's SO_REUSEPORT socket
   ↓
shard worker
```

This allows the first transport packet to reach the correct owner.

There is no `Ingress` packet message in the shard mailbox protocol. A client
UDP packet that reaches the node is steered to the socket owned by the route's
shard before PulseBeam userspace receives it.

---

# 17. Established UDP flow routing

DTLS/SRTP/RTP packets do not carry the ICE ufrag.

Therefore bootstrap parsing alone is insufficient.

Once STUN establishes the transport flow, eBPF can maintain a small flow-affinity table:

```text
5-tuple / connection flow
      ↓
ShardId
```

Then:

```text
non-STUN UDP
     ↓
flow lookup
     ↓
shard socket
```

This state is fundamentally different from a full route table.

The kernel does **not** need:

```text
RouteId → ShardId
```

for every track/endpoint.

It needs only the flow affinity required for client transport delivery.

The flow-affinity table is kernel steering state. It does not replace the
transport route, route epoch, or shard-local transport table.

---

# 18. NAT rebinding

If the client's tuple changes, ICE connectivity checks provide the bootstrap information again.

Conceptually:

```text
new tuple
   ↓
STUN with ufrag
   ↓
decode TransportRoute
   ↓
recover ShardId
   ↓
install/update flow affinity
```

The routing metadata therefore naturally participates in rebinding recovery.

---

# 19. ICE-TCP

TCP should not be forced through the same packet-level steering model.

The ICE routing key still applies, but transport ownership is transferred once.

```text
TCP accept
   ↓
read initial RFC4571-framed STUN
   ↓
extract/decode IceUfrag
   ↓
TransportRoute
   ↓
ShardId
   ↓
handoff TCP connection
   ↓
shard owns it permanently
```

After handoff, no repeated routing lookup is necessary.

ICE-TCP may use one reliable control handoff carrying the accepted connection
to the owning shard. It does not require forwarding individual UDP packet
batches through the shard mesh.

---

# 20. Why ICE-TCP remains consistent with the model

UDP and TCP use different mechanics:

```text
UDP:
    packet steering

TCP:
    one-time connection ownership transfer
```

but share the same ownership rule:

> **The transport route identifies the shard that owns the WebRTC transport.**

---

# 21. Inter-node protocol

Inter-node traffic is raw UDP with a fixed PulseBeam Envelope.

Every inter-node message uses the same framing protocol.

There are not separate framing protocols for:

```text
media
feedback
SCTP
telemetry
control
```

Instead, the Envelope has a first-class `type`.

---

# 22. Fixed inter-node Envelope

The common Envelope header is exactly 16 bytes.

Wire layout:

```text
0               1               2                               4
+---------------+---------------+-------------------------------+
| version       | type          | epoch                         |
+---------------+---------------+-------------------------------+
| route                                                         |
+---------------------------------------------------------------+
| extension                                                     |
|                                                               |
+---------------------------------------------------------------+

                              16 bytes
```

Field sizes:

```text
version       u8      1 byte
type          u8      1 byte
epoch         u16     2 bytes
route         u32     4 bytes
extension     u64     8 bytes
----------------------------
total                 16 bytes
```

---

# 23. Envelope wire struct

Conceptually:

```rust
#[repr(C)]
struct EnvelopeWire {
    version: u8,
    kind: u8,
    epoch: zerocopy::big_endian::U16,
    route: zerocopy::big_endian::U32,
    extension: zerocopy::big_endian::U64,
}
```

The fixed size should be compiler-checked:

```rust
const _: () = assert!(size_of::<EnvelopeWire>() == 16);
```

---

# 24. Envelope version

`version` describes the base Envelope framing.

A version change is for incompatible changes to:

- the 16-byte layout,
- field widths,
- route semantics,
- or extension interpretation rules.

Adding a new message type should not require a version bump.

---

# 25. Envelope type

`type` identifies the payload family.

Conceptual registry:

```text
Media
Feedback
Sctp
Telemetry
Control
...
```

The exact numeric values should be centralized in the protocol specification.

The semantic split is:

```text
type
    = what is this message?

route
    = where does it go?

extension
    = compact per-message metadata
```

---

# 26. Envelope route

The `route` field is a packed `RouteId`.

Layout:

```text
ShardId(12) | endpoint slot(20)
```

This means eBPF can determine the destination shard directly from the fixed-offset Envelope header.

No userspace route-table lookup is required merely to decide which shard socket should receive the UDP datagram.

---

# 27. Inter-node UDP eBPF steering

The inter-node path is particularly simple because PulseBeam controls the packet format.

```text
remote node
    ↓
UDP
    ↓
SO_REUSEPORT
    ↓
SK_REUSEPORT eBPF
    ↓
read Envelope.route at fixed offset
    ↓
shard = route >> 20
    ↓
select shard reuseport socket
    ↓
shard worker
```

This is the primary reason the shard belongs in the packed route.

The datagram is delivered directly to the selected shard socket. A client
ingress-style mailbox message is not a fallback when the kernel has already
selected the owner.

---

# 28. Why not use an eBPF map per RouteId

An opaque route would require:

```text
RouteId
   ↓
BPF map:
RouteId → ShardId
```

Every route creation, deletion, and reuse would then require synchronized kernel routing state.

With the packed layout, eBPF needs no per-route mapping.

The wire route itself already says which shard owns the destination.

---

# 29. Inter-node shard-local lookup

After eBPF selects the shard, userspace decodes the same route:

```text
RouteId
   ↓
ShardId + Slot
```

The shard already knows the route belongs to it.

Therefore the hot-path lookup becomes approximately:

```text
slot
  ↓
endpoint table
```

The shard does not need another global `RouteId → shard` lookup.

---

# 30. Shard-local storage

Conceptually:

```text
Shard 3
 ├── transport slots[]
 └── endpoint slots[]
```

A route:

```text
Shard 3 | Slot 17
```

resolves to:

```text
shard 3
   ↓
endpoint_slots[17]
```

The exact storage implementation can be:

- a dense generational table,
- a custom externally addressed slot table,
- or another shard-local layout.

The wire protocol does not depend on the specific Rust container.

---

# 31. Route allocation

A route allocator chooses:

```text
ShardId
Slot
Epoch
```

The shard is already known from control-plane placement.

Conceptually:

```text
new track
   ↓
owning ShardId already known
   ↓
allocate slot
   ↓
allocate/current epoch
   ↓
RouteHandle {
    epoch,
    route = shard | slot
}
```

The control plane is the route allocator. It chooses the destination shard,
allocates a slot and epoch in that shard's namespace, and asks the owning
shard to install the compiled endpoint. The route handle is not published to
a sender until installation is acknowledged.

The owning shard remains the authority for live endpoint state and validates
the route epoch on receipt. Allocation authority and endpoint state ownership
are separate:

```text
control plane:
    choose shard
    allocate (route, epoch)
    request installation
    publish only after acknowledgement

owning shard:
    install endpoint at slot
    process packets
    retire endpoint on command
```

Transport routes and distributed endpoint routes use separate allocator
namespaces even though they share the packed representation.

---

# 32. RouteHandle

A route reference is:

```rust
pub struct RouteHandle {
    pub epoch: u16,
    pub route: RouteId,
}
```

The Envelope transmits these fields directly.

For client transport routing, the equivalent is:

```text
TransportHandle {
    epoch,
    transport_route
}
```

The two handles share the same safety model.

---

# 33. Per-track RouteId

Media routes should generally be per routed stream/track.

Example:

```text
Alice
 ├─ microphone → RouteId A
 ├─ camera     → RouteId B
 └─ screen     → RouteId C
```

This matches the SFU data plane because each stream can have independent:

- fanout,
- subscribers,
- inter-node forwarding,
- feedback,
- recording,
- lifecycle,
- and policy.

---

# 34. RouteId is not limited to RTP

`RouteId` should be defined broadly:

> **A packed address for a typed distributed data-plane endpoint on a shard.**

Examples:

```text
Media endpoint
SCTP endpoint
Feedback endpoint
Recording endpoint
Telemetry endpoint
```

The Envelope `type` determines how the payload should be interpreted.

The route determines where it should go.

---

# 35. Envelope extension

The final 64 bits are:

```text
extension: u64
```

This field exists to provide fixed-cost extensibility without dynamic TLV parsing.

There is:

- no dynamic header length,
- no extension length,
- no speculative reserved bytes,
- no universal flags field,
- no universal sequence field.

The header always remains 16 bytes.

---

# 36. Extension interpretation

The extension can be interpreted as:

```text
tag + inline value
```

or by the Envelope type itself.

For example:

```text
Media:
    extension = link/timing metadata

SCTP:
    extension = SCTP-specific metadata or zero

Feedback:
    extension = request metadata
```

The exact tag/value partition should be chosen only once real extension requirements are enumerated.

---

# 37. Media extension example

The current media metadata is:

```text
link_seq       u32
playout_ntp32  u32
```

Together:

```text
64 bits
```

So the media Envelope can naturally use:

```text
extension[63:32] = link_seq
extension[31:0]  = playout_ntp32
```

Wire:

```text
+---------------+---------------+-------------------------------+
| version       | Media         | epoch                         |
+---------------+---------------+-------------------------------+
| RouteId                                                       |
+---------------------------------------------------------------+
| link_seq                      | playout_ntp32                  |
+---------------------------------------------------------------+
| media payload ...                                            |
+---------------------------------------------------------------+
```

This preserves the current 16-byte media overhead while using one unified protocol.

---

# 38. Feedback example

A reverse request can use:

```text
type = Feedback
route = publisher-side RouteId
extension = request metadata or zero
payload = typed feedback body
```

There is no separate `RouteEnvelope`.

The route itself does not need a direction bit.

The control plane provides the appropriate destination route handle.

---

# 39. SCTP example

SCTP/DataChannel forwarding uses the same framing:

```text
Envelope {
    type = Sctp,
    epoch,
    route,
    extension,
}
+ SCTP/data-channel payload
```

The route may point to participant-level SCTP state rather than a media stream slot.

The common packed route layout still applies.

---

# 40. Inter-node packet receive path

The complete receive path is:

```text
UDP datagram
   ↓
eBPF reads fixed Envelope.route
   ↓
extract ShardId
   ↓
select shard socket
   ↓
shard receives datagram
   ↓
parse 16-byte Envelope
   ↓
validate version
   ↓
validate epoch
   ↓
extract slot
   ↓
lookup endpoint
   ↓
dispatch by Envelope.type
```

The important point is that **kernel steering and userspace endpoint lookup use the same packed route**.

There is no intermediate `route.shard()` forwarding step in userspace. The
shard selected by eBPF is already the route owner. Userspace resolves only the
slot and validates the epoch.

---

# 41. Ufrag and Envelope symmetry

The routing architecture deliberately uses the same packed addressing concept in both ingress protocols.

## Client transport

```text
IceUfrag
    ↓
TransportRoute
    ↓
ShardId | transport slot
    ↓
shard
```

## Inter-node

```text
Envelope
    ↓
RouteId
    ↓
ShardId | endpoint slot
    ↓
shard
```

Same primitive.

Different semantic address spaces.

---

# 42. Why keep separate Rust types

Even though both are:

```text
ShardId | Slot
```

they route different objects.

Using the same Rust type would allow invalid operations such as:

```text
using a media RouteId as an ICE transport route
```

Therefore:

```rust
struct TransportRoute(PackedRoute);
struct RouteId(PackedRoute);
```

is preferable to:

```rust
type TransportRoute = RouteId;
```

---

# 43. Node and cluster routing

The packed route only handles routing **inside a node**.

The complete physical route is layered.

For client ICE:

```text
ClusterId
NodeId
TransportRoute
Epoch
```

For inter-node:

```text
network destination / peer node
RouteId
Epoch
```

The Envelope does not need to repeat `NodeId`, because the UDP destination already selects the target node.

---

# 44. Why Envelope has no node_id

Inter-node links already know the destination node.

Adding `node_id` to every packet would duplicate information already represented by the network destination.

Therefore:

```text
IP/UDP destination
    = node selection

Envelope.route
    = shard + endpoint selection
```

---

# 45. Why Envelope has no cluster_id

Inter-node links operate inside a known cluster/security domain.

Cluster identity belongs to:

- discovery,
- connection setup,
- configuration,
- or outer infrastructure.

It does not need to be repeated in every hot-path packet.

---

# 46. Why ufrag does include node_id and cluster_id

The ICE ufrag is bootstrap metadata.

The initial client packet may arrive at routing infrastructure that still needs to determine the correct PulseBeam placement.

Therefore the ufrag carries more physical information:

```text
cluster
node
transport route
epoch
```

The Envelope is already inside the cluster data plane and can therefore be smaller.

---

# 47. eBPF as part of the routing architecture

eBPF should be treated as a first-class design constraint, not merely a future optimization.

The packed route exists partly so the kernel can steer traffic efficiently.

The protocol therefore intentionally exposes:

```text
ShardId
```

in a cheap-to-decode position.

This applies to:

- client UDP bootstrap via encoded transport route,
- established client UDP flow affinity,
- and especially inter-node UDP, where the route is directly visible at a fixed Envelope offset.

---

# 48. eBPF routing state

The desired kernel routing state is small.

## Inter-node

No per-route map is required.

```text
RouteId
   ↓
extract ShardId
   ↓
select socket
```

## Client UDP

A flow-affinity map is useful after ICE bootstrap:

```text
flow tuple → ShardId
```

The kernel does not need to mirror every PulseBeam endpoint route.

---

# 49. Route placement

The shard can be chosen using placement logic such as:

```text
load
room affinity
participant affinity
rendezvous hashing
power-of-two choices
```

The specific placement algorithm is not encoded into the protocol.

Once chosen:

```text
ShardId
    becomes part of the packed route
```

and remains stable for that route incarnation.

If an endpoint moves to another shard, it receives a new route.

---

# 50. Example: media track

Suppose:

```text
Alice camera
ShardId = 17
Slot = 42
Epoch = 9
```

Then:

```text
RouteId =
    (17 << 20) | 42
```

Inter-node Envelope:

```text
version   = 0
type      = Media
epoch     = 9
route     = packed RouteId
extension = media metadata
```

eBPF extracts:

```text
ShardId 17
```

and selects shard 17's socket.

The shard extracts:

```text
slot 42
```

and reaches the track endpoint.

---

# 51. Example: transport routing

Suppose Alice's PeerConnection uses:

```text
ShardId = 17
transport slot = 12
epoch = 3
```

Then the ufrag contains:

```text
cluster_id
node_id
TransportRoute(Shard 17 | Slot 12)
epoch 3
```

Initial STUN can therefore be steered directly to shard 17.

---

# 52. Transport-route vs media-route slot spaces

The slot value should not be assumed to refer to one universal node table.

For example:

```text
TransportRoute:
    ShardId + transport-slot namespace

RouteId:
    ShardId + endpoint-slot namespace
```

The semantic wrapper determines which table the slot addresses.

This prevents accidental cross-domain interpretation.

---

# 53. Stale-route handling

Every receive path validates epoch before using the local slot.

Conceptually:

```text
decode route
   ↓
extract ShardId
   ↓
reach shard
   ↓
extract slot
   ↓
local slot entry
   ↓
compare epoch
```

If:

```text
packet_epoch != slot_epoch
```

the packet is stale and must be dropped.

---

# 54. Route reuse

A slot may be reused after teardown.

Example:

```text
Shard 17
Slot 42
Epoch 9
```

is destroyed.

Later:

```text
Shard 17
Slot 42
Epoch 10
```

is created.

An old packet with epoch 9 cannot reach the new endpoint.

---

# 55. Envelope compatibility model

The inter-node protocol evolves through:

```text
version
type
extension
```

Each serves a different purpose.

## version

Base framing compatibility.

## type

Payload family.

## extension

Compact per-message semantics.

This avoids changing the base header for ordinary feature evolution.

---

# 56. No flags field

The Envelope should not contain a speculative flags bitmap.

There is no currently required universal flag semantic.

The type and extension namespaces already provide structured extensibility.

A flags field can be added only in a future incompatible header revision if a real need appears.

---

# 57. No dynamic extension length

The extension is always:

```text
u64
```

The payload always begins at:

```text
byte offset 16
```

There is no need for:

```text
extension_len
header_len
TLV traversal
```

This keeps both userspace and eBPF parsing simple.

---

# 58. No universal sequence field

A sequence number is not useful for every payload type.

Media may need link sequencing.

SCTP already has sequencing semantics.

Control requests may use different retry/request semantics.

Therefore sequence data belongs in type-specific extension semantics, not the common header.

---

# 59. Wire-format summary

## Packed route

```text
32 bits

31                    20 19                    0
+-----------------------+-----------------------+
| ShardId               | Slot                  |
| 12 bits               | 20 bits               |
+-----------------------+-----------------------+
```

---

## ICE ufrag

```text
80 bits raw
16 Crockford Base32 characters

┌───────────┬────────────┬────────────┬────────────────────┬───────────┐
│ version   │ cluster    │ node       │ TransportRoute     │ epoch     │
│ 4 bits    │ 12 bits    │ 16 bits    │ 32 bits            │ 16 bits   │
└───────────┴────────────┴────────────┴────────────────────┴───────────┘
```

---

## Inter-node Envelope

```text
16-byte fixed header

┌────────────┬────────────┬────────────┬────────────────────┬────────────────────┐
│ version    │ type       │ epoch      │ RouteId            │ extension          │
│ 8 bits     │ 8 bits     │ 16 bits    │ 32 bits            │ 64 bits            │
└────────────┴────────────┴────────────┴────────────────────┴────────────────────┘
```

---

# 60. Routing pipeline summary

## Client UDP

```text
STUN
  ↓
ufrag
  ↓
TransportRoute
  ↓
ShardId
  ↓
eBPF selects shard socket
  ↓
transport slot
```

No client UDP packet is wrapped in a shard-to-shard mailbox message. The shard
mesh carries control-plane commands and explicitly message-based coordination,
not a second ingress path for client datagrams.

Established flow:

```text
5-tuple
  ↓
eBPF flow-affinity map
  ↓
ShardId
```

---

## ICE-TCP

```text
accept
  ↓
initial STUN
  ↓
ufrag
  ↓
TransportRoute
  ↓
ShardId
  ↓
one-time connection handoff
```

---

## Inter-node UDP

```text
Envelope.route
  ↓
ShardId
  ↓
eBPF selects shard socket
  ↓
slot
  ↓
typed endpoint
```

The datagram is delivered directly to the selected shard socket. A client
ingress-style `Ingress` mailbox message is not part of this path.

---

# 61. Terminology

## `PackedRoute`

The common 32-bit physical routing representation:

```text
ShardId | Slot
```

---

## `ShardId`

The actual PulseBeam shard/worker identifier.

Encoded in the route.

Currently maps 1:1 to the shard worker.

---

## `TransportRoute`

Typed wrapper around `PackedRoute`.

Routes client WebRTC transports.

Used in the ICE ufrag.

---

## `RouteId`

Typed wrapper around `PackedRoute`.

Routes distributed data-plane endpoints.

Used in the inter-node Envelope.

---

## `RouteHandle`

```text
epoch + RouteId
```

Generation-safe distributed route.

---

## `IceUfrag`

Bootstrap-routing token:

```text
cluster + node + TransportRoute + epoch
```

---

## `Envelope`

Single inter-node packet framing protocol:

```text
version + type + epoch + RouteId + extension
```

---

# 62. Final design principles

The routing architecture should follow these rules.

1. **The route encodes the actual `ShardId` that owns the destination.**

2. **eBPF should derive `ShardId` directly from the packet.**

3. **No per-route eBPF map should be required for inter-node routing.**

4. **Client transport routing and distributed endpoint routing share the same packed representation but use distinct semantic types.**

5. **The ICE ufrag carries physical bootstrap information: cluster, node, transport route, and epoch.**

6. **Every inter-node message uses the same fixed 16-byte Envelope.**

7. **The Envelope route is `ShardId(12) | Slot(20)`.**

8. **The Envelope `type` determines payload semantics.**

9. **The 64-bit extension field handles compact type-specific metadata.**

10. **Epoch protects route reuse.**

11. **The 12-bit shard field is protocol headroom; it does not imply 4096 current worker threads.**

12. **If a route moves to another shard, a new route is minted.**

13. **Stable identities such as `ParticipantId` and `TrackId` never appear in the packet hot-path routing format.**

14. **The control plane allocates route handles; the owning shard installs and executes them.**

15. **eBPF performs packet steering before userspace; the shard mailbox mesh does not carry client UDP ingress packets.**

---

# 63. Final conceptual diagram

```text
                         CONTROL / PLACEMENT

                    ParticipantId / TrackId / RoomId
                               │
                               │ compile placement
                               ▼
                            ShardId
                               │
                  ┌────────────┴────────────┐
                  │                         │
                  ▼                         ▼

          TransportRoute                  RouteId
             Shard | Slot              Shard | Slot
                  │                         │
                  ▼                         ▼
              ICE ufrag                 Envelope
                  │                         │
                  └────────────┬────────────┘
                               ▼
                              eBPF
                               │
                         extract ShardId
                               │
                               ▼
                         shard worker/socket
                               │
                               ▼
                         shard-owned state
```

The key architectural statement is:

> **PulseBeam's wire routes are compiled shard-local addresses. The shard bits let eBPF steer packets directly to the owning worker, while the slot bits let that worker resolve the destination without a global lookup.**

The 12-bit `ShardId` allocation is retained as future protocol headroom, but the current architecture remains deliberately simple: one `ShardId` maps directly to one shard worker.
