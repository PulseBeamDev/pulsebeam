# Naming boundaries

PulseBeam names values by who owns them and how long they remain valid. A
suffix is a boundary contract, not a description of the container holding the
value.

| Suffix | Owner and lifetime | Meaning |
| --- | --- | --- |
| `Id` | Application for room and participant names; PulseBeam for runtime IDs | Canonical entity identity, interpreted in its declared scope. |
| `Address` | PulseBeam control plane | Routable placement information that may cross nodes. |
| `Handle` | One runtime shard | Ephemeral incarnation valid only in its owning shard. |
| `Key` | One private component | Internal lookup discriminator with no public or cross-component identity meaning. |

An `Id` answers **which entity?** Applications supply `RoomId` and
`ParticipantId` as case-sensitive names of 1 to 36 ASCII letters, digits,
underscores, or hyphens. PulseBeam preserves the spelling exactly. A room is
scoped to its project; a participant endpoint is scoped to its project and room.
Neither name is a routing destination or a production log identifier.

PulseBeam mints each `ConnectionId`. SDKs carry connection IDs without deriving
them. A connection ID identifies one physical incarnation of an endpoint, not
its application identity.

An `Address` answers **where is the entity's current placement?** It may cross
nodes and can change without changing the entity's `Id`. A `Handle` directly
references live shard-owned state and must not leave that shard incarnation. A
`Key` exists only for its component's lookup and has no independent routing,
logging, or stability contract.

Semantic meaning wins over storage mechanics. An `Id` stored in a map remains
an `Id`; a shard `Handle` implemented with a slot map remains a `Handle`.
Private indices and slots remain representation details unless the component
needs a distinct `Key`. Natural domain nouns such as `MetricSeries`,
`TrackPlan`, and `RouteEnvelope` need no suffix.

## Identity and routing flow

```text
verified ProjectId + requested RoomId + JWT sub (ParticipantId)
    -> (project, room, participant) endpoint

connection admission
    -> ConnectionId

publication admission on the owning connection
    -> PublicationId

PublicationId + current placement
    -> RouteAddress
    -> destination shard resolves TrackHandle
```

`ConnectionId` is minted UUIDv7. The controller tracks the scoped endpoint
alongside its current connection ID. A shard's `Connection` owns media
execution for one incarnation; replacing that incarnation constructs a new
runtime. Its `RtcDriver` drives the lower-level `pulsebeam_rtc::RtcConnection`
WebRTC engine.

SDP bootstrap installs admitted initial publications and receiver slots into that
runtime. WHIP/WHEP admit those resources from SDP; the State adapter retains SDP
transport facts but admits media through resource declarations. Optional state
control updates installed resources through the same connection.
Protocol identity is bootstrap information, not a packet-dispatch mode: MID and
channel ownership select execution. Subscription defaults and permission to open
legacy topic channels are explicit connection policy. Authorization expiry fences
negotiated ingress as well as configured resources.

The server mints a UUIDv7 `PublicationId` at admission. The canonical text form
is `pub_0...` for audio, video, and data. Media kind, connection ownership, and
logical name are separate metadata. Use the ID from confirmed state or the
catalog rather than trying to derive it. Disabling a continuing publication
preserves its ID; retiring it and publishing again creates a new one. A
connection's canonical text form is `conn_0...`.

Address scope is explicit:

```text
NodeRouteAddress       = shard + private slot + epoch
RouteAddress           = node + NodeRouteAddress
NodeTransportAddress   = shard + private slot + epoch
TransportAddress       = cluster + node + NodeTransportAddress
```

Use private lookup and non-identity nouns for their actual roles:

```text
SubscriptionKey       UpstreamSlotKey       DownstreamSlotKey
FlowKey               MetricSeries
ControllerSender      TcpAcceptor           TransportAddressAllocator
```

For the distinction between a node-owned `RoomShard`, a local `RoomPartition`,
and an execution `Shard`, see [cluster topology and ownership](cluster.md).
These are placement and ownership concepts, not alternate identity suffixes.

## HTTP signaling

Native, WHIP, and WHEP create connection resources at:

```text
POST /api/v1/native
POST /api/v1/whip
POST /api/v1/whep
```

Each successful request returns an absolute `Location` ending in a canonical
V0 `ConnectionId`, for example `/api/v1/native/conn_0...`. Native JSON exposes
canonical room, participant, and connection IDs where the agent needs to carry
them; WHIP and WHEP clients treat the complete returned `Location` as opaque.
The Utoipa annotations in `control/api.rs` are the source of truth for generated
API documentation.
