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

connection admission
    -> ConnectionId

ConnectionId + TrackKind + TrackLabel + publication incarnation
    -> TrackId

TrackId + current placement
    -> RouteAddress
    -> destination shard resolves TrackHandle
```

`RoomId` and `ParticipantId` are deterministic hierarchical UUIDv8 identities.
`ConnectionId` is minted UUIDv7. The controller tracks participant identity and
room resources alongside the current connection ID. A shard's `Connection` owns
media execution for one participant connection incarnation; replacing that
incarnation constructs a new runtime. The shard currently keeps at most one
runtime per participant. Its `RtcDriver` drives the lower-level
`pulsebeam_rtc::RtcConnection` WebRTC engine.

SDP bootstrap installs admitted initial publications and receiver slots into that
runtime. WHIP/WHEP admit those resources from SDP; the State adapter retains SDP
transport facts but admits media through resource declarations. Optional state
control updates installed resources through the same connection.
Protocol identity is bootstrap information, not a packet-dispatch mode: MID and
channel ownership select execution. Subscription defaults and permission to open
legacy topic channels are explicit connection policy. Authorization expiry fences
negotiated ingress as well as configured resources.

`ConnectionId::derive_publication_track_id`
derives a UUIDv8 `TrackId` from the connection, track kind, immutable label and
nonzero publication incarnation. For desired/confirmed resources, the incarnation
is the first desired revision (owned ref), not each later settings revision.
Disabling a continuing publication preserves it; retirement and same-label
republishing require a fresh one. The transcript is the publication domain
`pulsebeam.identity.publication.v1`, connection UUID bytes, kind byte, incarnation
as big-endian uint64, then the label's big-endian uint32 byte length and UTF-8
bytes. UUID version/variant bits are applied to the first 16 SHA3-256 bytes.

`derive_track_id` retains the single bootstrap-lifetime transcript for SDP and
legacy sources without owned refs. It cannot identify multiple same-label
lifetimes on one connection. Both derivations retain the existing canonical
encoding; a reconnect creates fresh track IDs. The V0 text encodings are `rm_0...`, `pa_0...`, `aud_0...` / `vid_0...` / `dat_0...`, and `c_0...`.
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
V0 `ConnectionId`, for example `/api/v1/native/c_0...`. Native JSON exposes
canonical room, participant, and connection IDs where the agent needs to carry
them; WHIP and WHEP clients treat the complete returned `Location` as opaque.
The Utoipa annotations in `control/api.rs` are the source of truth for generated
API documentation.
