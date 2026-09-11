# Naming boundaries

PulseBeam names values by the boundary at which they are valid, not by the
container that stores them. The suffix is a contract: changing it changes where
the value may be used.

This is the target contract for the identity and addressing migration. Existing
legacy names do not weaken it and must not be copied into new interfaces.

| Suffix | Meaning | Valid scope |
| --- | --- | --- |
| `ExternalId` | Opaque identity supplied by an application or SDK user | Its documented application scope |
| `Id` | Stable identity issued by the PulseBeam server | PulseBeam and authorized consumers, across nodes and restarts |
| `Address` | Routable destination for a live placement | Its named routing scope |
| `Handle` | Reference to live shard-owned state | One shard incarnation |
| `Key` | Lookup value owned by one component | The owning collection or component |

The boundary rules are:

- An `ExternalId` enters through an API or SDK configuration and may be scoped
  by a parent. The application or SDK user owns its value. It is never a
  routing destination or the canonical entity value in production logs.
- An `Id` answers **which PulseBeam entity?** The PulseBeam server is the sole
  authority that derives or mints it. It is serializable, stable, and
  independent of current placement. Use it for cluster correlation and
  canonical entity fields in production logs.
- An SDK may receive, retain, compare, and send an `Id` back when a PulseBeam
  protocol asks it to reference an entity. Receiving or using an `Id` does not
  transfer issuance authority: clients and SDKs must never mint or derive one.
- An `Address` answers **where is its current live destination?** It is minted
  by the destination owner and may contain node, shard, private slot, and epoch
  information. Migration produces a new address without changing the entity's
  `Id`.
- A `Handle` directly references live state owned by one shard. It must not be
  sent to the controller, another shard, or another node.
- A `Key` exists only to perform a component-owned lookup. It has no independent
  entity, routing, logging, or stability contract outside that owner.

Semantic meaning wins over storage mechanics. An `Id` stored in a `HashMap`
remains an `Id`; a shard `Handle` implemented with `slotmap` remains a
`Handle`. Conversely, a private lookup type may be a vector index, reusable
slot, generational slot, or composite value and is still named `Key`.

`index` and `slot` are ordinary representation terms, not PulseBeam identity
suffixes. Keep them as private fields or local variables. If a component needs
a distinct lookup type, use `Key` rather than exposing whether its current
implementation uses an index or slot. Natural domain nouns such as
`MetricSeries`, `TrackPlan`, and `RouteEnvelope` need no artificial suffix.

## Identity and routing flow

```text
application / SDK user
    supplies RoomExternalId and ParticipantExternalId

PulseBeam server
    RoomExternalId
        -> derives RoomId

    (RoomId, ParticipantExternalId)
        -> derives ParticipantId

    (ParticipantId, TrackKind, TrackLabel)
        -> derives TrackId

    connection creation
        -> mints ConnectionId

server-issued Id
    -> may be published to an SDK
    -> may be referenced by that SDK in later protocol messages

server-owned TrackId + current placement
    -> destination owner mints RouteAddress
    -> destination shard resolves TrackHandle
```

`RoomId`, `ParticipantId`, and `TrackId` are deterministic hierarchical
UUIDv8 identities. `ConnectionId` is a minted UUIDv7 identity. All use the
canonical PulseBeam text form:

```text
<entity-prefix>_<1 Crockford Base32 version character><26 Crockford Base32 UUID characters>
```

The contract version is exactly one Crockford Base32 character using the same
canonical uppercase alphabet as the UUID portion. It versions the complete
`Id` construction contract: `0` is the implemented V0 contract; incompatible
future contracts use a different version character.
Only server-owned identity code may derive or mint an `Id`. Call sites must use
each concrete `Id` type's parser and formatter rather than constructing text
manually. SDK-side parsers and formatters operate only on IDs received from the
server; they do not provide derivation or minting authority.

Address scope must be explicit when more than one scope exists:

```text
NodeRouteAddress       = shard + private slot + epoch
RouteAddress           = node + NodeRouteAddress
NodeTransportAddress   = shard + private slot + epoch
TransportAddress       = cluster + node + NodeTransportAddress
```

## Naming examples

Use:

```text
RoomExternalId         ParticipantExternalId
RoomId                 ParticipantId          TrackId
ConnectionId
ParticipantHandle      TrackHandle
NodeRouteAddress       RouteAddress
NodeTransportAddress   TransportAddress
SubscriptionKey        UpstreamSlotKey        DownstreamSlotKey
FlowKey                MetricSeries
```

Avoid names that cross these meanings:

```text
ParticipantKey   # live shard state is a Handle
TrackKey         # live shard state is a Handle
RouteHandle      # a routable destination is an Address
TransportHandle  # a routable destination is an Address
RouteId          # placement is not entity identity
MetricKey        # a metric series is shared domain data, not a private lookup
EntityId         # use the concrete entity type
```

At HTTP boundaries, make external ownership explicit in both paths and code:

```text
/api/v1/rooms/{room_external_id}/participants/{participant_external_id}
```

Server responses and signaling protocols may expose canonical PulseBeam IDs
when an SDK must refer to a server-owned entity. For example, a server catalog
may publish `ParticipantId` and `TrackId` values that later client intents
reference. The SDK treats those values as opaque server-issued identities.

Signaling and other protocols migrate independently; do not rename their
fields implicitly as part of an HTTP API change.
