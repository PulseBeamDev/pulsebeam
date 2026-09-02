# `pulsebeam-rtc`

`pulsebeam-rtc` is PulseBeam's production WebRTC connection boundary for an
SFU. It encapsulates one peer connection and is designed to replace direct
`str0m` use in `pulsebeam`. Migrating the server is a separate project.

## Ownership boundary

- One `Connection` owns one peer's negotiation, ICE, DTLS, SRTP, SCTP, RTP,
  RTCP, stream composition, timers, scheduler, pacer, probing, and congestion
  control.
- The caller owns sockets, all connections, and shard scheduling. It feeds
  individual UDP packets or RFC 4571-framed ICE-TCP packets into a connection.
- A connection has no threads, callbacks, shared mutable state, global scans,
  or cross-connection coordination. It is `Send`, not `Sync`.
- Internal stream state is dense and data-oriented. Storage is sized from the
  accepted session rather than compile-time maxima.
- `str0m` supplies low-level ICE, DTLS, and SRTP components. `dcsctp` supplies
  low-level SCTP. Their types and policy do not cross the public API.

The SFU owns cross-connection routing, source and layer selection, keyframe
caches, and semantic admission. Once media is admitted, the connection owns
its wire representation and transport feasibility.

## Connection model

The public surface is one `Connection` facade plus configuration, stable typed
IDs, immutable media values, events, transmits, and statistics.

Inputs mutate connection state. The caller repeatedly drives:

```text
poll(now) -> Transmit | Event | Idle { next_wakeup }
```

`Idle` is the connection's single externally scheduled timer. Every emitted
transmit has an opaque receipt. The caller must report its actual socket
departure time or failure; TWCC history and forwarding-latency measurements use
that result rather than poll time.

A connection supports graceful close, which rejects new application work and
drains protocol shutdown traffic to a deadline, and immediate abort.

## Session and interoperability

- The session is one immutable ICE-lite remote-offer/local-answer exchange.
- BUNDLE, RTCP mux, inline candidates, all SDP media directions, UDP, and
  passive ICE-TCP over IPv4 and IPv6 are supported.
- Trickle ICE, ICE restart, media renegotiation, and a TURN client are outside
  this boundary. A changed session creates a new connection. Client relay
  candidates remain usable.
- A fresh ephemeral DTLS identity is generated from injected cryptographic
  entropy for each connection. Its private key is neither exported nor reused.
- Codecs and RTP header extensions are negotiated from SFU configuration. Core
  forwarding does not require payload parsing and must carry opaque or SFrame
  protected media.
- Egress video requires transport-wide congestion control. Negotiation rejects
  its absence with a specific reason instead of silently selecting a poorly
  performing fallback.

Compatibility means any standards-compliant client can connect when its offer
intersects the configured capabilities. Live acceptance evidence covers current
Chrome and Firefox. Stored SDP is parser regression evidence only; Safari and
`webrtcbin` are not acceptance targets.

## Packet and stream model

After ingress decryption, one immutable `Bytes` allocation is the canonical
plaintext packet. Parsing records compact byte ranges; semantic accessors decode
metadata lazily. Local fanout uses shallow clones. `to_transit` performs an
explicit deep copy before a packet crosses a shard or node boundary, so
reference counts never become shared packet-runtime state across cores.

Every ingress media packet retains its receive timestamp. Egress transmits
retain that timestamp until the socket departure receipt, making forwarding
latency directly measurable without a logging or metrics dependency. Timestamps
are comparable only within their monotonic clock domain.

- Negotiated media produce stable sender IDs. The set of senders cannot change
  after the answer.
- Unsignaled ingress SSRCs and RIDs produce stable encoding IDs and an
  `EncodingDiscovered` event before media delivery.
- Negotiated encodings are always admitted. Unsignaled encodings consume a
  configured connection-wide budget; overflow is dropped and counted without
  closing the connection.
- An unsignaled encoding remains stable until RTCP BYE or explicit SFU
  retirement. Inactivity alone never retires a paused encoding.
- The session accepts at most 128 negotiated media sections. Runtime encoding,
  RTX, and SSRC-zero probe entities are separate, dense, externally bounded
  entities rather than reserved media slots. There is no crate-level connection
  count limit.

The SFU forwards packets with `send_media(sender_id, packet)` and can switch
sources at any packet boundary. The connection preserves outbound SSRC,
payload-type, sequence, timestamp, RTP-extension, retransmission, and RTCP
continuity. It owns NACK/RTX handling and exposes only semantic keyframe request
operations and events.

Forwarding is packet-level and cut-through; the connection does not wait for a
complete video frame. It tracks frame boundaries and dependencies so admission
and shedding prefer whole, not-yet-started frames. If bounded pressure causes
post-admission packet loss, RTP sequence gaps remain visible to the receiver.
TWCC numbers are assigned only to packets actually transmitted.

## RTP extension policy

Extension policy is immutable per negotiated media sender and keyed by semantic
URI, never source wire ID.

- Connection-managed extensions, including MID, RID, repaired RID, TWCC,
  absolute send time, and dependency descriptors that require continuity, are
  regenerated or rewritten.
- Known endpoint-independent values such as audio level and video orientation
  can be forwarded after URI-based remapping.
- Unknown extensions are dropped by default and can be explicitly allowed as
  opaque pass-through.
- All ingress extensions remain available through lazy URI-based accessors even
  when they are not forwarded.

## Scheduling, latency, and congestion control

Protocol control remains deliverable and padding remains lowest-value traffic.
Media scheduling and allocation use mutable SFU-provided policy per sender:
playout-delay range, relative priority, and desired bitrate. The connection
measures actual sending rate itself. Policy is keyed by the stable negotiated
`SenderId`, not by a source SSRC, encoding, or RTX stream.

Each sender's playout-delay range expresses its desired quality/latency
tradeoff. A latency governor derives per-sender pacer horizon, admission,
shedding, and retransmission usefulness, then applies the strictest active
latency requirement to shared path queueing. Tightening a sender immediately
re-evaluates its queued video. The range is best-effort because remote capture,
decode, and render costs are not fully observable.

Congestion control is implemented in this crate, using libwebrtc as behavioral
guidance rather than an implementation dependency or a requirement for exact
parity. The detailed design is in
[docs/congestion-control.md](docs/congestion-control.md).

- Egress runs one connection-level, SCReAM-v2-derived delay/loss controller.
  A weighted allocator divides its latency-governed media capacity into
  per-sender allocations. The SFU configures sender priority and desired rate,
  receives those allocations, and continues to choose sources and layers.
- Ingress records packet arrivals and emits TWCC feedback. It exposes aggregate
  ingress rate, loss, RTT, and probe statistics but does not compete with the
  browser's send-side controller through REMB.
- SSRC-zero ingress packets contribute only transport feedback and never become
  media events or streams.
- SSRC-zero egress padding supports probing before media, during variable-rate
  media, and while streams are paused or application-limited.
- Probing and allocation behavior follows libwebrtc where useful, with
  documented SFU-specific decisions and deterministic tests.

## DataChannels and resource safety

DataChannels support local and remote DCEP opening, externally negotiated
channels, ordered and unordered delivery, reliability by retransmit count or
lifetime, text and binary message boundaries, buffered-amount backpressure, and
graceful close. No dcSCTP type is public.

Configuration explicitly bounds total buffered channel bytes, inbound message
size, channel count, transmit queues, incomplete protocol state, and dynamic
encodings. Outbound pressure returns a typed `WouldBlock`. An oversized
authenticated DataChannel message closes that channel where protocol semantics
permit rather than the peer connection.

Malformed, unauthenticated, replayed, unknown-tuple, and SRTP-authentication
failures are dropped with bounded cumulative counters and do not change
connection state. Authenticated violations are isolated to their stream or
channel where possible. Only unrecoverable authenticated transport state,
mandatory resource exhaustion, cryptographic failure, or timeout is terminal.
Statistics are coherent snapshots with aggregate connection data and stable-ID
stream detail; the crate has no metrics-framework dependency.

## Validation boundary

All implementation checks, tests, fixtures, benchmarks, and browser harnesses
for this project are isolated to `crates/pulsebeam-rtc`. Do not run workspace
tests for this work.

Required evidence includes deterministic crate-local network scenarios for
loss, delay, reordering, VBR, pauses, probing, malformed traffic, overload, and
timer scaling; parameterized many-connection/many-stream benchmarks with one
wakeup per connection; and live Chrome and Firefox sessions. Differential
checks against `str0m` are useful component evidence but are not independent
interoperability proof.
