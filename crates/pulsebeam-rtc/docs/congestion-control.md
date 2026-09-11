# Congestion control and latency governance

Status: normative private implementation contract for `pulsebeam-rtc` version one.

This document defines the egress RTP congestion controller, latency governor,
media allocator, pacer, scheduler, probe manager, and DataChannel coordination.
It does not add public controller types. The only public inputs are the semantic
values in [`design.md`](design.md): `TimePoint`, `MediaPacket`, `FrameMetadata`,
`SenderPolicy`, DataChannel commands, network input, `poll`, semantic events, and
statistics.

The words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative.

## Normative hierarchy and pinned algorithm

The network-control core is a direct implementation of
[`draft-ietf-ccwg-rfc8298bis-screamv2-01`](https://datatracker.ietf.org/doc/html/draft-ietf-ccwg-rfc8298bis-screamv2-01),
pinned to revision `01`. The source artifact used by the implementation review
has SHA-256:

```text
7beb2ff371106fa81a1678607ab3a1eb439ac9b0843d0d7fb64dab671132667f
```

The review artifact is the 100,016-byte archive copy. Run
`scripts/verify-scream-profile.sh` to retrieve that exact URL and verify both
properties before reviewing controller changes.

A later Internet-Draft revision, RFC, Ericsson commit, or libwebrtc commit MUST
NOT silently change production behavior. Adopting one requires a new documented
profile and fresh trace/simulation/browser evidence.

Normative precedence is:

1. the ownership/invariants in `../README.md` and exact public contracts in
   [`design.md`](design.md);
2. explicit PulseBeam decisions and deviations in this document;
3. the pinned SCReAM v2 revision for the self-contained core;
4. the cited RTP, RTCP, feedback, priority, SCTP, and DataChannel RFCs;
5. pinned Ericsson and libwebrtc revisions as comparison oracles only.

Where the pinned draft gives an equation, ordering rule, or numeric constant, the
private `ScreamController` implementation MUST transcribe it unchanged unless the
complete deviation is listed below. The implementation keeps a source citation
beside each transcribed equation or constant. No undocumented local tuning
constant is allowed.

### Explicit PulseBeam deviations and extensions

PulseBeam intentionally adds or changes only these behaviors:

1. Browser transport-wide feedback is normalized into the draft's packet-feedback
   model. RFC 8888 is an alternative normalized input.
2. A private latency governor supplies an upper ceiling on SCReAM's native
   queue-delay target. It can only make the controller more latency-conservative.
3. A weighted max-min allocator divides governed RTP media-payload capacity among
   stable outbound senders. The pinned draft does not define this SFU layer.
4. A deadline-aware transport scheduler combines per-sender service, frame
   usefulness, RTX, protocol control, DataChannels, and padding.
5. A bounded probe manager uses ordinary padding on a negotiated media or RTX
   SSRC; it never creates an SSRC-zero source.
6. SCTP retains its own congestion controller. Its acknowledged/admitted transport
   load is coordinated once with media capacity and top-level scheduling, but
   SCTP packets never become SCReAM-acknowledged data units.
7. Public allocation and demand are expressed in RTP media-payload bits per
   second, while in-flight and pacing accounting use emitted transport bytes.

These decisions mean PulseBeam is **SCReAM-v2-derived**, not a promise of bit-for-
bit behavior with any libwebrtc or Ericsson build.

## Congestion-control decisions considered

These are the consequential alternatives considered before implementation. The SCReAM
core remains separately testable; PulseBeam-specific behavior is explicit rather than
being hidden as tuning.

| Topic | Decision | Alternatives considered | Why this direction |
| --- | --- | --- | --- |
| Normative controller | Implement pinned SCReAM v2 revision `01` as the core | libwebrtc normative; Ericsson normative; custom controller inspired by SCReAM | Gives one reviewable algorithm baseline. Other implementations are comparison evidence, not moving production semantics. |
| PulseBeam influence on SCReAM | Only cap the native queue-delay target: `effective = min(native, policy_ceiling)` | Replace SCReAM qdelay logic; mutate gains/windows from playout policy; never influence SCReAM | Preserves SCReAM self-containment while allowing the product to choose lower latency than its coexistence logic might otherwise tolerate. |
| Controller scope | One aggregate RTP controller per connection | Per-sender congestion controllers plus coupling | All bundled RTP shares one bottleneck; sender priority should divide capacity, not manufacture independent capacity estimates. |
| Feedback | TWCC baseline; RFC 8888 normalized alternative | REMB; fixed-rate fallback; simultaneous TWCC + RFC 8888 | Packet-level arrival/loss data fits SCReAM. One selected mode avoids conflicting acknowledgment domains. |
| RFC 8888 accounting | Maintain acknowledgment progression per SSRC | Invent one aggregate RTP sequence cursor | RFC 8888 reports RTP sequence progression per SSRC; aggregation would mis-acknowledge packets. |
| Sent timestamp | `Output::Transmit` commit time | Caller departure receipts; NIC/kernel timestamp feedback | Keeps public API small; controller uses the last time observable inside the sans-I/O engine and conservatively treats later caller failure as loss. |
| Latency input | `global_media_at` + receiver playout policy derive private usefulness | Treat raw playout max as network queue target; trust Absolute Capture Time | Global server time is comparable across nodes; playout intent should affect usefulness/headroom, not create bufferbloat or rely on endpoint clocks. |
| Latency curve | Saturating private operating-point curve | Linear mapping of full playout range to queue/recovery; expose expert knobs | Large playout budgets must stop relaxing network behavior; fixed policy keeps API semantic and testable. |
| Multi-sender allocation | Weighted max-min over governed media-payload demand | Strict priority; equal split; per-sender cwnd | Gives proportional fairness without starvation or reservations while keeping source/layer selection in the SFU. |
| Rate units | SFU-facing allocations use media payload; pacer/BIF use transport bytes | One generic bitrate/byte unit | Keeps allocations directly comparable to codec/layer rates while congestion accounting includes observable transport overhead. |
| SCTP | Keep SCTP congestion control; reserve/coordinate its service outside SCReAM | Count SCTP as SCReAM BIF; feed SACK into a new coupled controller | TWCC/RFC 8888 does not acknowledge SCTP. V1 avoids inventing a second research-grade coupled controller. |
| Probing | RTP padding on negotiated media/RTX SSRC | Synthetic SSRC zero; no active probing | Matches normal RTP sender identity and enables pre-media/application-limited capacity observations without a special source. |
| Application-limited behavior | Freeze unsupported growth, decay confidence, demand-aware bounded probes | Collapse estimate to media rate; grow without observations | Separates offered media rate from path capacity and avoids both needless collapse and evidence-free optimism. |
| ECN/L4S | Optional only with validated RFC 8888 ECN | Require L4S/ECN; force-enable from configuration | Baseline must work on ordinary Internet paths; inconsistent/bleached ECN must not compromise control. |
| Frame scheduling | Prefer whole unstarted-frame drops; started frame is not guaranteed completion | Packet-only FIFO; unbounded commitment once first packet sends | Minimizes decoder damage while preserving bounded queues and congestion safety. |
| Constants | Freeze `CongestionProfileV1`; changes require profile/doc evidence | Leave tuning qualitative or implementation-defined | Prevents implementation agents from silently choosing controller semantics and makes deterministic comparison meaningful. |

## Goals

- Keep interactive media near the live edge while remaining congestion safe.
- Preserve the SCReAM core as one independently testable network controller.
- Give each receiver a continuous, monotonic quality/latency policy without
  exposing queue targets or controller gains.
- Divide one safe connection envelope among senders according to SFU demand and
  relative priority.
- Support ordinary Chrome and Firefox packet feedback without modified clients.
- Remain responsive before media, during VBR/application-limited media, after
  pauses, and during feedback loss.
- Commit packets at `Output::Transmit` without a transmit-receipt lifecycle.
- Keep all state bounded, dense, and local to one `Connection`.
- Make every public rate and statistic unambiguous about payload versus transport
  units.

## Non-goals

- Controlling a browser's uplink encoder. Ingress feedback generation is separate
  from this egress controller.
- Selecting an SFU source, simulcast/SVC layer, or keyframe cache entry.
- Proving exact physical capture-to-render latency. Endpoint capture clocks,
  encode time, and render internals are not fully trusted or observable.
- Mapping the playout-delay maximum directly or linearly to a network queue.
- Depending on L4S, ECN, RFC 8888, synchronized endpoint clocks, Absolute Capture
  Time, or a modified browser for baseline operation.
- Treating TCP fallback as a separately configurable public congestion mode.
- Exposing SCReAM revisions, gains, histories, probe clusters, pacer factors, or
  queue targets through the public API.

## Fixed architecture

```text
sender policy + immutable packet global_media_at + frame metadata
                              |
                              v
                      latency governor
                              |
                per-sender operating points
                              |
                              +------ strict queue-target ceiling -----+
                              |                                         |
                              v                                         v
packet feedback ------> self-contained SCReAM v2 core ----------> safe RTP envelope
                              |                                         |
                              +----------------+------------------------+
                                               v
                                   media-capacity accounting
                                               |
                              desired rates + priority weights
                                               v
                                  weighted max-min allocator
                                               |
                     +-------------------------+-------------------------+
                     |                                                   |
                     v                                                   v
          deadline/frame-aware RTP scheduler                 bounded SCTP scheduler
                     |                                                   |
                     +------------------- top-level service -------------+
                                               |
                                               v
                                      Output::Transmit
                                  (irrevocable send commit)
```

One connection owns exactly one RTP path controller. Original media, RTX, FEC,
and RTP padding share its path state. There is no per-sender congestion window
and no coupled layer above several independent RTP estimators.

The latency governor is not a second bandwidth estimator. The allocator cannot
create capacity. The scheduler cannot exceed the controller's RTP send-window
rules merely because a frame has a deadline.

All parts are ordinary fields of one `Connection`; they do not own independent
threads, callbacks, or public timers. Their earliest internal deadline contributes
to the one `Idle.next_wakeup` returned by `poll`.

## Time-domain separation

The controller uses two intentionally separate domains:

- `TimePoint.monotonic` drives sent history, feedback intervals, RTT, queue-delay
  trends, SCReAM state, pacing, retransmission timers, probes, and wakeups.
- `TimePoint.global` and `MediaPacket.global_media_at` drive cross-node media age
  and receiver-specific usefulness.

SCReAM never uses `GlobalMediaTime` to estimate RTT or network queue delay. RTCP
Sender Reports never drive SCReAM. Conversely, an RTP packet's receiver deadline
is never derived from an endpoint's untrusted wall-clock epoch.

A runtime global-clock correction can make queued media older and therefore less
useful; it cannot increase the SCReAM congestion window or fabricate path
capacity.

## Exact units

Version one uses distinct internal newtypes even where their storage is the same:

```rust
struct MediaPayloadBytes(u32);
struct TransportBytes(u32);
struct MediaPayloadRate(u64); // bits/s
struct TransportRate(u64);    // bits/s
```

### Media payload

`MediaPayloadBytes` is the original encoded audio/video payload represented by an
RTP packet. It excludes the RTP header, RTP header extensions, SRTP tag, RTX
header, padding, RTCP, SCTP, DTLS, ICE, RFC 4571 framing, and outer network
headers.

`SenderPolicy.desired_bitrate`, governed sender demand, sender allocation,
allocation events, and SFU-facing admitted/transmitted media rates use
`MediaPayloadRate`.

Relative media priority is also enforced over media payload service, matching the
WebRTC media-priority contract. Retransmitted media is charged to the originating
sender by its recovered original media payload size; repair and transport overhead
are accounted separately.

### Transport bytes

`TransportBytes` is exactly `Transmit.payload.len()` at the public connection
boundary:

- for UDP, the complete emitted STUN/DTLS/RTP/RTCP/SCTP datagram payload;
- for ICE-TCP, the complete emitted RFC 4571 frame including its two-byte length.

It does not include IP, UDP, TCP, link-layer, tunnel, or kernel framing that the
sans-I/O crate cannot observe. The document therefore uses **transport bytes**,
not “wire bytes.”

SCReAM sent-unit size, RTP bytes-in-flight, pacer tokens, queue byte limits,
protocol/SCTP service, and padding use `TransportBytes` or `TransportRate`.

### Payload-to-transport conversion

Each sender maintains bounded EWMAs of:

```text
payload_efficiency = media_payload_bytes / emitted_rtp_transport_bytes
```

The initial value is `0.90`; the value is clamped to `[0.50, 0.99]` and updated
only from non-padding RTP packets after `Transmit` commit. A sender allocation
`A_payload` is converted to expected RTP transport service as:

```text
A_transport = ceil(A_payload / payload_efficiency)
```

This conversion is used for pacing and aggregate scheduler reservation. It does
not change the public allocation unit and it does not count the same header bytes
twice.

The connection-level aggregate efficiency is a transmitted-payload-weighted mean
of active senders, with the same initial value and bounds.

## Packet-feedback contract

Every accepted connection with outbound RTP negotiates exactly one packet-
feedback mode. The absence of both modes rejects the session. REMB is never an
egress fallback.

```rust
private enum PacketFeedbackMode {
    TransportWide,
    Rfc8888,
}
```

Both modes normalize to one private record:

```rust
struct PacketFeedback {
    sent_id: SentPacketId,
    received: bool,
    receiver_arrival: Option<ReceiverTime>,
    ecn: Option<EcnMark>,
}
```

`ReceiverTime` is a mode-specific relative timeline. Its absolute epoch is never
interpreted as server or endpoint wall time.

### Transport-wide feedback

- One 16-bit transport sequence number space covers all bundled outbound RTP,
  RTX, and probe packets.
- A sequence number is assigned only when the encrypted packet is selected for
  immediate `Transmit` emission.
- Wrap is unwrapped against bounded sent history. A feedback status outside the
  accepted generation/reordering window is unknown and counted.
- Receiver arrival deltas are reconstructed in feedback order, then associated
  with committed sent entries.
- Duplicate reports update no bytes twice.
- Feedback for a padding probe follows exactly the same path as media.

### RFC 8888 feedback

RFC 8888 acknowledgment progression is **per reported SSRC**. The normalizer keys
sent media by `(outbound_ssrc, extended_rtp_sequence_number)` and maintains a
separate unwrap/acknowledgment cursor per SSRC. It MUST NOT invent one aggregate
“highest acknowledged RTP sequence number” across SSRCs.

- Original media, RTX, and any distinct repair SSRC retain separate report state.
- Multiple report blocks in one RTCP feedback packet are normalized independently.
- A report for an unknown SSRC or sequence is ignored and counted without
  allocating state.
- ECN is usable only when this selected feedback format reports it consistently
  and path validation succeeds.

### Feedback hold and staleness

Where the feedback format permits it, the implementation separates receiver
feedback hold from the network loop:

```text
feedback_rtt = feedback_received_at - packet_committed_at
network_loop = max(0, feedback_rtt - receiver_feedback_hold)
```

Sparse or application-limited samples cannot rapidly rewrite either estimate.
Feedback staleness is evaluated from monotonic send/feedback time and can never
increase the target rate.

## `Transmit` is the sent-data-unit commit

There is no departure receipt.

When `poll(at)` returns `Output::Transmit`, the connection has already, in this
order:

1. selected one eligible packet under the current schedule and send window;
2. assigned and committed any RTP, RTX, RTCP, SCTP, and transport sequence values;
3. encrypted/framed the final transport payload;
4. inserted an RTP sent-history entry when applicable at `at.monotonic`;
5. increased RTP bytes-in-flight by the applicable transport size;
6. charged pacer tokens and sender service;
7. recorded forwarding latency through the commit point;
8. returned the immutable `Transmit` value.

The caller synchronously attempts socket submission before invoking the connection
again. A caller-side drop or failed syscall is indistinguishable from path loss;
the packet remains a committed sent unit until acknowledged or expired by the
normal SCReAM history rules. This is intentionally conservative and keeps the
public API small.

For ICE-TCP, commit means submission to the caller-owned TCP stream, not physical
network departure. Kernel/TCP queueing is part of fallback-path behavior. No TCP
receipt, rollback, or alternate public controller exists.

RTP sequence numbers can be committed before a later queue overflow or caller
failure. Such loss remains an RTP gap. A TWCC number is never assigned to a packet
that has not reached the immediate `Transmit` selection point.

## SCReAM v2 core

The private core accepts normalized sent/feedback facts and produces one safe RTP
envelope:

```rust
struct SafeRtpEnvelope {
    target_media_payload_rate: MediaPayloadRate,
    max_rtp_bytes_in_flight: TransportBytes,
    pacing_transport_rate: TransportRate,
    native_queue_delay_target: Duration,
    effective_queue_delay_target: Duration,
    queue_delay: Duration,
    queue_delay_confidence: Confidence,
    smoothed_rtt: Duration,
    feedback_hold: Duration,
    delivered_rtp_transport_rate: TransportRate,
    application_limited: bool,
    feedback_stale: bool,
}
```

This type is private. Statistics copy semantic scalar values; callers never receive
or mutate it.

### Inputs

The core consumes:

- committed RTP data-unit time, transport size, transport/RTP identity, and class;
- normalized packet arrival/loss and valid ECN marks;
- feedback receipt time and estimated receiver feedback hold;
- current RTP bytes-in-flight and paced RTP queue state;
- selected-path changes and network availability;
- aggregate offered/admitted RTP media-payload rate;
- aggregate SFU desired media-payload rate;
- the private PulseBeam queue-delay ceiling.

Per-sender priority, frame IDs, source selection, SCTP sequence/SACK state, and
raw playout ranges are not SCReAM inputs.

### Delay model

The delay estimator follows the pinned draft. Conceptually:

```text
relative_one_way_delay = receiver_arrival_time - sender_commit_time
queue_delay = relative_one_way_delay - rolling_path_baseline
```

Only differences and trends are meaningful across endpoint clock domains. The
implementation uses bounded rolling minima and drift/variation filters and never
interprets the absolute result as wall-clock latency. Reordering, batching,
wireless scheduling jitter, and gradual receiver clock drift are accepted within
the pinned algorithm's rules. A selected-path replacement invalidates the path
baseline.

### Reference and send windows

The draft's reference window is retained as its internal network-safety state. It
is not renamed or treated as an absolute public congestion window. The draft's
bounded slack between reference window, send window, pacing, and bytes-in-flight
is implemented exactly. No PulseBeam layer can bypass those rules.

The reference window grows only from delivered evidence and falls on the draft's
queue, congestion-loss, or valid ECN signals. Random isolated wireless loss is
not silently reclassified as sustained congestion, but repeated loss or an
overflowing queue cannot be filtered away.

The target bitrate is a controller output for RTP media payload. It is not a proof
of physical bottleneck capacity and is named `target_media_payload_rate` in local
code and statistics. “Sustainable path capacity” is not used as a synonym.

### Application-limited behavior

The connection is RTP application-limited when offered RTP payload remains below
`85%` of the governed aggregate media allocation for at least `200 ms` and no
eligible RTP packet is held back by pacing or the send window.

While application-limited:

- unsupported reference-window growth stops;
- sparse delay/RTT samples cannot rapidly move the baseline;
- the last credible rate is retained while confidence decays with a `5 s`
  half-life;
- demand-aware probes MAY test retained or higher capacity;
- returning media exits application-limited state immediately once offered load
  reaches the threshold.

Low-complexity VBR video, audio-only periods, paused video, and all-media-paused
periods can be application-limited. Media rate and available network rate are
never equated.

### Native and effective queue-delay targets

SCReAM computes its native queue-delay target using the pinned draft, including
its competing-flow adaptation. Revision `01` begins from the draft's recommended
`60 ms` target and can natively relax as far as `400 ms` where the algorithm
permits.

PulseBeam supplies one upper ceiling:

```text
effective_queue_delay_target =
    min(native_queue_delay_target,
        strictest_active_sender_queue_delay_ceiling)
```

The ceiling cannot increase the native target, reference window, send window,
pacing rate, or bitrate. It is the only latency-governor input to the SCReAM core.
All native gains, loss response, ECN response, baseline handling, and competing-
flow logic remain self-contained.

This deliberately trades some coexistence aggressiveness against loss-based bulk
traffic for PulseBeam's interactive latency bound. That product decision is an
explicit deviation, not hidden “SCReAM tuning.”

### ECN/L4S

Baseline version-one behavior uses delay and loss and MUST be excellent without
ECN. ECN is disabled unless:

1. RFC 8888 is the selected feedback mode and reports ECN marks;
2. the negotiated sender/path supports the required marking semantics;
3. validation traffic proves marks are preserved and internally consistent.

Bleaching, impossible transitions, or inconsistent counts disable ECN for the
path without disabling delay/loss control. No public switch can force ECN on.

## `CongestionProfileV1`

`CongestionProfileV1` is private and compile-time fixed. Every value below is
normative. A code change to one value requires a profile-version change or a
matching documentation revision with new evidence.

Where a field says **pinned draft**, its complete formula and constants come
unchanged from revision `01`; this is still an exact value by normative reference,
not permission for implementation choice.

### Core and feedback ledger

| Field | Version-one value |
| --- | ---: |
| SCReAM algorithm revision | `draft-ietf-ccwg-rfc8298bis-screamv2-01` |
| Initial RTP media-payload target | `300_000 bit/s`, capped by active desired rate |
| Minimum RTP media-payload target | `20_000 bit/s` while RTP demand is nonzero |
| Maximum RTP media-payload target | `100_000_000 bit/s` and active desired sum, whichever is lower |
| Initial smoothed feedback RTT | `100 ms` |
| Native initial queue-delay target | `60 ms` |
| Native maximum queue-delay target | `400 ms` |
| Reference-window update | pinned draft |
| Send-window/reference-window slack | pinned draft |
| Queue/loss backoff | pinned draft |
| ECN response | pinned draft, gated by validated RFC 8888 ECN |
| Clock-drift and path-baseline filters | pinned draft |
| Target-rate derivation | pinned draft |
| Normal pacing derivation | pinned draft, then bounded by the transport limits below |
| Sent-history entries | `32_768` |
| Sent-history maximum age | `5 s` |
| Maximum accepted ack reordering behind newest generation | `4_096` packets |
| Maximum statuses normalized from one feedback packet | `8_192` |
| Feedback stale threshold | `clamp(max(500 ms, 3 * srtt), 500 ms, 2 s)` |
| Confidence half-life while stale or application-limited | `5 s` |
| Feedback recovery requirement | one syntactically valid report covering one committed packet |
| Maximum expiration work in one `poll` | `256` entries; return immediate wakeup if more remains |

### Transport, queue, and scheduler ledger

| Field | Version-one value |
| --- | ---: |
| Initial payload efficiency | `0.90` |
| Payload-efficiency clamp | `[0.50, 0.99]` |
| Payload-efficiency EWMA time constant | `1 s` |
| Maximum paced RTP packets | `8_192` |
| Maximum private paced RTP transport bytes | `8 MiB`, additionally capped by public configured media bytes |
| Hard maximum pacer horizon | `100 ms` |
| Maximum retained RTX age | `2 s`, additionally capped by sender usefulness |
| Maximum retained RTX transport bytes | public retransmission limit |
| Protocol-control minimum service reserve | `max(16_000 bit/s, 1% of current total transport service)` |
| Scheduler quantum | one current path-MTU transport packet |
| Sender allocation hysteresis | `max(25_000 bit/s, 10% of previous allocation)` |
| Upward allocation confirmation | threshold held for `2` valid feedback rounds |
| Downward safety change | immediate |
| Upward operating-point relaxation | at most `10%` of remaining delta per smoothed RTT |
| Fixed-range (`min == max != 0`) relaxation | at most `5%` of remaining delta per smoothed RTT |
| Maximum frame dependencies | `8` |

### Probe ledger

| Field | Version-one value |
| --- | ---: |
| Concurrent probe clusters | `1` |
| Probe-history records | `64` |
| Minimum unmet-demand trigger | desired payload exceeds credible allocation by `20%` and `50_000 bit/s` |
| Cluster target | `min(total governed demand, max(2 * credible media rate, 300_000 bit/s))` |
| Cluster duration | `20 ms` |
| Minimum packets | `5` |
| Maximum packets | `32` |
| Maximum cluster transport bytes | `48 KiB` and current send window, whichever is lower |
| Minimum interval | `max(1 s, 4 * srtt)` |
| Rolling probe overhead | at most `5%` of emitted transport bytes over `5 s` |
| Success feedback deadline | `2 * srtt`, clamped to `[200 ms, 2 s]` |
| Successful observation | at least `80%` of probe bytes reported received with no SCReAM congestion response |
| Queue-growth abort | effective target exceeded or queue rises `10 ms` during cluster |
| Loss abort | `5%` of reported probe packets lost before completion |

### Latency-governor ledger

| Field | Version-one value |
| --- | ---: |
| Most urgent queue-delay ceiling | `15 ms` |
| Quality-saturated queue-delay ceiling | `60 ms` |
| Urgent playout-maximum knee | `75 ms` |
| Quality saturation playout maximum | `500 ms` |
| Urgent allocation utilization | `0.80` |
| Quality allocation utilization cap | `0.95` |
| Urgent pacer horizon | `15 ms` |
| Quality pacer horizon cap | `80 ms` |
| Audio receiver processing reserve | `10 ms` |
| Video receiver processing reserve | `25 ms` |
| Synchronized media-clock uncertainty reserve | `5 ms` |
| Provisional media-clock uncertainty reserve | `25 ms` |
| Unverified media-clock uncertainty reserve | `50 ms` |
| Discontinuous media-clock uncertainty reserve | `75 ms` |
| Minimum network uncertainty reserve | `5 ms` |
| Maximum network uncertainty reserve | `50 ms` |
| ASAP audio maximum age | `100 ms` |
| ASAP video maximum age | `150 ms` |
| ASAP new-packet pacer horizon | `15 ms` |
| Maximum extra quality-oriented RTX allowance | `50 ms` |

The profile is encoded as one private immutable value. Tests obtain it through a
crate-private accessor; production callers cannot select or mutate it.

## Latency governor

The governor converts each sender's public policy and packet media time into a
private stable operating point. It does not continuously retune SCReAM per packet.

```rust
struct SenderOperatingPoint {
    queue_delay_ceiling: Duration,
    allocation_utilization: Ratio,
    pacer_horizon: Duration,
    new_frame_horizon: Duration,
    rtx_extra_allowance: Duration,
    probe_queue_impact: Duration,
    governed_demand: MediaPayloadRate,
}
```

### Active sender definition

A sender participates in strict path-level aggregation while any is true:

- `desired_bitrate > 0`;
- it has queued or committed media;
- it has useful retained retransmission work;
- it has an active probe caused by its unmet demand.

A sender with zero desired rate and no remaining work is inactive and cannot hold
the path at its latency ceiling indefinitely.

### Operating-point curve

For `playout_max > 0`, define:

```text
x = clamp((playout_max - 75 ms) / (500 ms - 75 ms), 0, 1)
```

Then:

```text
queue_delay_ceiling  = lerp(15 ms, 60 ms, x)
allocation_utilization = lerp(0.80, 0.95, x)
pacer_horizon        = lerp(15 ms, 80 ms, x)
rtx_extra_allowance  = lerp(0 ms, 50 ms, x)
probe_queue_impact   = lerp(5 ms, 20 ms, x)
governed_demand      = desired_bitrate * allocation_utilization
```

Interpolation uses integer fixed-point arithmetic with round-to-nearest ties-to-
even. Values above `500 ms` saturate and never authorize larger network queues.
Values at or below `75 ms` select the urgent endpoint. `0/0` bypasses the curve
and selects the urgent endpoint plus the ASAP rules below.

`playout_min` does not alter SCReAM's queue target or create network capacity. It
controls recovery classification and stability:

- predicted delivery before the minimum is comfortably recoverable;
- delivery between minimum and maximum has linearly decaying utility;
- delivery after the maximum has zero utility;
- a fixed nonzero range requests stable playback and uses the slower upward
  relaxation in the profile.

The strictest `queue_delay_ceiling` and `probe_queue_impact` among active senders
become the path values. Utilization, demand, frame/RTX usefulness, and service
balance remain per sender.

### Receiver-specific deadline derivation

`MediaPacket.global_media_at` remains immutable and cluster-global. The egress
connection privately derives usefulness for the selected receiver.

For a nonzero playout maximum:

```text
receiver_reserve = media_kind_reserve + media_clock_uncertainty

network_prediction = srtt / 2
                   + current_queue_delay
                   + clamp(max(5 ms, 2 * delay_variation), 5 ms, 50 ms)

predicted_handoff = now_global
                  + predicted_pacer_wait
                  + serialization_time

predicted_receiver_arrival = predicted_handoff + network_prediction

comfortable_arrival = global_media_at
                    + playout_min
                    - receiver_reserve

latest_useful_arrival = global_media_at
                      + playout_max
                      - receiver_reserve
```

All arithmetic is checked. Underflow makes the deadline already expired; overflow
saturates to the maximum representable global time and is counted.

A new frame is admitted only when its predicted receiver arrival is no later than
`latest_useful_arrival` and its predicted pacer wait is no longer than the current
sender pacer horizon. An RTX is admitted only when its predicted receiver arrival
has positive utility and fits the SCReAM send window.

This is a best-effort normalized media deadline, not proof of physical
capture-to-render latency. Endpoint-to-first-edge uncertainty is already part of
the server-anchored timeline and is reported separately.

### `0/0` ASAP policy

`0/0` does not construct `global_media_at + 0` as a literal deadline. It means:

- do not queue a newly admitted packet longer than `15 ms`;
- drop audio older than `100 ms` and video older than `150 ms` at admission;
- retransmit only when immediate prediction says it can arrive before newer
  replacement media and within the same age limit;
- use the `15 ms` queue-delay ceiling and `0.80` allocation utilization;
- never wait to fill a smoothing interval.

### Dynamic policy changes

Tightening takes effect in the same `Command::SetSenderPolicy` call:

- the new playout extension value is scheduled for signaling;
- private deadlines are recomputed from each queued packet's immutable
  `global_media_at`;
- not-yet-started stale video frames are removed whole;
- obsolete RTX is removed;
- governed demand, pacer horizon, and allocation may fall immediately;
- the path queue-delay ceiling tightens immediately when this sender becomes the
  strictest active sender.

Relaxing follows the profile's per-RTT damping. It cannot release a burst, jump
the SCReAM reference window, trust a stale estimate, or reconstruct dropped
media.

### Required monotonicity

For identical network, packet, and other-sender state, reducing one sender's
playout maximum MUST NOT:

- increase its or the path queue-delay ceiling;
- increase its allocation utilization, governed demand, pacer horizon, or RTX
  allowance;
- admit an older frame rejected by the looser policy;
- permit a probe with greater predicted queue impact;
- increase SCReAM's native target or reference window.

Increasing a maximum may relax only that sender's private limits and only through
damping. It cannot relax a path ceiling still required by another active sender.

## Global media time and frame admission

The global timeline contract is defined in [`design.md`](design.md). Congestion-control code
uses these additional invariants:

- all packets of one frame have identical `global_media_at`;
- a transit packet's value is never changed;
- source switching does not reset congestion state or sender policy;
- a packet mapping behind an already committed outbound frame is stale rather
  than an excuse to rewind RTP time;
- RTP clock discontinuities are ingress clock-mapper events, not congestion
  events;
- global-clock uncertainty affects usefulness reserves, never network capacity.

The scheduler prefers dropping a complete not-yet-started frame. Cut-through
forwarding means a started frame is not an unbounded commitment: later safety
pressure may drop a remaining packet, leaving a visible RTP gap and a
post-admission-damage counter.

When dependencies are known, dropping a required frame marks queued dependents
ineligible until a random-access or otherwise independent frame restores the
chain. `Unknown` dependencies permit only whole-current-frame decisions; the
connection does not invent dependency relationships.

## Media-capacity accounting and SCTP coordination

SCReAM's target is the upper bound for RTP media payload. SCTP has its own
acknowledgments and congestion window. Version one coordinates them without
pretending that RTP feedback acknowledged SCTP bytes.

The connection maintains:

```text
rtp_safe_transport_rate =
    target_media_payload_rate / aggregate_payload_efficiency

observed_path_transport_rate =
    delivered_rtp_transport_rate
  + delivered_sctp_transport_rate
  + delivered_protocol_control_transport_rate

reserved_sctp_transport_rate =
    max(sctp_acknowledged_rate_ewma,
        bounded_immediately_admissible_sctp_rate)

reserved_control_transport_rate =
    max(16_000 bit/s, 1% of current total transport service)
```

When `observed_path_transport_rate` has at least one RTT of credible samples:

```text
observed_media_transport_cap =
    max(0,
        observed_path_transport_rate
      - reserved_sctp_transport_rate
      - reserved_control_transport_rate)

media_transport_cap =
    min(rtp_safe_transport_rate,
        observed_media_transport_cap)
```

Before that observation is credible, `media_transport_cap` is
`rtp_safe_transport_rate`; SCTP remains bounded by its own controller and the
scheduler's queue limits.

Finally:

```text
available_media_payload_rate =
    media_transport_cap * aggregate_payload_efficiency
```

This construction reserves SCTP exactly once. SCTP transport bytes are never
inserted into SCReAM sent history, never increase SCReAM RTP bytes-in-flight, and
never appear as RTP delivery. Their queueing can still increase RTP delay and
therefore cause a normal SCReAM response.

A data-only connection uses SCTP congestion control and the bounded DataChannel
scheduler; the RTP SCReAM core remains unproven/inactive until RTP demand exists.

## Weighted media allocation

On a material safe-rate, demand, policy, or activity change, the allocator:

1. computes `available_media_payload_rate`;
2. computes each active sender's governed demand from the latency governor;
3. distributes capacity with weighted max-min fairness, capped by governed demand;
4. redistributes capacity unused by paused, demand-capped, or application-limited
   senders;
5. applies the fixed event hysteresis without delaying downward safety changes.

### Exact water-filling algorithm

For active sender set `S`, capacity `C`, governed demand `d_i`, and positive weight
`w_i`:

```text
allocation_i = 0
remaining_capacity = C
U = S

while U is not empty and remaining_capacity > 0:
    share_unit = remaining_capacity / sum(w_j for j in U)
    capped = { i in U | d_i - allocation_i <= w_i * share_unit }

    if capped is empty:
        allocation_i += w_i * share_unit for every i in U
        remaining_capacity = 0
    else:
        for i in capped:
            grant = d_i - allocation_i
            allocation_i += grant
            remaining_capacity -= grant
            remove i from U
```

Arithmetic uses unsigned fixed-point bits per second; the final remainder from
rounding is distributed one bit/s at a time by ascending `SenderId`. This makes
simulation deterministic.

Weights express proportions, not guarantees. Two equally backlogged senders with
weights `1` and `4` converge to a 1:4 media-payload split while both remain
constrained. A capped sender returns unused capacity. A positive weight prevents
scheduler starvation while eligible, but expired media can still be dropped.

Priority changes apply at the next scheduling decision. Existing service balance
is normalized to at most one scheduler quantum in either direction, preventing a
promotion burst or unpayable demotion debt. Priority changes never alter SCReAM
state.

Setting desired bitrate to zero rejects new media admission for that sender.
Already queued, committed, or useful RTX work drains or is shed under existing
deadlines. Once no work remains, the sender becomes inactive for path latency
aggregation.

## Pacer and top-level scheduler

The pacer is work-conserving only inside all active safety constraints:

- SCReAM send-window/reference-window rules;
- transport pacing rate;
- path and per-sender pacer horizons;
- sender allocation/service balance;
- frame and RTX usefulness;
- protocol and DataChannel bounds.

### Service classes

Selection has four bounded classes:

1. ICE, DTLS, SCTP association control, and RTCP required for liveness or
   congestion feedback;
2. governed user traffic, consisting of an RTP lane and an SCTP-user-data lane;
3. non-urgent reports/control;
4. RTP padding probes.

Class 1 can preempt user traffic but normally consumes only the fixed control
reserve. Class 4 is selected only when every real-traffic lane is empty or blocked.

The two class-2 lanes are **not** strict-priority ordered. A transport-byte deficit
scheduler gives the RTP lane service at `media_transport_cap` and the SCTP lane
service at `reserved_sctp_transport_rate`, subject respectively to the SCReAM send
window and SCTP congestion window. An eligible nonempty lane receives at least one
path-MTU quantum during each `max(100 ms, srtt)` interval, unless its own
congestion controller or deadline makes every packet ineligible. Borrowed idle
service is charged to the borrowing lane's deficit.

Within the RTP lane, one media-payload deficit is maintained per `SenderId`.
Original media, RTX, and FEC are charged to that sender. Packet deadlines determine
eligibility before deficit selection. A sender can borrow otherwise idle service,
but borrowed payload is charged to its balance and cannot become a permanent
priority boost.

Within the SCTP lane, `dcsctp` applies the negotiated RFC 8260 scheduler and the
public 16-bit DataChannel weights. SCTP user data cannot bypass its association
congestion window merely because the top-level lane has service.

The pacer charges final emitted transport bytes. Payload allocation and transport
pacing therefore remain consistent even when header/RTX overhead differs between
senders.

### Frame behavior

- A complete not-yet-started stale frame is removed atomically.
- A key/random-access frame is not immune to congestion safety; it receives only
  usefulness-aware preference.
- A started frame may be completed only while every next packet remains inside
  the send window, queue bound, and deadline. Unknown final frame size never
  creates an unbounded commitment.
- Padding disappears immediately when any eligible real traffic needs service.
- Protocol control remains deliverable under a large media keyframe.

## Probing

The probe manager is a PulseBeam extension around the self-contained SCReAM core.
It tests shared RTP path capacity only when greater capacity would be useful.

### Probe SSRC and packet form

- A probe uses ordinary RTP padding on an already negotiated outbound media SSRC
  or its negotiated RTX SSRC.
- RTX SSRC is preferred when its negotiated payload/extension form supports
  pure padding before original media; otherwise the media SSRC is used.
- The packet carries the selected feedback extension and ordinary outbound
  sequence continuity.
- No SSRC-zero entity, encoding, event, RTP stream, timestamp source, or keyframe
  state exists.
- Pre-media probing does not require a prior source RTP timestamp. Padding does
  not create or advance `GlobalMediaTime` or frame state.

With multiple candidates, the probe manager chooses the active sender with the
largest weighted unmet governed demand that has a valid padding form. Probe
results remain connection-level and are redistributed by the allocator.

### Eligibility and lifecycle

A probe may begin when:

- the path first becomes writable and RTP desired demand is nonzero;
- governed demand exceeds credible allocation by both fixed trigger thresholds;
- VBR or paused traffic has left capacity confidence decaying;
- all media is paused while nonzero desired demand remains;
- a congestion response has stabilized and evidence is needed before recovery.

The exact cluster size, interval, overhead, and success criteria are fixed in the
profile. One cluster aborts immediately on:

- effective queue target violation or the configured queue rise;
- configured probe loss;
- stale feedback;
- selected-path change;
- send-window contraction that removes budget;
- real media arrival that consumes the remaining service;
- connection close.

A successful observation is evidence supplied to the normal SCReAM growth logic;
it does not directly set a bitrate or reference window. A failed/aborted probe can
reduce confidence but cannot increase rate. Periodic probes cannot inflate the
estimate merely because the application cannot sustain the probed rate.

## Controller classifications and transitions

The implementation may encode these as flags rather than a public enum, but its
behavior MUST be equivalent.

| Classification | Entry | Required behavior | Exit |
| --- | --- | --- | --- |
| Unproven | RTP path writable, no feedback for a committed RTP packet | Initial target, bounded pre-media probe, no evidence-free growth | First valid packet feedback |
| Learning | Valid feedback exists but fewer than three feedback rounds or one smoothed RTT of delivery | Grow only from delivered evidence | Evidence threshold met, or congestion/staleness |
| Steady | Credible feedback and offered load | Follow pinned SCReAM and governed policy | ALR, congestion, stale feedback, or path change |
| Application-limited | Offered RTP below 85% threshold for 200 ms with no pacing/window block | Freeze unsupported growth, decay confidence, demand-aware probes | Offered load recovers or another state dominates |
| Congested | Pinned SCReAM queue/loss/ECN response triggers | Stop probes, back off, shed stale video/RTX | Pinned recovery conditions hold |
| Feedback-stale | No covering feedback by stale threshold | Stop growth/probes, decay confidence, conservative envelope | One valid covering report; return through Learning |

A selected-path replacement returns RTP control to `Unproven`, clears feedback
and in-flight entries that cannot apply to the new path, resets baseline/probes,
and preserves sender policy, desired rates, priorities, global media timestamps,
and outbound RTP identity where protocol continuity permits.

## Failure and ambiguity handling

- Duplicate or overlapping feedback acknowledges one committed data unit at most
  once.
- Unknown, abandoned, pre-history, or future packet IDs are ignored and counted.
- RFC 8888 report progression remains per SSRC.
- A receiver feedback pause cannot increase any rate, window, or probe budget.
- Delayed/batched feedback is separated from forward-path delay where the format
  permits and is never interpreted wholly as queue growth.
- Clock drift or a persistent delay-baseline shift triggers bounded draft-defined
  recovery, not queue-target inflation.
- Isolated random loss may slow unsupported growth; repeated loss or loss with
  queue growth causes the pinned congestion response.
- An external rate policer is a path condition. Repeated burst loss makes probes
  more conservative and cannot be answered by ever larger clusters.
- A caller-side failure after `Transmit` is ordinary apparent packet loss. There
  is no rollback or missing-receipt state.
- Controller faults produce bounded counters and one actionable warning/state
  transition, never unbounded per-packet events.

## Dense state and hard bounds

Hot sent/feedback processing uses dense rings indexed by unwrapped packet identity,
not per-packet heap objects or unbounded hash maps. Per-sender state uses dense
arrays indexed internally from `SenderId`.

Version-one hard internal bounds are:

| State | Bound |
| --- | ---: |
| Negotiated outbound RTP senders | `128` |
| Sent RTP entries | `32_768` and `5 s` |
| Feedback statuses accepted per RTCP packet | `8_192` |
| Acknowledgment reordering window | `4_096` |
| Paced RTP packets | `8_192` |
| Paced RTP transport bytes | `8 MiB` and public configured limit |
| Pacer time horizon | `100 ms` |
| Retained RTX age | `2 s` |
| Retained RTX bytes | public configured limit |
| Active probe clusters | `1` |
| Probe history | `64` records |
| Direct frame dependencies | `8` |
| Delay samples | `4_096` and `10 s` |
| Rate samples per category | `2_048` and `5 s` |
| Expirations processed per `poll` | `256` |

When both a count and duration/byte bound apply, the first reached wins. Expiration
advances monotonically. A ring slot validates packet generation before reuse.
Network input receives release-mode validation; internal generation invariants
also have debug assertions.

Reaching a media queue bound rejects new application admission or sheds whole
not-yet-started stale frames according to policy. Reaching feedback/history bounds
drops oldest no-longer-actionable diagnostic state, never still-required
congestion accounting. If preserving required accounting is impossible, the
connection enters a conservative terminal resource-exhaustion error rather than
continuing with false low bytes-in-flight.

No state is shared across connections. Scaling many connections is the caller's
scheduling problem. Each connection contributes one earliest wakeup and bounded
work.

## Statistics contract

Statistics are coherent snapshots. They expose semantic scalar values and reason
codes, not mutable controller objects.

### Connection-level observations

- negotiated packet-feedback mode and connection lifecycle state;
- current target media-payload bitrate, pacing bitrate, RTP bytes in flight, and
  queued media/DataChannel bytes;
- transmitted RTP, RTCP, SCTP, protocol-control, and padding byte totals;
- caller clock regressions, dropped network inputs, and
  unknown/duplicate/stale/wrong-path feedback counters.

### Per-sender observations

- the public sender policy and current allocation;
- queued packet/payload counts and transmitted packet/payload totals.

Encoding and DataChannel snapshots contain their identifiers and the bounded
receive/retirement or reliability/buffering/message counters documented in
`design.md`. Private controller variables and mutable controller state are not
public statistics.

## Validation

Acceptance uses deterministic crate-local simulation, property tests, component
comparison, the pinned live-browser matrix, and the root workspace gates.

### Property tests

- Reducing a sender's playout maximum satisfies every monotonicity property across
  all `0..=4095` ticks.
- Tightening one active sender cannot relax a shared path ceiling; making it
  inactive permits the next strictest active sender to control it.
- `effective_queue_delay_target <= native_queue_delay_target` always.
- Weighted allocation is demand-capped, capacity-conserving, deterministic, and
  converges to configured ratios.
- Priority or desired-rate changes never reset SCReAM, sent history, transport
  sequence space, or RTP continuity.
- Every acknowledged RTP transport byte corresponds to one previously committed
  sent entry and is acknowledged once.
- SCTP bytes never enter SCReAM sent history or RTP bytes-in-flight.
- Payload and transport accounting round trips without double-counting overhead.
- Feedback-stale and application-limited states cannot cause unsupported growth.
- Unknown, duplicate, reordered, wrapped, and per-SSRC RFC 8888 feedback cannot
  corrupt rings or acknowledgment progression.
- Paced queues, histories, dependency lists, and per-poll work remain within every
  hard bound.
- Policy changes recompute private deadlines but never mutate
  `MediaPacket.global_media_at`.
- Outbound RTP timestamps never rewind across source switches.
- One connection always exposes at most one next wakeup regardless of sender count.

### Deterministic network scenarios

The corpus includes:

- startup with media, startup before media, and delayed first feedback;
- constant rate, strong VBR, large keyframes, screen sharing, audio-only, and all
  media paused;
- bandwidth steps and rapid wireless capacity variation;
- low/high/changing RTT, receiver feedback batching, feedback loss, and stale
  recovery;
- uniform random loss, burst loss, reordering, duplication, and wraparound;
- tail-drop, AQM, rate policers, and competing CUBIC/BBR-like flows;
- optional RFC 8888 ECN/L4S paths, bleaching, and fallback;
- independent playout policies changing while queues, RTX, probes, and other
  senders are active;
- equal/skewed/changing priorities and desired rates;
- simultaneous SCTP and RTP load, large fragmented messages, and DataChannel
  priority changes;
- source switches with unrelated RTP timestamps but continuous global media time;
- provisional, synchronized, absent, contradictory, and discontinuous RTCP SR
  mappings;
- UDP and fallback ICE-TCP transport commit behavior;
- selected-path replacement and baseline reset.

### Quantitative acceptance

For deterministic scenarios whose configured bottleneck and propagation model are
known:

- the internal effective queue target never exceeds the strictest active ceiling;
- after convergence, controlled-bottleneck p99 queue delay is no more than the
  effective target plus `10 ms`, except during a declared path step or probe;
- a sustained equal-demand weighted allocation is within `10%` of its expected
  ratio after `5` smoothed RTTs;
- probe overhead never exceeds the fixed rolling `5%` bound;
- no target/window growth occurs while feedback is stale;
- no queue/history/resource count exceeds its hard bound;
- tightening latency never improves utilization by retaining work the looser
  policy rejected;
- SCTP/media accounting differs from independently summed emitted transport bytes
  by at most integer-rounding error;
- source switching produces no backward outbound RTP timestamp and no reset of
  feedback history.

The primary reported outcomes are useful delivered media, p50/p95/p99 global
media age at `Transmit`, queue delay, freeze duration, startup/post-pause
convergence, deadline damage, probe overhead, loss, fairness, and utilization.
Scenario pass thresholds are committed before tuning a candidate profile.

### External evidence

Pinned current Chrome and Firefox sessions MUST prove:

- negotiation and selected TWCC feedback;
- RTP, RTCP, NACK, and RTX behavior;
- pre-media padding on negotiated media/RTX SSRCs;
- independent per-sender playout-delay signaling and acknowledgment;
- priority reallocation, pause/resume, sustained VBR, and source switching;
- DataChannel coexistence and priority under RTP load;
- graceful close and fallback ICE-TCP behavior.

RFC 8888 behavior is proven with a standards test peer until ordinary browser
support is an acceptance target. Stored SDP cannot prove any runtime behavior.
Ericsson/libwebrtc traces are comparison oracles, not production dependencies or
independent browser evidence. A PulseBeam deviation is accepted only when the
same deterministic scenario demonstrates its intended latency/quality benefit
without violating congestion safety or the fixed bounds.

## Primary references

- [SCReAM v2, pinned revision 01](https://datatracker.ietf.org/doc/html/draft-ietf-ccwg-rfc8298bis-screamv2-01)
- [Ericsson SCReAM reference implementation](https://github.com/EricssonResearch/scream)
- [libwebrtc SCReAM implementation](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/congestion_controller/scream/)
- [libwebrtc SCReAM implementation differences](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/congestion_controller/scream/g3doc/implementation_diff.md)
- [libwebrtc transport-wide congestion-control extension](https://webrtc.googlesource.com/src/+/refs/heads/main/docs/native-code/rtp-hdrext/transport-wide-cc-02/README.md)
- [RFC 3550: RTP/RTCP](https://www.rfc-editor.org/rfc/rfc3550.html)
- [RFC 8835: WebRTC media transport priority](https://www.rfc-editor.org/rfc/rfc8835.html)
- [RFC 8888: RTP congestion-control feedback](https://www.rfc-editor.org/rfc/rfc8888.html)
- [RFC 8260: SCTP stream schedulers and message interleaving](https://www.rfc-editor.org/rfc/rfc8260.html)
- [RFC 8831: WebRTC DataChannels](https://www.rfc-editor.org/rfc/rfc8831.html)
- [RFC 8832: WebRTC DataChannel establishment](https://www.rfc-editor.org/rfc/rfc8832.html)
- [libwebrtc playout-delay extension](https://webrtc.googlesource.com/src/+/refs/heads/main/docs/native-code/rtp-hdrext/playout-delay/README.md)
