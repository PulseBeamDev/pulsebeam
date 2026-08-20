# PulseBeam API

Status: Proposed  
Scope: Public HTTP API, entity model, identity/security, hard transport limits, WHIP/WHEP, and raw webhooks.

## 1. Boundary

PulseBeam exposes a small RTC primitive:

```text
Room
└── Participant
    ├── Publications        logical room media
    └── Connections         physical transport
        ├── upstream bindings
        ├── downstream slots
        └── data channels
```

A `Connection` is a **role-free WebRTC transport container**. It is not a camera, screen, publisher, receiver, recorder, or data connection.

Media semantics belong to `Publication` and subscription intent. Data semantics belong to the data protocol/SDK. A connection only provides bounded transport capacity.

## 2. Entity IDs

PulseBeam uses one standard entity encoding everywhere:

```text
<prefix>_<Crockford Base32>
```

Examples:

```text
r_...   RoomId
p_...   ParticipantId
c_...   ConnectionId
t_...   TrackId
```

Rules:

- entity IDs are opaque strings on every public wire;
- clients MUST NOT parse the prefix payload or UUID representation;
- do not introduce a second encoding for HTTP, protobuf, or webhooks;
- new entity types use their assigned prefix plus the same Crockford Base32 encoding.

`participant_secret` is **not an entity ID** and does not use entity encoding.

## 3. Identity model

### `room`

Application-selected opaque room name, scoped by project.

### `room_id`

Canonical PulseBeam `RoomId`, deterministically derived from project + room using the existing namespaced UUIDv8/SHA-3 derivation.

Every node can derive the same `room_id` without a database lookup.

### `user_id`

Application identity from the authenticated join credential.

Example:

```text
user_id = "alice"
```

PulseBeam does not generate it. It is not part of the media roster.

### `participant_id`

PulseBeam logical RTC identity.

Example:

```text
participant_id = p_...
```

It is server-defined, opaque, and stable across the participant's connections.

The same application user may intentionally have multiple participants:

```text
user_id = "alice"

laptop -> p_A
phone  -> p_B
```

### `connection_id`

Opaque server-generated `ConnectionId`:

```text
c_...
```

It identifies exactly one concrete WebRTC connection and has no semantic role.

Connection IDs do not need participant-wide sequencing or coordination.

### `track_id`

Opaque `TrackId`:

```text
t_...
```

A publication is logically identified by its track ID and owned by a participant.

## 4. Participant continuity

`participant_id` is public room identity, so possession of it MUST NOT allow another client to attach a connection to that participant.

PulseBeam therefore returns a server-generated `participant_secret`.

### Creation

PulseBeam generates 128 random bits using the OS CSPRNG:

```text
S = participant_secret
```

Then derives:

```text
participant_id =
    p_ + encode(
        UUIDv8-SHA3(
            namespace = room_uuid,
            label = framed(
                "v1/participant",
                authenticated user_id,
                S
            )
        )
    )
```

`framed(...)` MUST be unambiguous and versioned.

The client never chooses or derives `participant_id`.

### Verification on any node

For an additional connection, any node receives:

```text
authenticated join credential
participant_id
participant_secret
```

Before allocating WebRTC state it recomputes:

```text
expected =
    derive(
        authenticated room_id,
        authenticated user_id,
        participant_secret
    )
```

and requires:

```text
expected == participant_id
```

This is stateless.

No participant lookup, participant token, shared MAC key, Redis entry, sticky routing, or participant-wide connection allocator is required.

### Threat model

The construction protects against:

- another room client learning `participant_id`;
- another device with the same `user_id` trying to join the participant without its secret;
- a stolen participant secret used under a different authenticated user/room.

It does not protect against compromise of both:

```text
join credential + participant_secret
```

At that point the attacker has the credentials required to act as the participant.

`participant_secret` MUST NOT appear in URLs, room signaling, webhooks, logs, metrics, or tracing attributes.

## 5. Entity relationships

The important distinction is **logical entity vs transport-local binding**:

```text
Participant p_A
│
├── Publication t_camera        room-global logical entity
├── Publication t_mic
├── Publication t_screen
│
├── Connection c_1             transport
│   ├── upstream video -> t_camera
│   ├── upstream audio -> t_mic
│   ├── video Rx slots[16]
│   ├── audio Rx slots[16]
│   └── RTC data channels
│
└── Connection c_2
    ├── upstream video -> t_screen
    ├── video Rx slots[16]
    ├── audio Rx slots[16]
    └── RTC data channels
```

### Downstream slots are not entities

A downstream slot is connection-local transport capacity.

For WebRTC/PeerConnection it is addressed by connection-local `mid`.

Bindings map:

```text
(connection, mid) -> track_id
```

The room entity is the remote `track_id`, not the slot.

### RTC data channels are not room entities

An `RTCDataChannel`/SCTP stream is a connection-local transport mechanism.

Application-level data topics/flows are defined by the PulseBeam data protocol and SDK and may be mapped to any suitable connection.

Do not make the connection ID encode data semantics.

## 6. Hard connection shape

Every connection has the same hard limits:

```text
MAX_UPSTREAM_VIDEO_PER_CONNECTION         = 1
MAX_UPSTREAM_AUDIO_PER_CONNECTION         = 1
MAX_DOWNSTREAM_VIDEO_SLOTS_PER_CONNECTION = 16
MAX_DOWNSTREAM_AUDIO_SLOTS_PER_CONNECTION = 16
```

Data-channel limits are defined separately by the data transport protocol.

There is no protocol-level `MAX_CONNECTIONS_PER_PARTICIPANT`.

Deployment/account/node capacity limits may still bound total WebRTC connections.

## 7. Why 1 video + 1 audio upstream

The bound is deliberate.

### Natural A/V source pair

The common publishing unit is:

```text
camera + microphone
```

or:

```text
screen + screen audio
```

One slot per media kind keeps the natural synchronized pair together.

### Independent source lifecycle

A second video source composes another connection. Starting/stopping screen share does not renegotiate the camera transport.

### Fixed publisher state

Every connection has at most:

```text
0..1 video publication
0..1 audio publication
```

This bounds SDP validation, routing registration, publish state, reconnect handling, and simulation invariants.

### Composition is the escape hatch

Multi-camera teleoperation, recorders, agents, and future clients use more generic connections rather than growing one connection into an arbitrary publication container.

## 8. Why 16 video + 16 audio downstream

These are architectural planning bounds, not merely SDK defaults.

### Fixed worst-case state

One connection has at most:

```text
16 video receiver slots
16 audio receiver slots
```

The server can use compact dense structures and small bitsets.

### Fixed worst-case work

Binding, allocation, adaptation, signaling, receiver state, and simulation work are bounded per connection.

A client cannot negotiate hundreds of downstream m-lines and enlarge one shard's per-connection hot-path cost.

### Why 16 video

Sixteen supports a full 4x4 gallery on one connection and gives substantial room for monitoring/teleoperation.

Eight forces ordinary clients into multiple connections too early.

Thirty-two or sixty-four increase worst-case state and work without being necessary because multiple connections already provide scale-out.

### Why 16 audio

PulseBeam audio is top-N/pin based. Sixteen simultaneous audio slots are already generous for an interactive client.

A recorder that needs more uses additional connections and explicit pins.

### Simple planning unit

The connection contract stays memorable:

```text
<= 1V up
<= 1A up
<= 16V down
<= 16A down
```

## 9. HTTP API

The SDK API is JSON-first and resource-oriented.

### Join: create participant and first connection

```http
POST /v1/rooms/{room}/participants
Authorization: Bearer <join-credential>
Content-Type: application/json
```

```json
{
  "offer": "v=0\r\n..."
}
```

Response:

```json
{
  "room_id": "r_...",
  "participant_id": "p_...",
  "participant_secret": "<opaque secret>",
  "connection": {
    "connection_id": "c_...",
    "answer": "v=0\r\n..."
  }
}
```

### Add another connection

```http
POST /v1/participants/{participant_id}/connections
Authorization: Bearer <join-credential>
Content-Type: application/json
```

```json
{
  "participant_secret": "<opaque secret>",
  "offer": "v=0\r\n..."
}
```

Response:

```json
{
  "connection_id": "c_...",
  "answer": "v=0\r\n..."
}
```

The receiving node verifies participant continuity before creating ICE/DTLS/SRTP/SCTP state.

### Delete a connection

```http
DELETE /v1/participants/{participant_id}/connections/{connection_id}
Authorization: Bearer <join-credential>
```

```text
204 No Content
```

A connection ID always identifies one concrete transport. Reconnect creates another connection; there is no `/reconnect` action endpoint.

## 10. Media signaling

HTTP only bootstraps transport.

The reliable connection-local signaling channel remains authoritative for live media intent.

Room-global entities:

```proto
message Participant {
  string participant_id = 1;
}

message Publication {
  string track_id = 1;
  string participant_id = 2;
  TrackKind kind = 3;
}
```

Downstream binding remains connection-local:

```text
VideoIntent.mid  -> local video slot
AudioBinding.mid -> local audio slot
```

`track_id` is global; `mid` is local.

Self-subscription exclusion compares:

```text
publication.participant_id == local participant_id
```

not connection IDs or application `user_id`.

## 11. Raw webhooks

PulseBeam emits only locally authoritative facts:

```text
connection.created
connection.deleted
publication.created
publication.deleted
```

No:

```text
participant.joined
participant.left
participant.online
room.started
room.ended
user.online
```

Those require cross-node reduction and are outside this scope.

### Producer-local ordering

If ordering metadata is exposed, it is local to a producer incarnation:

```text
node_id
shard_id
incarnation
sequence
```

No room-global sequence is promised.

### Event example

```json
{
  "type": "publication.created",
  "producer": {
    "node_id": "n_...",
    "shard_id": 3,
    "incarnation": 7
  },
  "sequence": 184,
  "room_id": "r_...",
  "data": {
    "user_id": "alice",
    "participant_id": "p_...",
    "connection_id": "c_...",
    "track_id": "t_...",
    "kind": "video"
  }
}
```

The event means only that this local producer installed that publication instance.

Centralized reduction, durable delivery, billing aggregation, presence, and analytics are separate layers.

## 12. WHIP

WHIP remains a secondary RFC 9725-compatible ingest API.

```text
POST /v1/whip/rooms/{room}
```

WHIP keeps its standard SDP body, `Location`, `ETag`, PATCH, and DELETE semantics.

Internally, a WHIP ingest maps to an ordinary generated participant + connection subject to:

```text
<= 1 video up
<= 1 audio up
```

The WHIP session `Location` uses its own unguessable external session token. It does not reuse `connection_id` as authorization.

## 13. WHEP

WHEP remains a secondary room-scoped egress API:

```text
POST /v1/whep/rooms/{room}
```

A WHEP offer may negotiate up to:

```text
16 video receive slots
16 audio receive slots
```

Video uses PulseBeam automatic assignment.

Audio uses the existing top-N selector.

WHEP does not expose the richer SDK `VideoIntent`/pin/priority model.

Its external session URL is separate from the internal `connection_id`.

## 14. Invariants

1. All public entity IDs use `<prefix>_<Crockford Base32>`.
2. `user_id` is application identity.
3. `participant_id` is server-defined PulseBeam identity.
4. `participant_secret` proves continuity and is not an entity ID.
5. Any node can verify participant continuity locally before allocating transport.
6. A participant may own multiple connections on different nodes.
7. A connection is role-free transport capacity.
8. Connection identity has no media/data semantics.
9. Publications are logical participant-owned room entities.
10. Downstream slots and RTC data channels are connection-local transport resources.
11. Each connection is bounded to 1V/1A upstream and 16V/16A downstream.
12. HTTP manages connection lifecycle; the data channel manages live intent.
13. Webhooks expose only local facts.
14. Cross-node reduction is outside PulseBeam core.
