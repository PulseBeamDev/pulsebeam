# PulseBeam SDK Design

Status: Proposed  
Scope: Client entities, connection allocation, data-channel placement, and browser/native layering.

## 1. Client model

Applications program against one logical participant:

```text
LocalParticipant
├── publications
├── subscriptions
└── data
```

The SDK hides the transport pool:

```text
Participant
├── Connection c_1
├── Connection c_2
└── ...
```

A connection is role-free transport capacity:

```text
<= 1 video up
<= 1 audio up
<= 16 video down
<= 16 audio down
+ data channels
```

Do not model camera, screen, publisher, receiver, or data connections.

## 2. Logical entities vs transport bindings

The client keeps a strict distinction.

### Logical state

Stable across placement/reconnect:

```text
ParticipantId
TrackId / Publication
subscription intent
data topic / flow
```

### Connection-local state

May change without changing logical identity:

```text
ConnectionId
upstream media slot
video Rx slot / MID
audio Rx slot / MID
RTCDataChannel / SCTP stream
connection-local protocol state
```

Example:

```text
TrackId t_B
    ↓
logical subscription
    ↓
pulsebeam-agent assignment
    ↓
Connection c_2 + video slot 7
    ↓
PeerConnection backend: MID "v7"
```

Downstream slots and RTC data channels are **not room entities**.

## 3. Connection allocation

`pulsebeam-agent` owns the connection pool and all placement decisions.

General rule:

> Reuse existing capacity before creating another connection.

### Publishing

Video:

```text
free video-up slot?
├── yes → use it
└── no  → request another connection
```

Audio follows the same rule.

The allocator should prefer colocating related pairs:

```text
camera + microphone
screen + screen audio
```

but this is client policy, not connection semantics.

### Downstream video

```text
subscribe(track)
    ↓
find free video-down slot
    ↓
assign (connection, slot)
    ↓
emit signaling effect for that connection
```

Only create another connection when existing video-down capacity is exhausted.

### Downstream audio

Same rule for audio slots.

For normal interactive clients, only one connection should normally use automatic top-N audio. Additional receiver capacity can use explicit pins. This avoids duplicate independent top-N selection across connections.

### Example

```text
c_1
├── camera up
├── microphone up
├── 12/16 video down
├── 6/16 audio down
└── data

start screen share
    ↓
c_1 video-up occupied
    ↓
create c_2

c_2
├── screen up
├── screen audio up
└── downstream currently empty
```

Later, `c_2` may also receive downstream media. It never becomes a special screen connection.

## 4. Data channels

Data makes the logical/transport boundary especially important.

The application API should be participant/topic-oriented:

```text
publishData(topic, ...)
subscribeData(topic, ...)
```

not:

```text
connection.openDataChannel(...)
```

### Logical data state

`pulsebeam-agent` owns:

```text
topic / flow identity
reliability semantics
desired publication/subscription
assignment to a transport
retry/recovery policy
```

### Physical data transport

The async runtime owns:

```text
RTCDataChannel / SCTP stream
open/close
buffering
backpressure
actual send/receive I/O
```

Placement should be stable rather than aggressively balanced:

```text
1. choose an existing suitable connection;
2. keep the flow there while healthy;
3. move/recreate only when transport failure or policy requires it.
```

Each connection may also have its own PulseBeam internal signaling data channel because MIDs and media bindings are connection-local.

## 5. Package and crate layering

This is the canonical client layering.

```text
APPLICATION API
────────────────────────────────────

@pulsebeam/react
    React hooks/components
    strictly depends on:
        ↓
@pulsebeam/client
    public TypeScript/browser API
    - JS ergonomics
    - MediaStreamTrack handles
    - devices
    - browser-facing types

        ↓

PLATFORM RUNTIME — ASYNC
────────────────────────────────────

pulsebeam-agent-web
    browser/WASM runtime
    - fetch / promises
    - browser timers
    - MediaStreamTrack integration
    - RTCPeerConnection today
    - RTCTransport later
    - executes Effects from pulsebeam-agent
    - converts browser callbacks into Events

            OR, same layer:

pulsebeam-agent-native
    native runtime
    - native async I/O
    - timers
    - pulsebeam-rtc / str0m
    - executes Effects from pulsebeam-agent
    - converts RTC/network callbacks into Events

        ↓

BEHAVIORAL CORE — FULLY SYNCHRONOUS
────────────────────────────────────

pulsebeam-agent
    deterministic PulseBeam client state machine
    - participant/publication state
    - desired subscriptions/data intent
    - connection pool
    - upstream/downstream assignment
    - data-flow assignment
    - reconciliation
    - reconnect/session policy
    - retry decisions
    - protocol state
    - consumes Events
    - emits Effects
    - NEVER await
    - NEVER perform I/O
    - simulator drives this exact crate

        ↓

SHARED LIBRARIES — FULLY SYNCHRONOUS
────────────────────────────────────

pulsebeam-core
    shared with server
    - entity IDs
    - domain types
    - shared algorithms/invariants

pulsebeam-proto
    protobuf wire definitions
    - encode/decode
```

Dependency graph:

```text
                  @pulsebeam/react
                         │
                         ▼
                  @pulsebeam/client
                         │
                         ▼
                pulsebeam-agent-web
                         │
                         ▼
                pulsebeam-agent
                    /            \
                   ▼              ▼
           pulsebeam-core   pulsebeam-proto
```

Native is parallel:

```text
             pulsebeam-agent-native
                         │
                         ▼
                 pulsebeam-agent
                    /            \
                   ▼              ▼
           pulsebeam-core   pulsebeam-proto
```

`@pulsebeam/react` MUST remain strictly above `@pulsebeam/client`.

## 6. What lives in `pulsebeam-agent`

`pulsebeam-agent` is the shared synchronous client brain.

It contains the concepts previously described as participant engine, connection pool, allocators, and per-connection protocol state. Those are modules/responsibilities, not separate architectural layers or crates.

A useful internal shape is:

```text
pulsebeam-agent
├── Agent
│   ├── participant state
│   ├── room/publication view
│   ├── desired intent
│   └── reconnect/retry policy
│
├── connections
│   ├── logical ConnectionId state
│   ├── upstream capacity
│   ├── downstream slot assignments
│   ├── data-flow assignments
│   └── connection-local protocol state
│
├── reconciliation
│   ├── desired → current diff
│   └── placement decisions
│
├── Event
└── Effect
```

The core may know PulseBeam protocol concepts such as:

```text
ConnectionId
TrackId
slot index
MID binding
ClientIntent
ServerState
```

It MUST NOT own platform RTC objects such as:

```text
RTCPeerConnection
RTCRtpTransceiver
MediaStreamTrack
RTCDataChannel
native socket
timer handle
```

MID can exist as protocol state because the current PeerConnection signaling protocol uses it, but platform manipulation of transceivers/MIDs belongs to the runtime.

## 7. Event / Effect model

The core interaction is synchronous:

```text
ASYNC RUNTIME
     │
     │ Event
     ▼
┌──────────────────────┐
│ pulsebeam-agent │
│                      │
│ event → state → effect
└──────────┬───────────┘
           │ Effect
           ▼
ASYNC RUNTIME
```

Example:

```rust
agent.handle(Event::Disconnected(connection_id), &mut effects);
```

The core may emit:

```rust
Effect::StartReconnectTimer { ... }
```

The runtime schedules the timer.

Later:

```rust
agent.handle(Event::TimerFired(timer_id), &mut effects);
```

Rule:

> `pulsebeam-agent` decides synchronously.  
> `pulsebeam-agent-web` and `pulsebeam-agent-native` perform asynchronously.

Typical events:

```text
JoinRequested
ConnectionReady
ConnectionClosed
ServerStateReceived
TimerFired
LocalTrackAdded
LocalTrackRemoved
SubscribeRequested
DataReceived
DataWritable
```

Typical effects:

```text
CreateConnection
DeleteConnection
StartTimer
CancelTimer
SendClientIntent
BindLocalTrack
OpenDataChannel
SendData
CloseDataChannel
```

Effects describe desired side effects; they do not contain platform objects.

## 8. Browser runtime

Today:

```text
pulsebeam-agent-web
├── fetch
├── browser timers
├── MediaStreamTrack
├── RTCPeerConnection
├── RTCRtpTransceiver
└── RTCDataChannel
```

It:

1. executes effects from `pulsebeam-agent`;
2. translates browser callbacks/promises into core events;
3. owns JS/WASM/platform object lifetimes;
4. does not duplicate PulseBeam behavioral policy.

`@pulsebeam/client` sits above it and owns the ergonomic public TypeScript API.

## 9. Native runtime

```text
pulsebeam-agent-native
├── native async runtime
├── timers
├── network/device integration
└── pulsebeam-rtc / str0m
```

It drives the exact same `pulsebeam-agent`.

The behavioral differences between browser and native should come from platform capability/events, not independently implemented PulseBeam state machines.

## 10. RTCTransport evolution

RTC implementation belongs inside/under the async runtime, never under `pulsebeam-agent`.

Browser today:

```text
pulsebeam-agent-web
    ├── MediaStreamTrack
    ├── RTCPeerConnection
    └── RTCDataChannel
```

Browser future:

```text
pulsebeam-agent-web
    └── RTCTransport
        + lower-level media/data integration
```

Native:

```text
pulsebeam-agent-native
    └── pulsebeam-rtc / str0m
```

The public API and behavioral core should survive that transport replacement.

Therefore `pulsebeam-agent` should model PulseBeam intent and assignments, not assume that a PeerConnection is the permanent browser primitive.

## 11. No `pulsebeam-media` yet

Do not introduce a shared `pulsebeam-media` implementation crate today.

Current reality is asymmetric:

```text
browser → opaque RTCPeerConnection media machinery
native  → raw RTC/media machinery
```

A shared media implementation would create artificial abstraction without shared implementation.

If RTCTransport eventually exposes enough packet/media control, then extracting:

```text
pulsebeam-media
    shared RTP/RTCP/media logic
```

may become natural.

Until then:

> Share behavior in `pulsebeam-agent`; keep RTC/media mechanics in the platform runtimes.

## 12. Join and connection lifecycle

First join:

```text
@pulsebeam/client
    ↓
pulsebeam-agent-web
    ↓ Event
pulsebeam-agent
    ↓ Effect::CreateInitialConnection
pulsebeam-agent-web
    ↓ HTTP + RTCPeerConnection
POST /rooms/{room}/participants
    ↓
Event::Joined {
  room_id,
  participant_id,
  participant_secret,
  connection_id,
  ...
}
    ↓
pulsebeam-agent
```

Additional capacity:

```text
pulsebeam-agent reconciliation
    ↓
needs another connection
    ↓
Effect::CreateConnection
    ↓
runtime creates offer + HTTP request
    ↓
Event::ConnectionCreated
```

The application never manages `participant_secret`, connection placement, MIDs, or SCTP streams.

## 13. Reconnect

Reconnect preserves logical intent while replacing transport:

```text
ConnectionClosed
    ↓
pulsebeam-agent keeps desired publications/subscriptions/data
    ↓
retry policy
    ↓
Effect::CreateConnection
    ↓
new ConnectionId
    ↓
pulsebeam-agent reconciles assignments
```

Logical identities may remain stable:

```text
ParticipantId
TrackId
data topic / flow
```

Physical placement may change:

```text
ConnectionId
MID / slot
RTCDataChannel / SCTP stream
```

## 14. Entity encoding

All PulseBeam entity IDs use the existing public encoding:

```text
<prefix>_<Crockford Base32>
```

Examples:

```text
RoomId        r_...
ParticipantId p_...
ConnectionId  c_...
TrackId       t_...
```

The SDK treats them as opaque on the wire and may use strongly typed wrappers internally.

Do not introduce another entity encoding in the browser, WASM boundary, native agent, protobuf, or HTTP API.

`participant_secret` is secret material, not an entity ID.

## 15. Invariants

1. Applications see participant/publication/subscription/data concepts, not transport topology.
2. Connections are generic role-free capacity.
3. `pulsebeam-agent` owns all behavioral policy and connection allocation.
4. Downstream slots are connection-local bindings, not room entities.
5. Data topics/flows are logical; RTC data channels/SCTP streams are runtime transport.
6. The core is fully synchronous and deterministic.
7. The core never performs I/O or awaits.
8. Browser and native runtimes execute Effects and return Events.
9. The simulator drives the exact `pulsebeam-agent`.
10. Browser RTC objects live only in `pulsebeam-agent-web`.
11. Native RTC objects live only in `pulsebeam-agent-native`.
12. `@pulsebeam/react` strictly depends on `@pulsebeam/client`.
13. RTCTransport can replace RTCPeerConnection without replacing the behavioral core.
14. Do not create `pulsebeam-media` until real shared raw-media implementation exists.
15. Entity IDs always use `<prefix>_<Crockford Base32>`.
