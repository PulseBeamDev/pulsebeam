# pulsebeam-rtc requirements

`pulsebeam-rtc` is the shard-local RTC data plane for PulseBeam. It may retain
and reuse suitable lower-level protocol code from str0m, but its ownership,
scheduling, buffering, and policy boundaries must be designed for PulseBeam.

The goal is a high-throughput SFU data plane with predictable latency, bounded
memory, and enough sampled packet telemetry to separate SFU overhead from
pacing, socket, and network delay.

## Core invariants

These are non-negotiable:

1. No protocol component becomes a scheduler. Components expose state
   transitions, outputs, readiness, and deadlines; the shard runtime decides
   when they run.
2. Forwarding preserves packet identity, provenance, and the original packet
   buffer. Parsing produces structural views and offsets rather than new
   semantic packet objects unless materialization is explicitly required.
3. All mutable RTC state belongs to one shard. Cross-shard communication is an
   owned message handoff, never shared mutable state.
4. Established RTP/RTCP forwarding does not allocate on the heap after warm-up.
5. Rich per-packet telemetry is sampled and bounded. Metrics must not turn the
   packet path into an event-generation or allocation path.

## Control-plane and data-plane separation

Negotiation must produce a validated immutable `NegotiatedSession`, not a live
RTC object. It contains only negotiated information, such as:

- media sections, directions, codecs, and payload types;
- RTP extension mappings;
- ICE credentials and candidates;
- DTLS fingerprint and parameters;
- negotiated limits and feature flags.

The negotiator must not create `Rtc`, start timers, allocate media queues, or
construct pacing, retransmission, or stream state.

The shard creates `pulsebeam-rtc` from `NegotiatedSession` and shard-owned
runtime resources. This makes the control/data boundary explicit and ensures
that all live state is created and destroyed by the owning shard.

SDP, ICE admission, DTLS setup, signaling, statistics, and DataChannel setup
must stay off the established RTP forwarding hot path.

## Ownership and representation

RTC state must be data-oriented and keyed by stable IDs:

- participants, media sections, SSRCs, tracks, encodings, and subscribers use
  stable numeric IDs or dense indexes;
- known hot-path lookups use direct indexing or bounded indexed tables;
- per-packet string lookups, dynamic type maps, and linear scans are avoided;
- hot media state is separated from cold negotiation and control state.

`Arc`, locks, atomics, and shared mutable state are not used in shard-local RTC
state. `Rc` is permitted only for immutable data shared within one shard. It is
not a default packet representation and must not cross a shard boundary.

Packets that will be rewritten or encrypted require unique mutable storage.
When several local outputs can share cleartext bytes, they may use an `Rc` or a
pool-backed immutable buffer until a destination-specific mutation or crypto
operation requires a private output buffer.

Cross-shard transfer moves owned packet storage or performs an explicit copy
into destination-owned storage. It must not transfer reference-counted packet
ownership between shards.

## Packet buffers and parsing

The canonical packet representation is an owned packet buffer plus a
structural view. For example:

```rust
struct RtpView<'a> {
    bytes: &'a mut [u8],
    payload_offset: u16,
    twcc_offset: Option<u16>,
    dependency_descriptor_offset: Option<u16>,
}
```

Requirements:

- receive buffers come from an arena or pool and can be lent to the parser;
- RTP/RTCP parsing operates directly on the packet buffer;
- structural fields and extension offsets are parsed once and cached;
- downstream components reuse parsed information;
- codec classification and dependency-descriptor parsing are not repeated by
  separate layers;
- extension values do not require per-packet `Arc<dyn Any>` materialization;
- malformed packets fail at the first boundary with explicit errors;
- buffer offsets, lengths, and mutation ranges are defensively asserted.

The forwarding path should not convert bytes into a str0m packet, then into a
PulseBeam packet, and then back into another str0m packet. Protocol adapters
may materialize semantic values on cold paths or when a protocol explicitly
requires them, but ordinary RTP forwarding uses views and offsets.

## In-place mutation and crypto

Sequence numbers, timestamps, SSRCs, markers, RTP extensions, TWCC values,
dependency descriptors, SRTP, and SRTCP must operate on existing or
pool-provided buffers wherever possible.

Crypto APIs must accept caller-owned input and output storage, or use a
reusable per-shard scratch buffer. A protected packet must not require an
unavoidable allocation followed by another copy into the socket batch.

Destination-specific encryption is expected when subscribers have different
SRTP contexts. The design should share cleartext work before that boundary and
make the required per-destination crypto cost explicit.

## Packet identity and sampled telemetry

Every ingress datagram receives a cheap stable `PacketId`. The packet carries
minimal provenance through the pipeline:

- ingress receive timestamp;
- source and destination addresses;
- transport and ECN metadata;
- owning shard and participant;
- packet type and stable stream identifiers;
- optional sampling token.

Fanout outputs retain their parent packet identity. Cross-shard messages retain
the same identity and provenance. Retransmissions identify the original packet
when one exists.

Full timestamp capture is sampled. Sampling must be decided with a cheap,
allocation-free operation, such as a deterministic packet-ID decision or a
runtime-configured sampler. It must not require an RNG dependency or per-packet
metric objects.

Sampled traces use a fixed-capacity per-shard ring or arena. If the trace store
is full, the trace is dropped; the media packet is not delayed or dropped for
that reason.

A sampled trace may record:

```text
ingress_at
owner_at
parsed_at
forwarding_ready_at
pacing_eligible_at
pacing_released_at
send_queued_at
send_submitted_at
send_completed_at (optional)
```

The measurements must distinguish:

- SFU processing: ingress → forwarding-ready;
- SFU queueing: forwarding-ready → pacing-eligible;
- pacing delay: pacing-eligible → pacing-released;
- egress batching/socket delay: pacing-released → send-submitted;
- SFU forwarding latency: ingress → send-submitted;
- network latency: send-submitted → remote receive, when receiver or TWCC
  timestamps make it observable;
- end-to-end latency: sender ingress → remote receive.

Only aggregate counters and bounded histograms are required for unsampled
packets. Rich trace fields, labels, formatting, and export work must not occur
on the hot path. Packet drops must record the drop stage and reason through
low-cost counters, with sampled queue-age information where available.

The socket layer must return a send token or packet identity so the shard can
record actual send submission time. If kernel or wire completion timestamps are
available, they are an additional metric, not a replacement for send
submission time.

## SFU fanout

Fanout is a first-class operation:

```text
one ingress packet
    -> zero or more local outputs
    -> zero or more remote-shard outputs
```

Requirements:

- fanout work is proportional to the number of outputs;
- the packet is parsed and classified once;
- output metadata is inherited without reconstructing the packet;
- subscriber-specific sequence, timestamp, SSRC, extension, and crypto work is
  explicit;
- local immutable data may be shared, but mutable output state is unique;
- remote transfer carries provenance, timing, route generation, and link
  sequence;
- no cross-shard handoff overwrites the original ingress timestamp;
- cache and replay history have explicit bounds and ownership.

## Scheduling and readiness

`pulsebeam-rtc` components expose explicit deadlines and readiness:

- input is available;
- output can be produced;
- output is blocked by socket or queue capacity;
- a protocol timer is due;
- a component needs a send opportunity;
- a component is closed or requires control-plane action.

The shard owns one timer system. There are no periodic participant scans,
hidden timer heaps, internal pacers, or mandatory mutate-then-drain loops.

Components should accept bounded input batches and produce bounded output
batches in one drive operation. Processing one packet must not require draining
an unrelated global RTC state machine.

Idle connections should consume effectively zero CPU except when their explicit
deadline is due.

## Policy and protocol separation

PulseBeam owns SFU policy:

- bandwidth estimation and GCC policy;
- pacing and probing;
- subscriber allocation;
- simulcast and layer selection;
- NACK and RTX policy;
- retransmission retention policy;
- fanout and scheduling decisions.

`pulsebeam-rtc` owns wire protocols and protocol mechanics:

- ICE and connectivity checks;
- DTLS state transitions;
- SRTP/SRTCP protection;
- RTP/RTCP parsing and serialization;
- TWCC and feedback encoding/decoding;
- SCTP transport mechanics.

Protocol components may report feedback and capabilities, but must not silently
introduce their own pacing, BWE, retransmission history, or scheduler. There
must be one owner for every queue, packet history, and policy decision.

ICE, DTLS, SRTP, RTP/RTCP, and SCTP must be independently drivable so servicing
one does not traverse unrelated media streams.

## Batching, backpressure, and memory

Ingress and egress APIs are batch-oriented and compatible with `recvmmsg` and
`sendmmsg`.

- receive buffers are transferred from the socket layer without an unnecessary
  per-datagram copy;
- egress buffers transfer directly into a bounded send batch;
- packet batching does not add unmeasured waiting time;
- TCP and UDP output readiness are explicit;
- send failure, queue overflow, and backpressure produce explicit outcomes.

Every queue has an explicit capacity, ownership, and drop policy. Bounds are
required for ingress, egress, pacing, retransmission, TWCC, NACK, DTLS, SCTP,
control events, and packet pools. Queue age and occupancy must be observable
through aggregate metrics.

## Time and simulation

RTC components must use PulseBeam’s existing runtime clock and simulator shim.
They must not create independent wall-clock loops or hidden time sources.

RNG is not a required constructor dependency. Components must use the existing
process-wide RNG/simulation shim and must not create independent nondeterministic
RNG state merely to satisfy an API.

State transitions, deadlines, buffer bounds, route generations, and crypto
lengths must use defensive assertions so simulation fails early on invalid
state.

## Acceptance criteria

The implementation is ready only when it demonstrates:

- no steady-state heap allocations for established RTP/RTCP forwarding after
  warm-up;
- no normal-path cross-shard reference counting;
- no per-packet linear scans over all streams, participants, or subscribers;
- no O(all-outgoing-streams) work for every pacing timer event;
- bounded memory under high stream counts and high fanout;
- explicit queue age, drop reason, and backpressure metrics;
- sampled packet traces that separate SFU processing, pacing, batching, socket,
  and network delay;
- idle participants requiring no polling beyond explicit deadlines;
- deterministic simulation using the existing clock and RNG shims;
- property-focused tests for packet identity, timestamp preservation, buffer
  ownership, bounded queues, fanout correctness, and state-machine validity;
- benchmarks covering high stream count, high fanout, cross-shard forwarding,
  sampled telemetry, and telemetry-disabled hot-path performance.

The performance requirement is not “zero copies under every circumstance.” It
is that every copy, allocation, queue, scan, and per-destination operation is
intentional, bounded, measurable, and located at the correct ownership or
crypto boundary.
