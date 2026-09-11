# Naming boundaries

PulseBeam names values by who owns them and how long they remain valid. A
suffix is a boundary contract, not a description of the container holding the
value.

| Suffix | Owner and lifetime | Meaning |
| --- | --- | --- |
| `ExternalId` | User application/backend | Public opaque identity supplied to PulseBeam. |
| `Id` | PulseBeam | Stable canonical entity identity minted or derived by PulseBeam. |
| `Address` | PulseBeam control plane | Routable placement information that may cross nodes. |
| `Handle` | One runtime shard | Ephemeral incarnation valid only in its owning shard. |
| `Key` | One private component | Internal lookup discriminator with no public or cross-component identity meaning. |

An `Id` answers **which entity?** PulseBeam always owns it, even when an SDK
carries it. Clients and SDKs may retain, compare, and return an `Id` received
from PulseBeam, but they never mint or derive one.

The agent crates intentionally define same-named opaque string wrappers such as
`RoomId`, `ParticipantId`, `ConnectionId`, and `TrackId`. Those wrappers allow
cloning, exact equality, hashing, string access, and protocol forwarding. They
do not parse, validate, format, derive, mint, or expose the canonical encoding.

An `ExternalId` enters the server through the `room` and `sub` JWT claims. The
user backend owns these values; PulseBeam validates them and derives the
corresponding canonical IDs. External IDs are not routing destinations or the
entity identity used in production logs.

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
JWT claims: room + sub
    -> RoomExternalId + ParticipantExternalId

ProjectId + RoomExternalId
    -> RoomId

RoomId + ParticipantExternalId
    -> ParticipantId

ParticipantId + TrackKind + TrackLabel
    -> TrackId

connection admission
    -> ConnectionId

TrackId + current placement
    -> RouteAddress
    -> destination shard resolves TrackHandle
```

`RoomId`, `ParticipantId`, and `TrackId` are deterministic hierarchical UUIDv8
identities. `ConnectionId` is a minted UUIDv7 identity. Their V0 text encodings
are `rm_0...`, `pa_0...`, `aud_0...` / `vid_0...` / `dat_0...`, and `c_0...`.
Only the canonical identity implementation may derive, mint, parse, or format
these values.

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

## HTTP signaling

Native, WHIP, and WHEP create connection resources at:

```text
POST /api/v1/native
POST /api/v1/whip
POST /api/v1/whep
```

Each successful request returns an absolute `Location` ending in a canonical
V0 `ConnectionId`, for example `/api/v1/native/c_0...`. Native JSON exposes
canonical room, participant, and connection IDs where the agent needs to carry
them; WHIP and WHEP clients treat the complete returned `Location` as opaque.
The Utoipa annotations in `control/api.rs` are the source of truth for generated
API documentation.
