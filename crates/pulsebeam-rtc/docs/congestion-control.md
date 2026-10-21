# Congestion control and aggregate demand

This is the private SCReAM-derived implementation contract for the lean
`pulsebeam-rtc` transport. It does not introduce public controller types,
sender assignments, delivery bounds, source rates, or a second admission budget.
The [connection contract](design.md) defines the public boundary.

The words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative.
Public Rust definitions and executable owner proof remain authoritative.

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


## Public demand and estimate

`set_desired_bitrate(Option<MediaPayloadBitrate>)` is one connection-level
application-demand hint. It is not an allocation, reservation, source binding,
or promise of spare path capacity. A positive hint supplies the draft's
application maximum target rate and can enable source-free negotiated RTX
probing. It does not create per-sender targets.

`None` and zero are neutral. The controller uses its existing native operating
ceiling rather than disabling ordinary RTP. Speculative padding requires a
positive hint; actual primary media and feedback can establish service without
one. Clearing a hint does not reset congestion, numbering, sent history, or
spent pacing service.

The native target is exposed as `Event::BandwidthEstimate` and
`ConnectionStats.bandwidth_estimate`. It is a media-payload rate estimate, not
a `MediaBudget`, IP/TCP wire capacity, or a reservation to redistribute. There
is no per-source rate admission, selected/potential-rate model, or duplicate
aggregate payload pacer.

### Explicit transport adaptations

1. TWCC and RFC 8888 normalize to one packet-feedback model. Each connection
   selects one mode; acknowledgments are not counted in both.
2. The transport adapter derives feedback freshness, timing, confidence, actual
   offered rate, and application-limited state around the pinned core.
3. Ordinary RTP and repair use the native transport-byte pacer and send window.
   A bounded probe manager retains transport-byte cluster pacing and debt, not
   a second media-payload admission budget.
4. Equal-byte sender service, protocol control, repairs, channels, and lowest-
   priority padding share bounded scheduling. Frames, sources, decode targets,
   and playback usefulness are not scheduler inputs.
5. SCTP retains its own congestion/acknowledgment state and bounded queues.
   Coordination does not invent a combined capacity estimate or count SCTP as
   acknowledged RTP.
6. Committed packet retention and resend spacing bound ordinary repair. They
   never predict playback usefulness.

These adaptations make the engine SCReAM-v2-derived, not a promise of bit-for-bit
behavior with any libwebrtc or Ericsson build. Replacing it with GCC is not part
of the lean boundary.

## Units and send commits

Desired rate and bandwidth estimate are RTP **media-payload bits/s**. Media
payload excludes RTP headers/extensions, SRTP, RTX OSN, padding, RTCP, SCTP,
DTLS, ICE, RFC 4571 framing, and outer network headers.

At the public boundary, transport bytes are exactly `Transmit.payload.len()`:
a UDP datagram payload or an RFC 4571 frame including its length prefix.
They do not include kernel, IP, UDP/TCP, link, tunnel, or other unobservable
network overhead. Native RTP flight/window/pacing accounting uses emitted RTP
transport bytes. Queue admission conservatively estimates protection and
framing overhead before commit.

Returning `Transmit` commits sent identity, departure time, transport bytes,
classification, pacing service, and counters. Every acknowledged byte must
correspond to a previously committed unit. Local socket loss cannot roll that
commit back. Prepared work is not an application departure receipt.

Primary sequence indices are supplied by the caller and may contain gaps, wrap,
and valid late holes. The caller must not reuse an accepted original index under
the same sender/key. RTC-owned RTX and TWCC spaces progress independently.
Discarding pending originals resets none of those committed histories.

## Packet feedback

Outbound RTP requires exactly one negotiated packet-feedback mode. TWCC and
RFC 8888 share validation, bounded history, once-only accounting, and controller
conversion. Their receiver-relative timelines are not wall clocks. Actual
arrival remains separate from delayed execution, with ordinary `Instant`
inputs. The initial protocol NTP anchor does not alter feedback deltas.

### TWCC

One transport-wide sequence space covers bundled original RTP, RTX, and padding.
Committed entries retain extended identity, send time, emitted length, path
epoch, and packet class. Generation/reordering windows reject unknown or stale
feedback without advancing acknowledgment state.

The ACK edge advances to the highest received identity only after validating
the complete retained interval. Covered gaps may earn once-only span credit,
but not delivery or confirmed loss. All-missing reports do not move that edge,
acknowledge flight, or start a reordering deadline. Duplicate/reordered reports
cannot acknowledge bytes twice.

Loss confirmation uses the bounded reordering deadline anchored to a higher
received identity, or five-second history expiry. Late receipts and expired
entries must not refresh stale-report evidence or replace newer feedback hold.

### RFC 8888

Acknowledgment progression is per SSRC, keyed by outbound SSRC and extended RTP
sequence. There is no invented connection-wide RTP acknowledgment cursor.
Report-time freshness, timestamp generations, sent-entry identity, loss status,
and relative arrival fields are validated before accounting.

Parser/report/accounting coverage includes unavailable arrival sentinels, sparse
reports, wrapping, late receipts, duplicate reports, ECN fields, resource
exhaustion, and path epochs. This is component coverage, not a claim that the
current native client exercises RFC 8888.

### Path changes and stale feedback

Accepted feedback is fenced by selected-path epoch. Replacement invalidates the
delay baseline and old-path feedback without resetting crypto identity or
allowing old receipts to fund new growth. Timers can retire history or apply
pending reduction, but cannot replay ACK credit for growth.

Feedback freshness follows useful network evidence, not merely packet count,
duplicate receipt, report replay, or unknown identities. The no-RTX native
outage proof uses actual primary RTP after feedback returns; it does not invent
padding identities to regain service.

## SCReAM core

The private adapter supplies committed RTP evidence, normalized feedback,
selected-path state, bytes in flight, queue state, actual offered payload rate,
and aggregate desired rate. The core receives accepted arrival/loss evidence,
ACK-edge credit, committed flight, RTT/feedback hold, path reset, disabled
production ECN mode, and the draft application maximum target rate. Routing,
frames, raw playout ranges, and SCTP sequence/SACK state are not core inputs.

The output contains the native media-payload target, transport pacing rate,
send window, queue-delay target, delay/confidence observations, RTT, feedback
hold/freshness, delivered transport rate, and application-limited state.
This is private transport state, not a public bandwidth allocator.

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

The adapter measures actual validated payload offered over bounded 200 ms
buckets, including structurally valid attempts rejected for a full queue.
Configured demand and queue nonemptiness do not substitute for measured offer.
Below 85% of the lower of positive desired demand and native target, with no
eligible RTP held by pacing/window, the connection can be application-limited.
A neutral hint uses the native target for that comparison.

Application-limited state governs outer confidence and probes, not a separate
freeze of the draft growth equation. Sparse samples do not rapidly move the
delay baseline. The last credible rate is retained while confidence decays with
a five-second half-life. Real returning media and fresh feedback, not a source
assignment, restore ordinary service.

### Native queue target and ECN

The pinned core owns its queue-delay target, starting from 60 ms and allowing its
native competing-flow adaptation up to 400 ms. RTC does not cap or replace that
target with playout settings, frame deadlines, or source-rate policy. A private
effective-target observation equals the native target; it is not a second input.

Production uses delay and loss. ECN is disabled in both negotiated feedback
modes even when RFC 8888 reports marks. The private core retains Classic/L4S
equations for independent tests. Live ECN requires a separate validation and
marking change; there is no public deployment switch.

## Queues, scheduling, and repair

Admission checks negotiated stream/extension facts, the primary crypto-index
limit, packet-envelope size, and connection packet/payload/transport bounds.
A full queue returns owned work. There is no source-rate policer, age admission,
frame grouping, decoder-ready state, or dependency-aware shedding.

Original queues are FIFO per sender, with equal-byte service quanta across
eligible senders. Native pacing and the complete-packet send-window check remain
network safety gates. Protocol progression remains available under load. RTP and
SCTP use bounded service arbitration; SCTP bytes never enter RTP history.

Committed originals with negotiated RTX may be retained under configured byte
bounds and a three-second mechanical age bound. RTT-based resend spacing
prevents tight repeated repair. Queue discard preserves retained repairs and
RTX numbering. These bounds are storage/protocol constraints, not a statement
that a frame remains useful to playback. Graceful shutdown and abort can discard
media without making delivery promises.

## Probing

Padding requires positive aggregate demand, a viable path/window, and negotiated
RTX. It uses that sender's RTX SSRC/payload mapping, MID/TWCC, and continuous RTX
index space. It never consumes or invents primary indices, sends an SSRC-zero
stream, or requires primary media to bind a source first. Without RTX, actual
primary media supplies bootstrap and recovery.

Queued originals, pending repairs, and eligible channel output take precedence
over speculative padding. Ordinary media/repair use the native pacer; probe
clusters use bounded transport pacing while preserving spent service and the
native send window. Resetting a queue or changing demand is not a credit refund.

The trailing five-second emission ledger enforces:

```text
probe_bytes <= floor(nonprobe_rtp_bytes / 19) + allowance
```

Ordinary allowance is 48 KiB. Initial source-free discovery has a bounded,
demand-sized allowance, at most 96 KiB, for the initial service period. Its spent
bytes cannot become fresh ordinary credit, and path replacement cannot rearm
the initial allowance. Ring overflow suppresses credit rather than evicting
safety evidence and granting a new burst.

Clusters retain requested service, transport bytes, packet count, deadline,
received/lost evidence, and their own result. Success requires the cluster's
receipts and quorum, not unrelated ACKs, logical gaps, or a controller estimate
alone. Native RTT/window bounds, stale feedback, loss, queue impact, and timeout
can constrain or abort a cluster. Retry, startup discovery, and bounded stale
feedback solicitation are distinct states, not repeated bootstrap resets.

Clearing demand prevents speculative emission. It does not prevent genuine
primary media or reset the estimate, debt, sent history, or RTX continuity.

## Evidence

Owner targets are linked from the [README](../README.md#contributor-verification).
Core scenario and feedback-history tests retain the pinned equations and
adversarial accounting proof. Controlled native connection tests establish:

* at least 85% stable delivered bottleneck utilization in designated saturated
  intervals, including the 2 Mbit/s single-flow baseline;
* contraction/recovery across 2/1/2 Mbit/s capacity steps without changing the
  aggregate demand hint;
* two independent backlogged connections each obtaining 40%-60% of combined
  delivered traffic on their shared bottleneck, with at least 85% total use;
* every committed RTP emission fitting the contemporaneous native send window,
  with delivered-packet queue-sojourn/target quantiles reported for that cohort;
* source-free RTX bootstrap, ordinary/initial probe allowance checks, and
  neutral-hint suppression;
* no-RTX feedback outage, selected-path replacement, explicit caller pause/
  queue discard, and resumed primary service without a hidden primary allocator;
* actual sparse-offer application-limited classification and flight observation
  before batched ACK retirement.

Public native tests separately cover fragmented media, direct payload/timing/
marker/extension values, wrap/late-hole authenticated receipts, ordinary repairs,
channel backpressure/coexistence, passive TCP, and bounded lifecycle. The checked-
in public network trace describes only the loss/delay/duplication/reordering,
sender-service, data, and malformed-input workload it actually runs. Capacity
steps and feedback outages are proved by their production-logic owner tests,
not inferred from trace names.

The former end-to-end 10 ms excess-sojourn bound depended on the deleted
payload-target pacer. It is not a guarantee of native transport pacing. The
isolated controller scenarios retain their original queue-equation oracles;
the native connection proof checks actual window/commit accounting and reports
queue observations without recreating that payload pacing policy.

Source switching, DD/keyframe/cache semantics, routing, node serialization, and
playback policy remain SFU proof. RTC does not infer physical capacity or media
quality from metadata-only assertions. The pinned native baseline is not browser,
Pion, RFC 8888 client, additional-platform, or production-cutover evidence.

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
