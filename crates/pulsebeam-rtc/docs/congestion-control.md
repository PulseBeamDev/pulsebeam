# Congestion control and explicit delivery bounds

Status: normative private implementation contract for `pulsebeam-rtc` version one.

This document defines the egress RTP congestion controller, caller delivery-bound enforcement,
media-payload and transport pacers, scheduler, probe manager, and DataChannel coordination.
It does not add public controller types. The only public inputs are the semantic
values in [`design.md`](design.md): `TimePoint`, `MediaPacket`, `FrameMetadata`,
`SenderAssignment`, `DeliveryConstraints`, DataChannel commands, network input, `poll`, semantic events, and
statistics.

The words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative.

## Normative hierarchy and pinned algorithm

The network-control core uses the window, queue-delay, target-rate, pacing, and
ACK-growth equations of
[`draft-ietf-ccwg-rfc8298bis-screamv2-01`](https://datatracker.ietf.org/doc/html/draft-ietf-ccwg-rfc8298bis-screamv2-01),
pinned to revision `01`, with the explicit loss-qualification deviation below. The source artifact used by the implementation review
has SHA-256:

```text
7beb2ff371106fa81a1678607ab3a1eb439ac9b0843d0d7fb64dab671132667f
```

The review artifact is the 100,016-byte archive copy. Run
`scripts/verify-scream-profile.sh` to retrieve that exact URL and verify both
properties before reviewing controller changes.

A later Internet-Draft revision, RFC, Ericsson commit, or libwebrtc commit MUST
NOT silently change production behavior. Adopting one requires a new documented
profile and fresh trace/simulation/native-client evidence.

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
2. Caller delivery bounds act outside the SCReAM core on media usefulness,
   admission, queue residence, and repairs. Their enforcement MUST NOT
   modify SCReAM's queue-delay target, congestion window, gains, or pacing equations.
3. Caller-selected sender rates bound local admission. One aggregate media-payload
   pacer enforces the native budget without assigning per-sender shares.
4. A deadline-aware transport scheduler combines per-sender service, frame
   usefulness, RTX, protocol control, DataChannels, and padding.
5. A bounded probe manager first discovers each negotiated primary stream using
   ordinary primary-SSRC padding with MID. Only authenticated sent-history receipt
   of that stream permits switching padding to its negotiated RTX SSRC; loss,
   missing feedback and other streams' receipts do not prime it. Without RTX it
   retains primary padding. It never creates an SSRC-zero source. RTX padding
   advances only the RTX sequence space and never signals or
   confirms primary-media playout. It carries MID and TWCC but no RTX OSN or media
   payload, keeping probe loss separate from primary-media contiguity.
6. SCTP retains its own congestion controller and bounded queues. The top-level
   scheduler coordinates eligible RTP/SCTP output without inventing a combined
   delivered-rate estimate. SCTP packets never become SCReAM-acknowledged data units.
7. Public aggregate media budget and demand are expressed in RTP media-payload bits per
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
| PulseBeam influence on SCReAM | Use only controls permitted by the pinned draft, principally the application maximum target bitrate; keep playout/deadline policy outside the controller | Mutate queue targets, gains, windows, or pacing from product policy | Keeps the SCReAM core faithful and independently testable while PulseBeam governs what media it offers and schedules. |
| Controller scope | One aggregate RTP controller per connection | Per-sender congestion controllers plus coupling | All bundled RTP shares one bottleneck; caller targets cannot manufacture independent capacity estimates. |
| Feedback | TWCC baseline; RFC 8888 normalized alternative | REMB; fixed-rate fallback; simultaneous TWCC + RFC 8888 | Packet-level arrival/loss data fits SCReAM. One selected mode avoids conflicting acknowledgment domains. |
| RFC 8888 accounting | Maintain acknowledgment progression per SSRC | Invent one aggregate RTP sequence cursor | RFC 8888 reports RTP sequence progression per SSRC; aggregation would mis-acknowledge packets. |
| Sent timestamp | `Output::Transmit` commit time | Caller departure receipts; NIC/kernel timestamp feedback | Keeps public API small; controller uses the last time observable inside the sans-I/O engine and conservatively treats later caller failure as loss. |
| Delivery input | Immutable `global_media_at` plus explicit caller age, queue-residence and repair bounds | Infer freshness from wire playout or treat playout max as a queue target | Caller policy stays separate from native transport estimates and browser render intent. |
| Delivery authority | Complete caller assignment without implicit profiles | Derive application limits from a private playout curve | RTC enforces declared limits without choosing source quality or freshness policy. |
| Sender target ownership | Caller-selected rates bound local admission; native aggregate pacing and window rules restrict output | RTC-owned weighted redistribution | Keeps application priorities and source/layer decisions in the SFU without weakening congestion safety. |
| Rate units | Caller targets and aggregate media budget use media payload; native burst pacer/BIF use transport bytes | One generic bitrate/byte unit | Keeps budgets directly comparable to codec/layer rates while congestion accounting includes observable transport overhead. |
| SCTP | Keep SCTP congestion control; reserve/coordinate its service outside SCReAM | Count SCTP as SCReAM BIF; feed SACK into a new coupled controller | TWCC/RFC 8888 does not acknowledge SCTP. V1 avoids inventing a second research-grade coupled controller. |
| Probing | RTP padding on negotiated media/RTX SSRC | Synthetic SSRC zero; no active probing | Matches normal RTP sender identity and enables pre-media/application-limited capacity observations without a special source. |
| Application-limited behavior | Keep the draft-defined bytes-in-flight growth bound authoritative; decay PulseBeam confidence and use demand-aware bounded probes | Add a PulseBeam-specific ref_wnd freeze; collapse estimate to media rate | Keeps product policy outside the SCReAM core while low offered load naturally limits growth through the draft's bytes-in-flight state. |
| ECN/L4S | Disabled in production; core-only equation tests | Require L4S/ECN; force-enable from configuration | Baseline must work on ordinary Internet paths; marking deployment needs separate validation. |
| Frame scheduling | Prefer whole unstarted-frame drops; started frame is not guaranteed completion | Packet-only FIFO; unbounded commitment once first packet sends | Minimizes decoder damage while preserving bounded queues and congestion safety. |
| Constants | Freeze `CongestionProfileV1`; changes require profile/doc evidence | Leave tuning qualitative or implementation-defined | Prevents implementation agents from silently choosing controller semantics and makes deterministic comparison meaningful. |

## Goals

- Keep interactive media near the live edge while remaining congestion safe.
- Preserve the SCReAM core as one independently testable network controller.
- Give each receiver a continuous, monotonic quality/latency policy without
  exposing queue targets or controller gains.
- Apply caller-selected sender ceilings inside one safe connection envelope,
  without redistributing targets or choosing application priorities.
- Support negotiated TWCC and RFC 8888 packet feedback without client modifications.
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
caller assignment + packet global_media_at + frame metadata
                              |
                       local admission
                              |
                        bounded queue
                              |
packet feedback -> native SCReAM -> aggregate media budget
                              |             |
                              |     media-payload pacer
                              |             |
                              +-> transport-byte pacer + native send window
                                            |
                                      transmit commit
```

One connection owns exactly one RTP path controller. Original media, RTX, FEC,
and RTP padding share its path state. There is no per-sender congestion window
and no coupled layer above several independent RTP estimators.

Delivery bounds are not a second bandwidth estimator. Caller targets cannot
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

`SenderAssignment.selected_bitrate`, selected/potential sender demand, aggregate media
budget events, and SFU-facing admitted/transmitted media rates use
`MediaPayloadRate`. Sender service quanta count original media payload; repairs
and transport overhead remain subject to native transport-byte pacing and window
accounting.

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

### Separate payload and transport accounting

The aggregate media-payload pacer charges committed original payload directly.
The native transport pacer charges the final emitted transport length. Admission
and send-window checks conservatively account for transport overhead before
commit. No per-sender grant, inferred service reservation, or payload-efficiency
conversion bridges these two counters.

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
    transport_bytes: u32,
    received: bool,
    newly_acked: bool,
    lost: bool,
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
- The ACK edge advances to the highest *received* sequence, after the entire retained interval and every reported identity have been validated. A gap beneath that edge contributes one-time span credit but not delivery or confirmed loss. An all-missing report does not move the edge or acknowledge bytes in flight.
- Missing packets remain recoverable until the bounded reordering deadline anchored to the first acknowledged higher received sequence in that space, or five-second history expiry, confirms loss. Repeated all-missing reports cannot start the reorder deadline. Duplicate reports update no bytes twice.
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
- Received statuses with unavailable or over-range arrival offsets are still received, but provide no arrival sample.
- ECN marks remain normalized for validation; production RTP control supplies disabled ECN in both feedback modes. Classic and L4S behavior is core-only test coverage, not live path support.

### Feedback hold and staleness

Where the feedback format permits it, the implementation separates receiver
feedback hold from the network loop:

```text
feedback_rtt = feedback_received_at - packet_committed_at
network_loop = max(0, feedback_rtt - receiver_feedback_hold)
```

Sparse or application-limited samples cannot rapidly rewrite either estimate.
Only a newly accepted, validated covering network report refreshes feedback
freshness, including a new all-missing report; replaying the same report does not. Duplicate-only, unknown, wrong-path, malformed and locally
confirmed loss do not. Staleness is evaluated before recovery evidence, so a
recovery report returns through Learning without growing the window on that update.
Feedback staleness can never increase the target rate.

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

The connection's adapter receives committed RTP identities, transport sizes,
packet classes, normalized feedback, selected-path state, current RTP bytes in
flight, paced queue state, offered/admitted media rates, and aggregate desired
rate. Aggregate potential sender demand, not only currently selected media,
sets the draft's application maximum target bitrate. Selected demand bounds
local body-media admission; it does not determine a redistributed sender share. Potential below selected is
invalid; potential is source/layer interest, not physical capacity or a reservation.
The adapter derives feedback freshness, report timing, confidence, application-
limited state, and the draft's application maximum target bitrate outside the
core.

The isolated SCReAM core receives accepted arrival/loss evidence, ACK-edge
credit, committed send evidence, RTT/feedback hold, RTP bytes in flight, path
reset, ECN mode (disabled in production), and the application maximum target
bitrate. Caller application priorities, frame IDs, source selection, SCTP sequence/SACK
state, raw playout ranges, and the adapter's offered/admitted media rates are
not SCReAM core inputs.

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

The reference window grows only on eligible fresh feedback with accumulated
highest-received ACK-edge credit, including one-time credit for covered gaps,
and falls on the draft's queue signals or qualified confirmed congestion-loss
signals. Congestion reduction precedes the independent once-per-smoothed-RTT increase; eligible fresh
feedback can fund growth even when congestion is pending or reduction occurs on
that update. Timer-only updates can apply pending reduction but do not replay
ACK credit for growth. Production ECN remains disabled. Random isolated wireless loss is
not silently reclassified as sustained congestion, but repeated loss or an
overflowing queue cannot be filtered away.

### Loss qualification

Confirmed loss remains truthful input to the native loss EWMA, ACK-edge ledger,
and loss counters. It is not automatically evidence that the available bitrate
fell. This profile explicitly deviates from the draft's low-queue `1%` EWMA
backoff trigger, using bounded loss observations inside the same native core.

The reference is libwebrtc commit
[`ba469aa2093ba950066258ca0a59a6fbd1295582`](https://github.com/webrtc-sdk/webrtc/tree/ba469aa2093ba950066258ca0a59a6fbd1295582):
`send_side_bandwidth_estimation.cc` accumulates at least 20 expected packets,
does not reduce for loss up to 10%, and limits decreases to 300 ms plus RTT.
`loss_based_bwe_v2.cc` uses observations spanning at least 250 ms of send time, requires three
observations, retains fifteen, and uses 0.9 temporal weighting with a byte-loss
objective instead of treating every lost data unit as capacity loss.

PulseBeam qualifies low-queue loss over observations of at least `20` received or
confirmed-lost data units and `250 ms` of packet send time. Completion advances the
send-time anchor to the latest reported send; delayed feedback and timer polls do
not age a short burst into a completed observation. Actual confirmed statuses and
transport bytes remain accumulated, including late losses. At least three
completed observations are required; the latest fifteen are weighted by `0.9` per
observation in Q16. The
loss fraction is weighted lost transport bytes divided by weighted expected
transport bytes. Unconfirmed gaps contribute no observation. A fraction above
`10%` permits the existing native loss reduction at most once per
`300 ms + srtt`, using an observation no older than `6 s`. This is a loss
qualifier, not libwebrtc's bandwidth estimator: it stores no alternate bitrate,
never supplies a rate floor, and never overrides the native send window.
Unqualified low-queue loss does not reset the native congestion-recovery epoch
or install a policer-window cap. Sent history carries loss provenance through
controller conversion: RFC reordering confirmation and five-second history expiry
remain native losses, but expiry cannot establish persistent policing. Age-driven
retirement preserves an earlier reordering deadline as the loss origin even if
bounded confirmation work deferred it. The policer
uses a separate observation with the same packet/span/history/weight/age bounds,
including received transport bytes and reordering-confirmed losses but excluding
expiry-origin losses. It supplies no bitrate or window. Policer classification
requires this capacity-loss qualification in addition to its existing native
loss/queue conditions, a current reordering-confirmed loss, and a received-arrival
observation no older than one smoothed RTT. Receipts and losses need not arrive in the same feedback batch.
Loss-only feedback without that bounded contemporaneous delivery/delay evidence
retains native loss backoff but cannot install a persistent policing ceiling
from stale low-delay state. A fresh receipt or new isolated loss cannot combine
with expiry-heavy history to establish that ceiling. Native loss accounting and
backoff still include expiry. Qualified flowing loss and actual policer remediation
still act. Selected-path replacement clears the receipt witness.

Confirmed loss with native average queue delay above one quarter of its target
retains the draft's immediate reaction cadence. Native delay, ECN, policer
remediation, backoff equations, and reference-window growth bounds are unchanged.
A selected-path replacement clears the observation and cadence state.

The target bitrate is a controller output for RTP media payload. It is not a proof
of physical bottleneck capacity and is named `target_media_payload_rate` in local
code and statistics. “Sustainable path capacity” is not used as a synonym.

### Application-limited behavior

The connection is RTP application-limited when the measured RTP payload offered
at admission remains below `85%` of the lower of aggregate caller-selected rate
and the native media target for
at least `200 ms` and no eligible RTP packet is held back by pacing or the send
window. A bounded `200 ms` measurement bucket counts validated submission bytes,
including media rejected by usefulness or queue admission; accepted bytes are measured
separately. Queue nonemptiness and configured desired bitrate are not substitutes
for measured offer. This measurement governs only outer confidence and probes, never SCReAM's native growth equation.

While application-limited:

- SCReAM's draft-defined bytes-in-flight growth bound remains authoritative; PulseBeam injects no separate application-limited freeze into the core;
- sparse delay/RTT samples cannot rapidly move the baseline;
- the last credible rate is retained while confidence decays with a `5 s`
  half-life;
- demand-aware probes MAY test retained or higher capacity;
- returning media exits application-limited state immediately once offered load
  reaches the threshold.

Low-complexity VBR video, audio-only periods, paused video, and all-media-paused
periods can be application-limited. Media rate and available network rate are
never equated.

### Queue-delay target ownership

SCReAM computes its queue-delay target entirely inside the pinned draft core,
including competing-flow adaptation. Revision `01` begins from the draft's
recommended `60 ms` target and can natively relax as far as `400 ms` where the
algorithm permits.

PulseBeam does **not** cap, replace, or otherwise mutate that target. Playout and
deadline policy is enforced outside the congestion controller by deciding which
media remains useful, what media demand is offered, how aggregate media is paced,
and what queued work is scheduled or dropped. The SCReAM core sees
only draft-defined controller inputs such as packet feedback, path ECN mode,
bytes in flight, and the application maximum target bitrate.

`effective_queue_delay_target` remains in private statistics for compatibility in
version one, but equals the SCReAM-native target. It is not a second policy input.

### ECN/L4S

Production version-one behavior uses delay and loss. ECN is disabled for both
negotiated feedback modes, even when RFC 8888 reports marks. The private core
retains draft Classic and L4S equations for independent testing; deploying live
ECN requires a separate path-validation and marking change. No public switch
can force ECN on.

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
| Queue/loss backoff equations | pinned draft; low-queue loss eligibility uses the explicit bounded observation policy above |
| Low-queue loss observation | at least `20` data units and `250 ms` send-time span, byte-weighted |
| Low-queue loss readiness/history | at least `3` completed observations; latest `15`, temporal weight `0.9` |
| Low-queue capacity-loss threshold | above `10%` |
| Low-queue loss decrease interval | `300 ms + srtt` |
| Maximum age of a completed loss observation | `6 s` |
| ECN response | disabled in production; pinned draft equations tested in the private core |
| Clock-drift and path-baseline filters | pinned draft |
| Target-rate derivation | pinned draft |
| Normal pacing derivation | pinned draft, then bounded by the transport limits below |
| Sent-history entries | `32_768` |
| Sent-history maximum age | `5 s` |
| Maximum accepted ack reordering behind newest generation | `4_096` packets |
| Maximum statuses normalized from one feedback packet | `8_192` |
| Feedback stale threshold | `clamp(max(500 ms, 3 * srtt), 500 ms, 2 s)` |
| Confidence half-life while stale or application-limited | `5 s` |
| Feedback recovery requirement | a new validated covering report for a nonterminal current-path committed unit or an accepted late-receipt transition |
| Maximum expiration work in one `poll` | `256` entries; return immediate wakeup if more remains |

### Transport, queue, and scheduler ledger

| Field | Version-one value |
| --- | ---: |
| Initial payload efficiency | `0.90` |
| Payload-efficiency clamp | `[0.50, 0.99]` |
| Payload-efficiency EWMA time constant | `1 s` |
| Maximum paced RTP packets | `8_192` |
| Maximum private paced RTP transport bytes | `8 MiB`, additionally capped by public configured media bytes |
| Maximum media pacing service/residence | caller `max_queue_residence`, within configured byte and packet limits |
| Maximum retained RTX age | caller `repair_window`, additionally capped by media-age usefulness |
| Repeat repair suppression | No retransmission within one smoothed RTT of its last committed repair; first repair remains immediate |
| Maximum retained RTX transport bytes | public retransmission limit |
| Protocol service | Prepared protocol output precedes user lanes; due RTP control precedes RTP media |
| Scheduler quantum | one eligible emitted packet per user-lane turn |
| Media-budget updates | one coalesced latest native aggregate-budget event |
| Downward safety change | immediate |
| Maximum frame dependencies | `8` |

### Probe ledger

| Field | Version-one value |
| --- | ---: |
| Concurrent probe clusters | `1` |
| Probe-history records | `64` cluster records; `32,768` emitted RTP accounting entries. Recent-entry eviction suppresses probes for `5 s` |
| Minimum unmet-demand trigger | desired payload exceeds the native estimate by `20%` and `50_000 bit/s`, or a sender's potential source service exceeds its caller-selected rate and aggregate potential exceeds the native estimate. The latter includes paused sources and active-source upgrades below the ordinary trigger |
| Demand-derived cluster rate | `min(total probe budget rate, max(2 * credible media rate, 300_000 bit/s))` |
| Ordinary cluster transport target | `max(3,000 B, demand-derived cluster rate * 20 ms / 8)`, additionally at least `declared potential payload rate * srtt / 8` for a potential-only candidate that has not committed media or remains in the initial five-second period. Established paused or application-limited service requests at least the native reference-window bytes so a successful sample can fund observed-flight growth. RTT/reference-window extensions additionally fit at most `100 ms` of the chosen probe rate, with the existing `3,000 B` minimum. All targets remain capped at the current send window and `48 KiB`; actual emission remains subject to rolling allowance, lifetime, horizon, and per-packet send-window checks |
| Ordinary cluster pacing | `min(2 * declared potential probe rate, max(2 * credible media rate, 300_000 bit/s))`; separate commit-driven probe deadline with at most `1 ms` of missed scheduling intervals retained. Native pacing is not a minimum discovery rate. If any sender has committed body media and positive selected demand, discovery additionally stays at or below the current native pacing rate, repriced downward on native rate changes. Every emitted byte still charges ordinary native pacing debt; native rate updates reprice remaining byte debt without clearing it |
| Cluster duration | `clamp(requested bytes * 8 / cluster pacing rate, 20 ms, 100 ms)`; one-packet recovery retains `20 ms`. An emitted sample with at least five packets and `80%` of the requested bytes remains eligible for feedback |
| Minimum packets | `5` |
| Maximum packets | `192` |
| Maximum cluster transport bytes | `48 KiB` and current send window, whichever is lower |
| Selected-media probe reservation | pending service must fit every selected sender's explicit `max_queue_residence`; wire playout and media kind do not change it |
| Minimum interval | Periodic discovery: `max(5 s, 4 * srtt)`; initial discovery and one-packet feedback solicitation: `max(1 s, 4 * srtt)`. Successful discovery feedback may bypass only this interval during initial probing or for an eligible potential-only source that has never committed media; it grants no padding credit |
| Rolling probe admission | A new padding emission requires `P <= floor(N / 19) + A` transport bytes in the trailing `5 s`, including that emission, where `P` is emitted RTP padding probes and `N` is emitted non-probe RTP including media and RTX. Ordinary `A = 48 KiB`. Windows retaining spent initial service stay bounded by its earned initial allowance, without granting new ordinary credit |
| Initial probe service | Once per connection, for `5 s` from its first ordinary probe, and only while aggregate selected demand is zero. `A` may grow to `clamp(2 clusters * 100 ms * 2 * potential payload rate / 8, 48 KiB, 96 KiB)`. Path changes never rearm this period or erase emitted bytes |
| Success feedback deadline | `2 * srtt`, clamped to `[200 ms, 2 s]` |
| Successful observation | At least `80%` of `max(requested bytes, emitted bytes)` newly reported received and `80%` of the minimum packet count before the deadline, with no SCReAM congestion response; reordered receipt transitions count independently of earlier ACK-edge credit, while duplicate reports earn no credit. A one-packet feedback-recovery solicitation uses its emitted packet count and cannot trigger startup continuation |
| Discovery continuation | Earned once by the latest successful full cluster; initial or never-started eligible sources may consume it after candidate selection. Feedback processing and wakeup scheduling retain it only while that eligibility holds. Outside initial service it uses ordinary rolling credit, never rearms the five-second startup allowance, and preserves pacing debt and native bounds |
| Queue-growth abort | effective target exceeded or queue rises `10 ms` during cluster |
| Loss handling | Loss remains truthful native-controller input. A qualified SCReAM congestion response aborts the cluster; an isolated loss report alone does not replace the received-byte/packet quorum or deadline |

### Caller delivery-bound ledger

| Constraint | Authority |
| --- | --- |
| Media age | caller `max_media_age`, measured against immutable global media time |
| Local queue residence and pending pacing service | caller `max_queue_residence` |
| Retained-original age from transmit commit | caller `repair_window` |
| Receiver render intent | `playout_delay` is wire signaling only |
| Queue/history storage | independent configured byte and packet limits |
| Probe queue impact | fixed transport safety limit of `10 ms`, independent of wire playout |

## Explicit delivery constraints

Every sender begins unbound. Admission requires a complete `SenderAssignment`
and a matching caller-supplied `SourceIncarnation`. The assignment supplies both
payload rates and every delivery bound; RTC supplies no audio/video freshness,
playout-derived recovery allowance, or implicit potential-rate inheritance.
Durations must fit the microsecond media-time domain and a local timer deadline.
Unrepresentable values are rejected without mutating the previous binding.

### Media and queue usefulness

```text
latest_useful_global = global_media_at + max_media_age
native_network_service = native_queue_delay + measured_srtt / 2
estimated_arrival = observation_global + pending_pacer_service
                   + native_network_service
```

Global addition is bounded by the timestamp domain. The native service estimate
is zero before a controller envelope is available; no fixed processing or
clock-quality reserve is invented. The estimate is transport feasibility, not a
claim of exact one-way or capture latency.

New and queued media must remain useful at estimated arrival. Pending pacing
service and elapsed local queue residence must separately fit
`max_queue_residence`. Source bytes and `global_media_at` are never rewritten to
make a packet eligible. Native send-window, transport pacing, aggregate payload
pacing and configured storage limits remain authoritative even for large caller
bounds.

### Repair usefulness

RTX requires a retained original whose age since transmit commit is strictly
less than `repair_window` and whose predicted arrival still satisfies
`max_media_age`. The first NACK may repair immediately; repeated repairs remain
suppressed for one smoothed RTT. A long RTT never extends the caller's repair
window. Wire-default playout and explicit `0/0` do not select different histories.

### Complete assignment changes

Validation precedes mutation. Replacing or removing the source incarnation fences
old queued work and retained repairs, including when its SSRC is reused.
Positive-to-zero selection clears old media and repairs while explicit potential
may retain bounded probe interest. Decoder-ready `RecoverSenderMedia` supplies a
complete same-incarnation assignment and retains only useful committed repairs.

Tighter bounds apply immediately; bounded expiry work may continue through
subsequent polls. Larger bounds do not resurrect discarded work, clear spent
pacing debt, alter committed identity, or override native controller state.
Changing wire playout alone does not change these bounds.

## Global media time and frame admission

The global timeline contract is defined in [`design.md`](design.md). Congestion-control code
uses these additional invariants:

- all packets of one frame have identical `global_media_at`;
- a transit packet's value is never changed;
- source switching does not reset committed congestion state or wire identity;
- a packet mapping behind an already committed outbound frame is stale rather
  than an excuse to rewind RTP time;
- RTP clock discontinuities are ingress clock-mapper events, not congestion
  events;
- clock quality does not supply implicit delivery limits or network capacity;

The scheduler prefers dropping a complete not-yet-started frame. Cut-through
forwarding means a started frame is not an unbounded commitment: later safety
pressure may drop a remaining packet, leaving an interrupted frame. Video
recovery is surfaced to the caller.

When dependencies are known, dropping a required frame marks queued dependents
ineligible until a random-access or otherwise independent frame restores the
chain. `Unknown` dependencies permit only whole-current-frame decisions; the
connection does not invent dependency relationships.

## RTP media budget and SCTP coordination

The connection reports the native aggregate RTP media-payload budget, not
measured spare bandwidth, physical bottleneck capacity, or a reservation:

```text
media_budget = native_controller.target_media_payload_rate
```

Startup, contraction, and recovery come from the same native controller.
Potential demand supplies the application maximum and probe interest; it never
raises the native target. Caller-selected rates remain unchanged when the budget
changes. `MediaBudgetChanged(MediaPayloadBitrate)` coalesces pending updates to
one latest value, also reported as `ConnectionStats.media_budget`.

Local admission uses each sender's caller-selected payload rate. Accepted payload
charges a leaky bucket; rejected commands do not. Debt cannot accumulate idle
credit. The caller's queue-residence horizon bounds burst debt, with at most one
packet of overshoot. Zero selected rate rejects new media. A caller retries the
same immutable packet after `WouldBlock` without advancing source frame state.
Once usefulness expires, `MediaExpired` instead tells the caller to continue
with fresh media.

At transmit commit, a separate aggregate media-payload pacer charges original
payload at the native budget. It preserves byte debt on rate changes and does
not grant per-sender shares. The native transport-byte burst pacer and send
window still constrain all RTP, including repairs and probes. Control output
remains independent of the media-payload pacer. Expiry, queue, and continuity
bounds remain authoritative.

SCTP keeps its own acknowledgments, congestion window, and bounded buffering.
Supported committed transport counters describe sends, not acknowledgments or
delivery. Version one does not derive a combined delivered-path estimate or
subtract an inferred SCTP ACK rate from RTP. It does not promise an independent
fixed-rate SCTP or control reservation. Coordination occurs through bounded
protocol service and eligible user-lane scheduling. Shared-path SCTP queueing can
increase RTP delay and cause the normal SCReAM response.

SCTP transport bytes never enter SCReAM sent history, RTP bytes-in-flight, or RTP
delivery samples. A data-only connection uses SCTP congestion control and bounded
DataChannel service; without selected or potential RTP demand its RTP budget is
zero. No dependency patch or additional acknowledgment-byte observable is needed
for this contract.

## Caller targets and bounded sender service

The SFU owns source/layer selection, application priorities, and quality
hysteresis. RTC neither computes sender grants nor redistributes unused caller
targets. Eligible original-media senders receive equal byte quanta, independent
of their selected or potential rates. This is bounded transport service ordering,
not another application allocator.

Setting selected bitrate to zero rejects new media admission and clears old queued
media and repairs. Already committed wire work remains irrevocable. A caller may
explicitly keep positive potential while selected media is paused, or declare
both zero to withdraw RTP interest. Potential never grants body-media service or
bypasses window, pacing, usefulness, feedback, or probe-budget limits.
Every complete update supplies both rates. Same-path application updates,
including zero-to-positive changes, preserve native core state; only a genuine
selected-path change resets path state.

## Pacer and top-level scheduler

The pacer is work-conserving only inside all active safety constraints:

- SCReAM send-window/reference-window rules;
- transport pacing rate;
- native path safety and explicit per-sender queue-residence horizons;
- aggregate media-payload pacing and equal-byte-quantum sender service;
- frame and RTX usefulness;
- protocol and DataChannel bounds.

### Service classes

Selection has four bounded classes:

1. ICE, DTLS, SCTP association control, and RTCP required for liveness or
   congestion feedback;
2. governed user traffic, consisting of an RTP lane and an SCTP-user-data lane;
3. non-urgent reports/control;
4. RTP padding probes.

Prepared protocol output precedes user lanes. On an RTP turn, due RTCP control
precedes media. SCTP association control uses its native bounded output lane.
Padding is selected only when every real-traffic lane is empty or blocked; ready
SCTP output prevents an otherwise eligible padding probe.

The two class-2 lanes are **not** strict-priority ordered. They alternate eligible
packet service, subject respectively to the SCReAM window/pacer and SCTP's own
congestion controller. A blocked or empty RTP lane yields immediately to eligible
SCTP output; a blocked or empty SCTP lane does not stall RTP. This is bounded
work-conserving service, not a rate reservation or a guarantee within one RTT.
Every returned transmit is committed and must receive its immediate caller
socket-write attempt before the connection is driven again.

Within the RTP lane, useful repairs precede original media. Original-media
deadlines determine eligibility before sender deficit selection. Each eligible
sender receives the same fixed media-payload byte quantum, not a target-derived
weight. Empty or blocked senders do not reserve service.

Within the SCTP lane, `dcsctp` applies the negotiated RFC 8260 scheduler and the
public 16-bit DataChannel weights. SCTP user data cannot bypass its association
congestion window merely because the top-level lane has service.

The media-payload pacer charges only committed original payload. The native
transport pacer charges final emitted transport bytes, including header and RTX
overhead. Neither counter substitutes for the other.

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
largest unmet caller-declared potential source interest with a valid padding form.
Probe results remain connection-level and do not redistribute caller targets.

### Eligibility and lifecycle

Ordinary clusters use a separate probe pacing deadline. The first packet must
wait for ordinary pacing eligibility; subsequent packets may use the cluster's
fixed probe rate while no media or repair is queued. Each emitted probe charges
both the probe deadline at that rate and the ordinary deadline at the current
native pacing rate, using final emitted transport bytes. The ordinary deadline
advances from the later of its previous deadline and the emission time, without
burst credit. Only the probe deadline may retain at most 1 ms of missed
scheduling intervals, matching the production timer quantum. Larger delays
cannot accumulate more catch-up credit, and the 20 ms cluster lifetime remains
unchanged. Aborting or completing a cluster clears only its probe schedule,
never the ordinary debt. New clusters still wait for that debt.
One-packet recovery solicitations retain ordinary pacing, without acceleration.
Before any probe packet, its reserved transport service plus existing ordinary
pacing debt must fit every selected sender's explicit queue-residence horizon.
Otherwise the cluster aborts without clearing ordinary debt. Potential-only
senders have no selected-media reservation, but remain subject to all other
window, budget and congestion gates.

For a potential-only candidate (zero selected demand) that has never committed
media or remains in the initial five-second period, the ordinary cluster
transport target samples one smoothed RTT of aggregate declared potential
payload demand, or the ordinary demand-derived 20 ms target or 3,000 bytes,
whichever is larger, capped by the current send window and 48 KiB. Candidates
with selected media retain the smaller ordinary demand-derived target, so a
quiet interval in an active stream does not request an RTT-sized burst.
Sampling demand's flight size permits potential-only senders to supply
the observed native RTP flight that SCReAM requires before increasing its
reference window. A fixed 3,000-byte flight ceiling cannot bootstrap layers
above the corresponding RTT-dependent native target when no real media is
selected. The 192-packet limit accommodates the bounded byte sample despite
RTP padding's 255-octet limit. This is only a requested probe size, not reserved
allowance, a window floor, or permission to emit that many bytes: every packet
must pass the rolling-budget and send-window checks.
Packet padding is still bounded to 255 octets and at most five milliseconds of
the selected pacing rate. Probe deadlines participate in connection wakeups and
cannot extend the 20 ms cluster lifetime. Once a sender has committed media and
the finite initial period ends, losing selected demand does not rearm the
demand-sized bootstrap flight sample. Established periodic clusters and promoted
covering-report recovery follow-ups retain the ordinary demand-derived 20 ms
target and minimum. They do not renew initial allowance or request a larger RTT
flight sample. A fresh potential-only sender still needs the bootstrap native
reference-window sample.
Ready protocol or SCTP output and real
RTP retain precedence. Neither probe pacing nor successful feedback directly
increases the SCReAM reference window or guarantees a media bitrate.

A probe may begin when:

- the path first becomes writable and RTP desired demand is nonzero;
- potential source demand exceeds the native estimate by both fixed trigger thresholds;
- VBR or paused traffic has left capacity confidence decaying;
- a sender has potential service above its selected rate, and aggregate potential
  exceeds the native estimate, even below the ordinary fixed trigger thresholds;
- a congestion response has stabilized and evidence is needed before recovery.

The exact cluster size, interval, overhead, and success criteria are fixed in the
profile. Periodic discovery uses libwebrtc's five-second ALR probe interval,
separate from its one-second repeated-initial wait. Feedback solicitation retains
the one-second interval so the periodic policy cannot starve fresh observations.
Both intervals remain bounded below by four smoothed RTTs, and the covering-report
follow-up retains its explicit bypass. The ordinary 48 KiB allowance is PulseBeam low-traffic policy, not a
SCReAM window floor or an unconditional 5% share. It bounds steady zero-media
probing to 78,644 bit/s averaged over five seconds, while permitting a
window-sized flight sample. After the initial period, a never-started,
potential-only candidate on a connection with no established active media may
request one native reference window plus the existing 3,000-byte minimum sample.
This bounded additive headroom supports native RTT-cadenced increases without
doubling the idle request on a rate-limited path.
The current window, 48 KiB request cap, unchanged rolling allowance and finite
rate-derived service bound still limit the request and every emission. Existing
body service and previously committed candidates retain the one-window sample;
this does not rearm initial service credit.

Initial capacity discovery has a separate finite service allowance. This adapts
libwebrtc's two initial probes, 100 ms initial service duration, five-second
repeated-initial period, application-demand cap of twice the requested rate,
and feedback-driven continuation (`ProbeControllerConfig`,
`InitiateExponentialProbing`, `SetEstimatedBitrate`, and `InitiateProbing` at
pinned revision `ba469aa2093ba950066258ca0a59a6fbd1295582`). PulseBeam retains
20 ms minimum window samples rather than replacing SCReAM with a probe-rate
estimator. RTT/reference-window-sized samples do not force all requested bytes
through that minimum interval: as in `ProbeController`, application-driven
probe pacing is limited to twice the declared declared potential rate and twice
credible media rate, with the existing 300 kbps credible-rate floor. Native
pacing is an allowance, not a command to use that entire rate for discovery.
The service duration adapts to requested bytes at the separate probe rate,
capped at 100 ms, matching the pinned initial-duration bound; RTT/reference
requests also fit that finite service budget apart from the existing minimum.
Ordinary media retains native pacing. Any established active sender additionally
caps connection-wide probe pacing at the current native rate, including downward
repricing while a cluster runs, so a different paused candidate cannot borrow
service from ongoing body media. When there is no established active sender,
never-started and paused candidates retain the separately bounded
bootstrap/recovery rate. Every probe byte charges the same native debt, and
native window authority and rolling credit are unchanged.
During the initial period, with positive
potential demand and zero aggregate selected demand, the allowance may grow
with actual potential demand to at most 96 KiB. It is neither refreshed by a
successful probe nor rearmed on path change. A successfully completed latest
ordinary sample can trigger a prompt follow-up without waiting the ordinary
interval. This adapts libwebrtc's 80% requested-byte and minimum-packet feedback
criteria (`ProbeBitrateEstimator`), not GCC's 70% probe-rate threshold: only
SCReAM computes credible capacity. The packet quorum counts receipts, not loss
reports. As in that estimator, partial feedback containing an isolated loss
cannot fail a cluster before its remaining receipts can satisfy the quorum.
All loss still reaches native qualification, and a native congestion response
or expired feedback deadline aborts the sample. Timer quantization may end transmission just
short of the requested byte target; a sufficient sample remains pending until
its own feedback arrives. Continuation identifies the latest cluster by its
finish time, independently of its last packet time. A promoted ordinary sample
may grow the allowance with newly declared demand, but never refreshes spent
credit or the initial-period start. The bypass
is consumed by an attempt, and congestion, abort, stale feedback, selected
media or initial-period expiry prevents continuation. Recovery solicitations
do not earn this startup continuation.

When initial discovery ends or real media is selected, the ordinary allowance
applies again. Retained startup bytes can therefore suppress further padding
until they age out; the ledger is not cleared. Neither allowance nor the
requested sample or adaptive duration bypasses congestion gates or guarantees startup on every RTT,
source rate or path. The budget uses final emitted RTP transport bytes; RTCP,
SCTP and other protocol traffic earn no credit. It remains
connection-wide across selected-path changes and is checked against the expiry
of older non-probe traffic as well as current emissions. All safety gates take
precedence over the minimum cluster packet count. One cluster aborts immediately on:

- effective queue target violation or the configured queue rise;
- configured probe loss;
- stale feedback;
- selected-path change;
- send-window contraction that removes budget;
- real media arrival that consumes the remaining service;
- connection close.

When feedback is stale, positive potential demand remains, and no media is
queued or in flight, recovery may solicit a covering report with one padding
packet. A positive caller-selected target does not block recovery: it is a ceiling,
not evidence of sendable media, and stale delay estimates can prevent admission.
The same one-packet solicitation is permitted before feedback becomes stale when
positive selected demand remains and even newly created media cannot meet its
explicit media-age bound under the native queue-delay estimate plus half the
measured RTT. This requires a padding-eligible sender and empty media
queue and RTP flight; ordinary congestion backoff and media admission are not
bypassed. It supplies a new delay observation when the retained estimate itself
prevents media from producing feedback. Media usefulness and expiry checks remain
unchanged.
This recovery-only packet retains the ordinary probe retry
interval, pacing, selected-path, send-window, and emitted-padding budget gates.
After a valid covering report, a prompt follow-up may bypass only the cluster
interval. If feedback is fresh and normal probe permission is granted, this
follow-up may become one ordinary probe cluster, subject to the existing packet,
duration, byte, pacing, send-window, congestion and rolling-overhead limits.
Successful feedback from an established sender's promoted cluster cannot rearm
the covering-report interval bypass or startup continuation after the initial
period.
Otherwise it remains one padding packet to obtain a second fresh covering report.
The interval bypass is consumed by that attempt; it cannot chain additional
clusters. A blocked or aborted attempt cannot keep that interval bypass armed.
Recovery does not grant ordinary probe permission while stale, reset SCReAM,
or make the first recovery report growth-eligible.

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
| Steady | Credible feedback and offered load | Follow pinned SCReAM and explicit caller bounds | ALR, congestion, stale feedback, or path change |
| Application-limited | Offered RTP below 85% threshold for 200 ms with no pacing/window block | Freeze unsupported growth, decay confidence, demand-aware probes | Offered load recovers or another state dominates |
| Congested | Pinned SCReAM queue/loss/ECN response triggers | Stop probes, back off, shed stale video/RTX | Pinned recovery conditions hold |
| Feedback-stale | No covering feedback by stale threshold | Stop growth/ordinary probes, allow only the bounded recovery solicitation above, decay confidence, conservative envelope | One valid covering report; return through Learning |

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
| Probe history | `64` cluster records and `32,768` emitted RTP accounting entries |
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
- native aggregate media budget, pacing bitrate, RTP bytes in flight, and
  queued media/DataChannel bytes;
- transmitted RTP, RTCP, SCTP, protocol-control, and padding byte totals;
- caller clock regressions, dropped network inputs, and
  unknown/duplicate/stale/wrong-path feedback counters.

### Per-sender observations

- the public sender policy and caller-selected/potential demand;
- queued packet/payload counts and transmitted packet/payload totals.

Encoding and DataChannel snapshots contain their identifiers and the bounded
receive/retirement or reliability/buffering/message counters documented in
`design.md`. Private controller variables and mutable controller state are not
public statistics.

## Validation

Acceptance uses deterministic crate-local simulation, property tests, component
comparison, pinned controlled libwebrtc interoperability, and root workspace gates.

### Property tests

- Wire playout intent does not alter explicit age, queue-residence, or repair bounds.
- Tightening delivery bounds does not rewrite caller targets or the native
  queue-delay target.
- `effective_queue_delay_target == native_queue_delay_target` always.
- Native budget changes do not redistribute caller-selected targets; equal-byte
  sender service remains bounded across different packet sizes.
- Caller target changes never reset SCReAM, sent history, transport
  sequence space, or RTP continuity.
- Every acknowledged RTP transport byte corresponds to one previously committed
  sent entry and is acknowledged once.
- SCTP bytes never enter SCReAM sent history or RTP bytes-in-flight.
- Payload and transport accounting round trips without double-counting overhead.
- Feedback-stale timer polls cannot replay growth; under low offered load, SCReAM growth remains bounded by the draft-defined bytes-in-flight gate.
- Unknown, duplicate, reordered, wrapped, and per-SSRC RFC 8888 feedback cannot
  corrupt rings or acknowledgment progression.
- Paced queues, histories, dependency lists, and per-poll work remain within every
  hard bound.
- Delivery-bound changes recompute usefulness but never mutate
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
- disabled production CE in both feedback modes; private core-only Classic and L4S equations;
- delivery bounds varied independently from wire playout while queues, RTX,
  probes, and other senders are active;
- equal/skewed/changing caller-selected rates without RTC redistribution;
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

- PulseBeam policy never mutates the SCReAM-native queue-delay target;
- after five smoothed RTTs of warmup, each designated stable interval spans at
  least `max(20 smoothed RTTs, 10 s)` and contains at least 1,000 delivered RTP
  queue samples. Compare each shared-bottleneck sojourn to the native target at
  that unit's enqueue; nearest-rank p99 of the differences is at most `10 ms`.
  Exclude path-step through five RTTs and probe-start through two RTTs after
  probe end from stable queue claims only; report losses and transients separately;
- native aggregate budget and emitted transport contract and recover across
  capacity steps without changing caller-selected targets;
- delivered RTP transport from emission cohorts uses at least `85%` of available
  bottleneck service in designated stable, non-demand-limited intervals, including
  the `2 Mbit/s`, `50 ms` RTT, `4 Mbit/s` demand single-flow baseline;
- two independent, equally backlogged PulseBeam connections on one `4 Mbit/s`,
  `50 ms` RTT bottleneck each receive `40%`–`60%` of combined delivered RTP
  transport and together use at least `85%` of bottleneck service; heterogeneous
  modeled competitors do not inherit this equal-share threshold;
- probe admission checks `P <= floor(N / 19) + A` in the trailing five-second window at every emission, using the profile's ordinary or bounded initial allowance; spent initial bytes remain bounded while aging out and cannot grant new ordinary credit. Raw probe ratio and allowance usage are reported separately;
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

The sole client acceptance baseline is pinned libwebrtc `0.6.2` in controlled
execution. It MUST establish authenticated bidirectional media, actual native
TWCC reaching RTC send accounting, DataChannel delivery/coexistence, and
bounded lifecycle behavior. Simulation and component tests retain quantitative
proof for loss/reordering, padding, priorities, pause/resume, VBR, source-switch
continuity, malformed input, congestion safety, and resource bounds.

RFC 8888 negotiation/parsing/accounting and ICE-TCP framing retain client-free
deterministic RTC component evidence, not production-server integration or
additional client compatibility. Active-client ICE-TCP native interoperability
is separately scoped to the controlled packet-socket boundary in `design.md`.
Stored SDP cannot prove runtime behavior or browser compatibility. Ericsson
traces are comparison oracles, not production dependencies or client evidence.
A PulseBeam deviation is accepted only when the same deterministic scenario
demonstrates its intended latency/quality benefit without violating congestion
safety or fixed bounds.

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
