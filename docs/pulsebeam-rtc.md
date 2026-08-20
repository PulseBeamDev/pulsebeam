# pulsebeam-rtc requirements

`pulsebeam-rtc` is the shard-local RTC data plane for the PulseBeam **server**.

It is not a general WebRTC library and must never become one. It implements
exactly one role — the media-forwarding endpoint of an SFU — against exactly one
runtime, the thread-per-core shard described in `docs/thread-per-core.md`, on
exactly one platform, Linux (`docs/linux-only.md`).

`pulsebeam-agent` is out of scope and stays on str0m. That is a permanent
decision, not a migration step: the agent is a client, and every client-side
capability it needs (offer generation, candidate gathering, sample-mode media,
codec packetization) is a capability this crate exists to *not* have. Nothing in
this document may be widened to accommodate it.

The narrowness is the optimization. A general library must discover its
configuration at runtime; a library that knows its workload can decide it at
compile time. Every requirement below is downstream of that.

---

## 1. The workload

These are the current values in the tree. They are not incidental — they are the
sizing input for every data structure here.

| Property | Value | Source |
| --- | --- | --- |
| Signaling role | answerer only, never offerer | `control/negotiator.rs:88` |
| ICE role | ICE-lite, controlled only | `set_ice_lite(true)` |
| Media mode | RTP mode — forward, never decode | `set_rtp_mode(true)` |
| Codecs | H.264 + Opus | `configure_room_codecs` |
| Transports | UDP (primary), ICE-TCP (fallback) | `control/tcp_acceptor.rs` |
| Recv video slots / participant | 2 | `MAX_RECV_VIDEO_SLOTS` |
| Recv audio slots / participant | 2 | `MAX_RECV_AUDIO_SLOTS` |
| Send video slots / participant | 7 | `MAX_SEND_VIDEO_SLOTS` |
| Send audio slots / participant | 3 | `MAX_SEND_AUDIO_SLOTS` |
| Simulcast layers / encoding | 3 | `MAX_SIMULCAST_LAYERS` |
| Data channels / participant | 1 | `MAX_DATA_CHANNELS` |
| Data lanes | `rt` unreliable+unordered, `rel` app-reliable, `sys/signaling` | `track.rs:805` |
| Signaling message cap | 16 KB | `signaling.rs:12` |

No SDP crosses the data channel — signaling carries `ClientIntent` and
`StateUpdate` only — which removes the obvious large-message case. `StateUpdate`
is already a delta protocol (`tracks_upsert`/`tracks_remove`,
`assignments_upsert`/`assignments_remove`), so bounding a message to one MTU
means emitting smaller deltas, not adding a chunking layer.

**The hot loop's shape is fixed.** One ingress packet becomes N subscriber
outputs: parse once, then rewrite and encrypt N times. Per-subscriber SRTP is
irreducible — contexts differ per destination — so it is the cost floor, and
everything else on that path should approach free. Per-subscriber egress runs
roughly 500–1000 packets/second; a shard pays that times its participant count.

### 1.1 The stream-count band

The caps above are today's values, not constants of nature —
`MAX_SEND_VIDEO_SLOTS` has already moved once, and the table above will move
again. Sizing may be a compile-time constant; **no structural or algorithmic
choice may depend on today's exact number.**

The design point is **up to ~100 concurrent streams per participant session**,
counting every stream on the data plane: upstream encodings and their simulcast
layers, downstream slots, RTX, probing, and data-channel streams — which are the
same unit and share the budget (§8). Requirements:

- per-packet work is **O(1) in stream count**, or a bounded scan whose cost is
  measured at the top of the band and asserted — never "N is small today";
- cost degrades no worse than linearly to ~100 streams, with **no cliff**;
- structures are sized from named constants, so raising a cap is a constant
  change and a re-run of the benchmark, not a redesign.

Beyond that band is out of scope by construction (§2): scaling past one node is
a separate protocol, and this crate is a single node's endpoint.

**The cliff is the failure mode to design against, not the constant factor.**
`UpstreamRouteTable` (`participant/core.rs:84`) was originally direct-mapped on
`ssrc % MAX_UPSTREAM_ENCODED_STREAMS`; because clients pick SSRCs at random,
among six streams the collision probability exceeded 90%, and every packet from
a colliding pair took the slow miss path. It is now a split key array plus
payload array, scanned four bytes per stream. That is the right answer at 8
entries. It is not obviously the right answer at 100, and the requirement is
that the choice be **re-derived from a measurement at the top of the band**
rather than inherited.

---

## 2. Non-goals

This is the list that makes the scope narrow. Each of these is work a general
RTC library must do and this one must not.

**Signaling and negotiation**
- Generating offers, or renegotiating as the offerer.
- SDP features outside what the answerer path needs: BUNDLE variants we do not
  negotiate, `a=inactive`/`sendrecv` media lines (already rejected at
  `negotiator.rs:164`), plan-B, non-`rtcp-mux`, non-`rtcp-rsize`.
- W3C `getStats()` shapes. Metrics are ours, and they are for operating a
  server, not for satisfying a browser API.

**ICE**
- Candidate gathering, STUN binding *requests* as a client, TURN allocation,
  mDNS candidate resolution, candidate pair prioritisation, nomination, or the
  controlling role. ICE-lite responds; it does not search.
- ICE restart as an initiator.

**Media**
- Sample mode in every form: packetizers, depacketizers, jitter buffer,
  playout, resampling, mixing, transcoding, FEC decode, codec-specific media
  handling beyond what forwarding needs.
- What forwarding *does* need, and is therefore in scope: H.264 keyframe and NAL
  boundary scanning (`rtp/h264.rs`), dependency-descriptor parsing
  (`pulsebeam-core/src/dd`), VLA parsing, and enough structure to renumber and
  reorder. Nothing deeper.
- Codecs beyond H.264 and Opus. Adding VP9/AV1 later is a codec-classification
  change, not an architectural one, and the design must not make it one.

**Policy**
- Bandwidth estimation, pacing, probing, layer selection, NACK/RTX policy,
  retransmission retention, subscriber allocation. All PulseBeam's (§11).

**Data**
- SCTP fragmentation and reassembly inside the SFU. Fragmentation is the
  application's, and the SFU forwards bounded units (§8).
- SCTP's ordering and reliability as the delivery guarantee for topic lanes.
  SCTP is hop-by-hop; the guarantee that matters spans the forwarding hop.

**Scale-out**
- Anything past one node. Inter-node forwarding is a separate protocol with its
  own design; this crate is a single node's endpoint and must not grow
  addressing, discovery, or transport for a second one.
- Consequently, stream counts stay in the §1.1 band. Structures do not need to
  hold at 10,000 streams, and paying for that generality here would be the
  opposite of the goal.
- Cross-*shard* handoff within one node stays in scope — it is fundamental to
  thread-per-core — but it is a constraint on ownership (§5), not a routing
  responsibility of this crate (§7).

**Platform**
- Any non-Linux target. Any fallback path for a kernel capability the server
  requires — check at startup and fail loudly (`docs/linux-only.md`).
- Async runtimes, threads, sockets, timers, or an internal executor. This crate
  performs no I/O and starts nothing.

---

## 3. Invariants

Non-negotiable. A change that violates one is a regression even if tests pass.

1. **No protocol component becomes a scheduler.** Components expose state
   transitions, outputs, readiness, and deadlines. The shard decides when they
   run.
2. **Forwarding preserves packet identity, provenance, and the original
   buffer.** Parsing produces structural views and offsets, not new semantic
   packet objects, unless materialization is explicitly required.
3. **All mutable RTC state belongs to one shard.** Cross-shard communication is
   an owned message handoff, never shared mutable state. No `Arc`, no locks, no
   atomics (`clippy.toml` enforces this).
4. **Established RTP/RTCP forwarding does not allocate after warm-up.**
5. **Telemetry is sampled and bounded.** Metrics must not turn the packet path
   into an event-generation or allocation path.
6. **A hostile packet allocates nothing and costs O(1).** Reaching an allocation
   or an unbounded loop from unauthenticated input is a security defect (§13).

---

## 4. Control plane and data plane

Negotiation produces a validated, immutable `NegotiatedSession` — not a live RTC
object. It carries only negotiated facts:

- media sections, directions, codecs, payload types;
- RTP extension mappings;
- ICE credentials and candidates;
- DTLS fingerprint and parameters;
- negotiated limits and feature flags.

The negotiator must not create a live session, start timers, allocate media
queues, or construct pacing, retransmission, or stream state.

The shard constructs `pulsebeam-rtc` from a `NegotiatedSession` plus shard-owned
runtime resources (buffer pool, clock, telemetry sink). All live state is
therefore created and destroyed by its owning shard, and the control/data
boundary is a type, not a convention.

SDP parsing, ICE admission, DTLS setup, signaling, statistics, and DataChannel
setup stay off the established forwarding path. They are allowed to allocate.

---

## 5. Ownership and representation

State is data-oriented and keyed by stable IDs:

- participants, media sections, SSRCs, tracks, encodings, and subscribers use
  stable numeric IDs or dense indexes;
- no per-packet string lookups, dynamic type maps, or scans over anything that
  grows with room, shard, or node size;
- hot media state is laid out separately from cold negotiation and control
  state, so a packet never pulls a `NegotiatedSession` cache line.

### 5.1 SSRC resolution

Resolving an SSRC to a stream is the most frequent lookup in the system — once
per ingress packet, and again per egress rewrite — so it gets its own
requirement rather than inheriting a general one.

**The goal is to avoid the lookup, not to make it fast.** The two directions are
not symmetric, and the asymmetry is the whole opportunity:

- **Egress SSRCs are ours to assign.** A stream index can be encoded directly
  into the SSRC, making resolution a mask and a shift with no table at all and
  no dependence on stream count. `PackedRoute` (`route.rs`) already does exactly
  this for routes — `shard(12) | slot(20)` — so the pattern is established, and
  the same care applies: an encoded field must be recycled behind an epoch so a
  delayed packet cannot land on a reused index.
- **Ingress SSRCs are chosen by the publisher**, so a table is unavoidable. Its
  cardinality is bounded by what was negotiated, and that bound is *enforced at
  admission*, not assumed — an SSRC beyond it is rejected and counted (§13).

For the ingress table:

- worst-case lookup is bounded and **independent of the values a client
  chooses**. A structure whose probe length or bucket depth is attacker-
  controllable is a denial-of-service surface, not just a slow path, and the
  `ssrc % N` cliff above is the in-tree example of getting this wrong;
- a single-entry "last resolved SSRC" cache in front of the table is worth
  measuring: media arrives in runs, and a hit costs one comparison. Measure the
  hit rate with realistic interleaving before relying on it — with many active
  streams the runs are short;
- the structure is chosen by benchmark at the top of the §1.1 band and the
  result is recorded, so the next person raising a cap knows what was measured
  and at what size it was true.

`Rc` is permitted only for immutable data shared within one shard, and is not
the default packet representation. It must never cross a shard boundary.

Packets that will be rewritten or encrypted require unique mutable storage.
Where several local outputs share cleartext bytes, they may share a pool-backed
immutable buffer until the first destination-specific mutation or crypto
operation, which requires a private output buffer.

Cross-shard transfer moves owned storage or performs an explicit copy into
destination-owned storage. It never transfers reference-counted ownership.

---

## 6. Packet buffers and parsing

The canonical representation is a pooled buffer plus a structural view. Ingress
views are **read-only**: one ingress packet feeds many outputs, so an exclusive
borrow of the ingress bytes is wrong by construction.

```rust
/// Parsed once at ingress; shared by every output that fans out from it.
struct RtpView<'a> {
    bytes: &'a [u8],
    payload: Range<u16>,
    /// Offsets into `bytes` for the extensions forwarding actually rewrites
    /// or reads. Indexed by a small enum, not by name, so adding one is not
    /// a struct change.
    ext: ExtOffsets,
    ssrc: Ssrc,
    seq: u16,
    ts: u32,
    marker: bool,
    pt: Pt,
}
```

Requirements:

- receive buffers come from an arena or pool and are lent to the parser;
- parsing operates directly on the received bytes, in place;
- structural fields and extension offsets are parsed **once** and cached on the
  view; downstream components reuse them;
- codec classification and dependency-descriptor parsing happen once, not
  repeated per layer or per subscriber;
- extension values never require per-packet `Arc<dyn Any>` materialization —
  this is a concrete cost in str0m today and its removal is a headline goal;
- malformed packets fail at the first boundary with an explicit error and a
  counter, never a panic and never a deep runtime failure;
- buffer offsets, lengths, and mutation ranges carry `debug_assert!` bounds
  checks (`CLAUDE.md`).

Egress is the mirror image: a private `&mut [u8]` from the pool, a header
rewrite plan derived from the ingress view, and in-place SRTP protection into
the same allocation.

The forwarding path must not convert bytes into a library packet, then into a
PulseBeam packet, then back into another library packet. That round trip is the
single largest structural cost in the current design. Protocol adapters may
materialize semantic values on cold paths or where a protocol genuinely
requires them.

---

## 7. Fanout

**This crate does not own fanout — it must not make fanout expensive.** Who
subscribes to what, and which shard a copy is destined for, is PulseBeam's
(`shard/router.rs`, `route.rs`, and §11). What this crate owns is the API shape
that makes one-parse-many-writes possible:

```text
one ingress packet
    -> parsed and classified exactly once
    -> N independent egress rewrites, each with private mutable storage
```

- ingress parse cost is paid once, never per output;
- the parsed view is shareable by value and cheap to hand to each output —
  no re-parse, no reconstruction, no round trip back through a packet type;
- per-subscriber sequence, timestamp, SSRC, extension, and crypto work is
  explicit in the API and individually measurable, because it is the
  irreducible part (§9) and the only part that should scale with N;
- local immutable data may be shared; mutable output state is unique;
- a handoff to another shard moves owned storage and **never overwrites the
  original ingress timestamp** — provenance survives the boundary (§15);
- cache and replay history have explicit bounds and a single owner.

An API that forces the caller to re-enter a session object per destination has
already lost, however fast its internals are.

---

## 8. The data plane is RTP-shaped

Media and data are one plane, not two. Everything that crosses the data plane —
RTP, RTX, and DataChannel payloads alike — is the same unit:

```text
pooled buffer, at most one MTU
  + stream key
  + sequence
  + end-of-message
  + provenance (§15)
```

They differ in semantics and codec. They do not differ in shape. One pool, one
pacer, one estimator, one telemetry path, one cross-shard handoff.

**The SFU never reassembles.** It forwards bounded units and keeps a retransmit
cache; endpoints assemble messages from them, exactly as they already do for
video frames. This is the strong form of the invariant and it is worth stating
plainly: **no reassembly buffer exists anywhere in the forwarding path.** A
retransmit cache is RTX-shaped — indexed by sequence, bounded, droppable — and
is not a reassembly buffer.

Fragmentation therefore lives in the application layer, where RTP has always put
it. A large payload is chunked into MTU-bounded units by the publisher and
reassembled by the subscriber; the SFU in the middle sees only units. Large
payloads stay supported end to end, so this imposes **no product-visible size
cap**.

### 8.1 This formalizes what already exists

Both lanes are already RTP in all but representation:

- The `rt` lane requires `MaxRetransmits{retransmits: 0}` and `!ordered`
  (`track.rs:812-818`). SCTP contributes framing and nothing else.
- The `rel` lane already carries `RelMsg{stream_id, seq, payload}` and
  `RelNack{stream_id, from_seq}` (`pulsebeam-proto/proto/reliable.proto`), with
  a retransmit buffer, a reorder buffer, and resync-on-overflow in
  `pulsebeam-agent/src/agent/ordered_topic.rs`. That is RTP plus RTCP NACK plus
  RTX under other names.

That layer is not redundant — it is the only one that can work. SCTP is
hop-by-hop, and the SFU is a forwarding hop, so SCTP cannot recover a message the
SFU itself dropped. End-to-end recovery has to live above it.

Which makes **SCTP's own reliability the redundant copy**, and not a free one:
it head-of-line blocks the `rel` lane, runs a second retransmission timer and a
second congestion controller over the same bytes, and commits a large reassembly
buffer per association for messages we never send.

`RelMsg` gains one field — end-of-message — and becomes structurally identical to
an RTP packet. That is a `reliable.proto` change and a wire-contract change for
`pulsebeam-agent`, so it is staged (§17).

### 8.2 SCTP is framing

All lanes negotiate unordered and unreliable. Delivery guarantees come from the
application layer, because that is the only layer spanning the forwarding hop.
`max_message_size` is configured near one MTU, which means message interleaving
(RFC 8260 I-DATA) is defence in depth here rather than load-bearing — there are
no large messages left for it to interleave.

Oversized inbound messages remain possible from a non-compliant or hostile peer
and are rejected against an explicit bound, never reassembled (§13).

### 8.3 Why this keeps inter-node separable

Scaling past one node is a different protocol and out of scope (§2). This is what
keeps it *cheap* to build separately: it carries one shape, and it holds no
per-association, per-message, or reassembly state. A node forwards units it does
not have to understand, and a unit is self-contained by construction.

If data payloads could exceed an MTU, every one of those properties would have to
be rebuilt in the inter-node protocol — fragmentation, reassembly, and the
buffers and timeouts that come with them.

---

## 9. In-place mutation and crypto

Sequence numbers, timestamps, SSRCs, markers, extensions, TWCC values,
dependency descriptors, SRTP and SRTCP all operate on existing or pool-provided
buffers.

Crypto APIs accept caller-owned input and output storage, or use a reusable
per-shard scratch buffer. **A protected packet must not require an unavoidable
allocation followed by another copy into the socket batch** — protection writes
into the buffer that the `sendmmsg` batch already owns.

Per-destination encryption is expected and irreducible. The design shares
cleartext work up to that boundary and makes the remaining per-destination cost
explicit in the API, so it can be measured rather than assumed.

Cryptographic primitives are taken, not written: an audited backend
(`aws-lc-rs`) supplies AES-GCM/AES-CTR and HMAC. What we own is the SRTP
*framing* — key derivation, index reconstruction, replay window, and where the
bytes live.

---

## 10. Scheduling and readiness

Components expose explicit readiness and deadlines:

- input is available;
- output can be produced;
- output is blocked by socket or queue capacity;
- a protocol timer is due;
- a component needs a send opportunity;
- a component is closed or requires control-plane action.

The shard owns one timer system. There are no periodic participant scans, hidden
timer heaps, internal pacers, or mandatory mutate-then-drain loops.

Components accept bounded input batches and produce bounded output batches per
drive call. Processing one packet must never require draining an unrelated
global state machine — servicing ICE, DTLS, SRTP, RTP/RTCP, and SCTP must be
independently drivable.

Idle participants consume zero CPU except when an explicit deadline is due. A
participant with no traffic and no due timer must not be visited at all.

**The deadline a component reports is a contract.** The shard runs it to that
deadline; a component that under-reports stalls, and one that over-reports
spins. Both are defects, and both are assertable in simulation.

---

## 11. Policy and protocol

PulseBeam owns SFU policy: bandwidth estimation and GCC, pacing and probing,
subscriber allocation, simulcast and layer selection, NACK/RTX policy,
retransmission retention, fanout and scheduling decisions.

`pulsebeam-rtc` owns wire protocol mechanics: ICE-lite and connectivity checks,
DTLS state transitions, SRTP/SRTCP protection, RTP/RTCP parse and serialize,
TWCC and feedback encode/decode, SCTP transport mechanics.

Protocol components may report feedback and capabilities. They must not silently
introduce their own pacing, estimation, retransmission history, or scheduler.
Every queue, packet history, and policy decision has exactly one owner.

### The congestion-control contract

This boundary has been the source of most of the friction with str0m, so it is
specified rather than left to emerge:

- Every egress packet is issued an opaque, monotonic `SendId` at the point the
  library hands it to the caller.
- The caller reports actual departure: `report_sent(SendId, Instant)`. A
  sans-IO library cannot know when a packet left the wire; it must accept it.
  This is libwebrtc's own two-phase shape (`TransportFeedbackAdapter::AddPacket`
  then `ProcessSentPacket`).
- Inbound TWCC is parsed into `(transport_seq, arrival_time)` records and joined
  against departure times the library recorded. The join, not a guess, produces
  the estimator's input.
- **Feedback naming a packet with no recorded departure time is dropped from the
  estimator input**, with a counter. A sample we cannot trust does not enter the
  estimator. Never substitute a provisional or enqueue timestamp.
- Outbound TWCC (what publishers' own BWE measures) is stamped from the kernel
  receive timestamp, not from the tick clock.
- **Data-channel output goes through the same pacer and is measured by the same
  estimator as media.** It is the same unit on the same link (§8), so a separate
  path for it is a second pacing owner by another name — which invariant §3.1
  already forbids. Where SCTP carries its own congestion control, it is bounded
  so that ours remains the one that decides.

The last point rules out a live defect rather than a hypothetical one:
`str0m/src/lib.rs:1814` tries `self.dtls.poll_packet()` **before**
`self.session.poll_datagram()`, so data leaves ahead of media, unpaced, with no
TWCC sequence number — invisible to the estimator measuring the same link.

The library provides measurement. It never provides an estimate.

---

## 12. Batching, backpressure, and memory

Ingress and egress APIs are batch-oriented and shaped for `recvmmsg`/`sendmmsg`,
including GSO segmentation and GRO coalescing:

- receive buffers transfer from the socket layer without a per-datagram copy;
- egress buffers transfer directly into a bounded send batch;
- batching adds no unmeasured waiting time — a packet's time in a batch is
  observable;
- UDP and TCP output readiness are separately explicit;
- send failure, queue overflow, and backpressure produce explicit outcomes, not
  silent drops.

Every queue has an explicit capacity, owner, and drop policy. Bounds are
required for: ingress, egress, pacing, retransmission, TWCC, NACK, DTLS, SCTP,
control events, and the packet pool. Queue age and occupancy are observable
through aggregate metrics.

For SCTP these are configuration rather than something to build: `dcsctp`
exposes `max_send_buffer_size`, `max_receiver_window_buffer_size`, and
`max_message_size` directly. Setting the last near one MTU (§8) is what retires
the 256 KB per-association reassembly commitment that
`str0m/src/sctp/mod.rs:33`'s `LOCAL_MAX_MESSAGE_SIZE` advertises today.

Steady-state memory per participant must be computable from the §1 caps and
asserted in a test. An SFU that cannot state its per-participant footprint
cannot be capacity-planned.

---

## 13. Security and abuse resistance

The server is internet-facing and every ingress packet is hostile until DTLS
says otherwise. Narrow scope does not narrow the threat model.

- **Pre-consent work is O(1) and allocation-free.** Classification, STUN parse,
  and ufrag decode are already bounded and verifier-safe
  (`pulsebeam-routing/src/classify.rs`); the library's own pre-consent path must
  meet the same bar.
- **No amplification.** Before consent, the response to a packet must not exceed
  the request in size. STUN error and DTLS `HelloVerifyRequest` paths are the
  ones to check.
- **ICE consent freshness (RFC 7675).** A flow whose consent expires stops being
  served and releases its resources on a bounded schedule.
- **SRTP replay protection** with an explicit window, and authentication before
  any state mutation. A packet that fails auth must not have advanced a
  sequence, a cache, or a counter that an attacker can observe.
- **Bounded handshake resources.** DTLS retransmission, fragment reassembly, and
  half-open connection counts are capped per participant and per source, with
  the cap being a rejection rather than an allocation.
- **Per-participant hard caps** on streams, SSRCs, channels, and queue depth, so
  one participant cannot degrade a shard. The cap on distinct SSRCs is what
  makes the §5.1 ingress table's bound real; it is enforced at admission and a
  packet beyond it is dropped and counted, never admitted "just this once".
- **No attacker-controllable worst case in a per-packet lookup.** A client picks
  its own SSRCs, so any structure whose cost depends on key *values* — probe
  length, bucket depth, modular collision — is chosen by the attacker rather
  than by us. Either the worst case is bounded for all inputs, or the hash is
  keyed per session with a seed the client cannot observe.
- **Fuzzing is a requirement, not a nicety.** RTP, RTCP, STUN, SRTP, DD, and VLA
  parsers each carry a fuzz target and a corpus. These are the functions reached
  by unauthenticated input.

---

## 14. Time, simulation, determinism

Components use PulseBeam's runtime clock and the simulator shim. They must not
create independent wall-clock loops or hidden time sources.

RNG is not a constructor dependency. Components use the process-wide RNG shim
(`pulsebeam-simulator` shims `getrandom` and `clock_gettime`) and must not
create independent nondeterministic state merely to satisfy an API. This is a
concrete papercut with sans-IO crates that take an RNG parameter; wrap it once
at the boundary rather than threading it through.

State transitions, deadlines, buffer bounds, route generations, and crypto
lengths carry defensive assertions so simulation fails early (`CLAUDE.md`).

The whole crate must be drivable from `pulsebeam-simulator` with no I/O, and
must be **bit-deterministic at a given seed**. A run that diverges between two
executions of the same seed is a defect in this crate.

---

## 15. Telemetry

Every ingress datagram gets a cheap, stable `PacketId` carrying minimal
provenance: ingress receive timestamp, source and destination, transport and ECN
metadata, owning shard and participant, packet type, stable stream identifiers,
and an optional sampling token.

Fanout outputs and cross-shard messages retain their parent identity.
Retransmissions identify the original packet where one exists.

Full timestamp capture is **sampled**. The sampling decision is cheap and
allocation-free — a deterministic function of `PacketId`, or a runtime-configured
sampler — and requires neither an RNG call nor a per-packet metric object.
Traces land in a fixed-capacity per-shard ring; when it is full the trace is
dropped and the media packet is not.

A sampled trace records:

```text
ingress_at
owner_at
parsed_at
forwarding_ready_at
pacing_eligible_at
pacing_released_at
send_queued_at
send_submitted_at
send_completed_at   (kernel TX timestamp, when available)
```

These must separate:

- SFU processing — ingress → forwarding-ready;
- SFU queueing — forwarding-ready → pacing-eligible;
- pacing delay — pacing-eligible → pacing-released;
- egress batching and socket delay — pacing-released → send-submitted;
- SFU forwarding latency — ingress → send-submitted;
- network latency — send-submitted → remote receive, where TWCC makes it
  observable;
- end-to-end latency — sender ingress → remote receive.

Being able to say *"this delay is ours, this delay is the path's"* is the whole
point; a design that cannot separate them cannot be tuned.

Unsampled packets get aggregate counters and bounded histograms only. Trace
fields, labels, formatting, and export never run on the hot path. Drops record
stage and reason through low-cost counters, with sampled queue age where
available.

The socket layer returns the `SendId` of §11 so the shard can record submission
time. Kernel or wire completion timestamps are an *additional* signal, never a
replacement for submission time.

Telemetry beyond aggregate counters is compile-gated, and a build with it
disabled must show no measurable hot-path cost.

---

## 16. Interop

Scope narrowing applies to what we **emit**, never to what we **accept**.
Browsers send what they send, and an SFU that rejects a legal packet is broken
regardless of how clean its internals are.

- The receive path accepts the full range Chrome, Firefox, and Safari emit for
  the negotiated codecs and extensions, including padding, header extensions we
  do not use, RTX, redundant audio, and RTCP compound packets carrying reports
  we ignore.
- Unknown-but-legal input is ignored and counted, never treated as an error.
- Interop is a test obligation. `pulsebeam-testdata` holds browser-derived SDP
  and encoded H.264 today; it must also hold **recorded RTP/RTCP captures from
  each browser**, replayed through the parse and forward path as part of the
  suite. Capturing them is part of this work, not a prerequisite someone else
  supplies.
- Anything genuinely unsupported fails at negotiation, where a human can read
  the error — never mid-session.

---

## 17. Staging and parity

This replaces a working dependency, so it lands incrementally behind the §4
boundary rather than in one cut.

- `NegotiatedSession` is introduced **first**, against str0m, with no behaviour
  change. It is the seam everything else moves through.
- Subsystems are swapped one at a time — parse/rewrite, then SRTP, then ICE-lite,
  then DTLS and SCTP — each with the full suite green before the next.
- **Differential testing against str0m** is the primary correctness gate while
  both exist: the same input to both implementations must produce the same
  wire bytes, or a documented, justified difference.
- The simulation suite is the acceptance gate at every step. Per `CLAUDE.md`, a
  red simulation plan is assumed to be a production bug, and a seed-dependent
  failure is signal, not flake.
- Each step is independently revertable and leaves the tree shippable.

Lower-level pieces are taken rather than written where the risk is asymmetric:
`dimpl` for sans-IO DTLS, `aws-lc-rs` for primitives, and **`dcsctp` for SCTP**,
replacing str0m's `sctp-proto`.

`dcsctp` (Apache-2.0, crates.io) is by the same authors as the C++ dcSCTP that
ships in Chrome, so its behaviour is what browsers actually expect. It is also a
better fit for the requirements above than a generic SCTP: `SocketEvent` is
poll-shaped, `SocketTime` makes the clock injectable (§14), the buffer bounds of
§12 are configuration rather than construction, and
`SocketEvent::OnLifecycleMessageFullySent(LifecycleId)` is the native form of the
`SendId` / `report_sent` contract in §11 — the hook that had to be added to str0m
by hand.

Two known gaps, recorded rather than assumed away:

- **`SocketEvent::SendPacket(Vec<u8>)` allocates per packet**, which conflicts
  with invariant §3.4. Needs either an upstream change or a pooling wrapper at
  the boundary; it is not resolved by adopting the crate.
- **0.1.14 and pre-1.0**, roughly two thirds documented. The provenance is
  strong and the API surface is small, but this is a real maturity risk and the
  differential testing in this section is what covers it.

Staging, matching this section's revertability rule:

1. adopt `dcsctp` behind the existing boundary with no behaviour change;
2. negotiate all lanes unordered and unreliable, so SCTP becomes framing (§8.2);
3. add end-of-message to `RelMsg` and move fragmentation to the application
   layer. This is a `pulsebeam-proto/proto/reliable.proto` change and a wire
   contract change for `pulsebeam-agent`, so it lands last and on its own.

---

## 18. Acceptance criteria

Ready only when it demonstrates, with measurements committed to the tree:

**Allocation and ownership**
- zero steady-state heap allocations on established RTP/RTCP forwarding after
  warm-up, asserted by a test and not merely profiled;
- no cross-shard reference counting on the normal path;
- per-participant steady-state memory is computable from §1 and matches a test.

**Complexity**
- no per-packet scan over all participants or subscribers, and no per-packet
  work that grows with room, shard, or node size;
- **per-packet cost is measured at both ends of the §1.1 band** — a handful of
  streams and ~100 — and the curve between them is recorded. A structure that
  is only ever benchmarked at its current cardinality has not been shown to
  scale, it has been shown to work today;
- adversarial SSRC selection does not change those numbers (§13);
- no O(all-outgoing-streams) work per pacing timer event;
- fanout cost is linear in output count with a measured per-output constant;
- idle participants require no polling beyond their explicit deadlines.

**Correctness**
- property tests for packet identity, timestamp preservation, buffer ownership,
  bounded queues, fanout correctness, and state-machine validity;
- differential parity against str0m for the paths that replace it (§17);
- browser-recorded traffic replays clean (§16);
- fuzz targets for every parser reachable from unauthenticated input (§13);
- deterministic simulation on the existing clock and RNG shims, bit-identical
  across runs at a fixed seed.

**Observability**
- explicit queue age, drop reason, and backpressure metrics;
- sampled traces that separate SFU processing, pacing, batching, socket, and
  network delay (§15);
- telemetry-disabled builds show no measurable hot-path cost.

**Performance**
- benchmarks for high stream count, high fanout, cross-shard forwarding, and
  sampled vs disabled telemetry;
- per-packet forwarding cost and per-subscriber fanout cost are stated as
  numbers, tracked over time, and regressions are visible.

The performance requirement is **not** "zero copies under every circumstance."
It is that every copy, allocation, queue, scan, and per-destination operation is
intentional, bounded, measurable, and located at the correct ownership or crypto
boundary.

---

## Open questions

Recorded so they are decided deliberately rather than by accident.

- **DTLS role.** ICE-lite plus answerer suggests `a=setup:passive` and a
  server-only DTLS implementation, which would remove the client handshake path
  entirely. Confirm against what Chrome, Firefox, and Safari actually offer
  before treating it as scope reduction.
- **`is` (str0m's ICE crate) vs our own ICE-lite agent.** ICE-lite is small and
  we already own STUN parsing for eBPF (`pulsebeam-routing/src/stun.rs`).
  Writing it removes the last dependency on str0m's connectivity model; taking
  it saves interop debugging. Decide before §17 reaches ICE.
- **Where RTX and NACK response live.** str0m owns the TX buffer today while
  PulseBeam keeps its own cache for switching (`rtp/cache.rs`). Two histories is
  one too many, and §3.5 says every history has one owner — but which side owns
  it follows from the §11 split and has not been settled.
- **Whether crypto stays `aws-lc-rs`.** It is the right default. The question is
  only whether the SRTP framing layer wants a narrower interface than the one
  str0m's backend traits expose.
- **`dcsctp` association handover.** `HandoverError` / `RestoreError` suggest an
  association can be serialized and restored elsewhere. Whether that is the right
  primitive for moving a participant between shards deserves its own
  investigation; do not fold it into this work.
- **RFC 9653 zero-checksum** (`zero_checksum_alternate_error_detection_method`)
  skips CRC32c over every SCTP packet, which is per-packet CPU on the data path.
  Both peers must agree. Measure what it actually saves before enabling it by
  default.
- **Whether `max_message_size` near one MTU trips an interop floor.** RFC 8831
  may assume a 64 KB minimum that browsers rely on. Verify against real browsers
  before depending on it — §8's application-layer fragmentation means
  correctness does not rest on this, only defence in depth.
