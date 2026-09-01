# Congestion control and latency governance

This document defines the egress congestion-control contract for
`pulsebeam-rtc`. It is an implementation design, not a public promise to match
one revision of SCReAM or libwebrtc exactly.

The connection owns one aggregate congestion controller, pacer, probe manager,
and latency governor. The SFU supplies aggregate media demand and one desired
playout-delay range. The SFU continues to choose sources and layers; it does not
set congestion-window gains, pacing factors, queue targets, probe clusters, or
per-stream bitrate allocations.

## Goals

- Keep interactive media near the live edge without sacrificing congestion
  safety or fairness.
- Turn one connection-wide playout-delay control into a useful, continuous
  quality/latency policy.
- Remain interoperable with ordinary Chrome and Firefox transport-wide
  congestion-control feedback.
- Stay responsive before media starts, during variable-rate media, and while
  every media stream is paused or application-limited.
- Treat actual socket departure, not packet construction or polling, as the
  start of network flight.
- Share one network estimate and one transport budget across every media and
  DataChannel stream on the connection.
- Keep all histories, queues, and timers bounded and local to one `Connection`.
- Expose semantic bitrate and statistics, without exposing SCReAM, TWCC, pacer,
  or RTCP implementation types.

## Non-goals

- Controlling a browser's uplink encoder. The browser owns its send-side
  controller; this crate only generates ingress feedback for it.
- Selecting an SFU source, simulcast layer, SVC layer, or keyframe cache entry.
- Promising an exact capture-to-render deadline. Remote capture, decode,
  rendering, and jitter-buffer state are not fully observable.
- Mapping a playout-delay value directly to a network queue target.
- Depending on L4S, ECN, RFC 8888 feedback, synchronized endpoint clocks, or a
  modified browser for baseline operation.
- Offering public expert knobs for controller gains or scheduler priorities.

## The central distinction

The playout-delay extension and congestion control describe related but
different things.

| Quantity | Meaning | Owner |
| --- | --- | --- |
| Playout minimum | Best-effort minimum capture-to-render delay requested from the receiver | SFU policy, signaled by the connection |
| Playout maximum | Best-effort maximum capture-to-render delay requested from the receiver | SFU policy, signaled by the connection |
| End-to-end latency | Capture through encode, uplink, SFU, downlink, decode, and render | Not fully observable by this crate |
| Forwarding delay | Ingress socket receipt through egress socket departure | Measured exactly within one clock domain |
| Network queue delay | Variable delay above the estimated path baseline | Estimated by the SCReAM core |
| Pacer delay | Predicted time before an admitted packet can depart | Owned by the connection |
| Recovery horizon | Time during which retransmission is likely to improve rendered quality | Chosen by the latency governor |

The playout maximum is an intent and an upper envelope, not a target to fill.
A larger maximum must never cause the connection to deliberately build a
similarly large network queue. Extra latency budget is spent first on receiver
smoothing and useful recovery, not bufferbloat.

The `min = 0, max = 0` value is a special request to render as soon as
possible. It does not mean that transport, decode, or render can complete in
zero time, and it must not produce a zero-sized congestion window or a literal
zero-duration packet deadline.

## Architecture

```text
                         connection-wide playout range
                                      |
                                      v
SFU sending/desired demand ---> latency governor <--- packet age and frame state
                                      |
                       operating point and deadlines
                                      |
                                      v
departure receipts ---> SCReAM v2 core ---> safe rate/window ---> pacer/scheduler
        ^                     ^                                    |
        |                     |                                    v
        +------------- TWCC feedback <---------------------- socket transmits
```

The design has four cooperating parts:

1. The SCReAM core estimates a network-safe reference window and sustainable
   rate from actual departures and packet feedback.
2. The latency governor translates playout intent and current conditions into
   a bounded operating point for the controller, pacer, retransmission engine,
   and media admission.
3. The probe manager creates SSRC-zero padding when ordinary media does not
   provide enough observations.
4. The scheduler enforces the resulting transport budget across all connection
   traffic and applies frame-aware media shedding.

These are internal components of one connection. They do not run independent
timers or communicate through callbacks. Their next actions contribute to the
single deadline returned by `Connection::poll`.

## Public contract

The public API remains algorithm-neutral. Conceptually, the SFU can:

```text
set_playout_delay(min, max)
set_bitrate_demand(sending, desired)
send_media(sender_id, packet)
report_departure(receipt, sent_or_dropped, observed_at)
poll(now) -> Transmit | Event | Idle { next_wakeup }
stats() -> coherent snapshot
```

The exact Rust names can evolve, but the ownership must not.

- Playout delay is one connection-wide value. When the RTP extension is
  negotiated, the connection writes the same range to every egress video
  sender and repeats a changed value until RTCP proves a carrying packet was
  received. When the extension is absent, the local latency policy still
  applies and statistics report that it was not signaled.
- Bitrate demand has aggregate `sending` and `desired` rates. `sending`
  describes admitted media production; `desired` describes what the SFU could
  use if more bandwidth were available.
- Available bitrate events report the governed allocation rate, not an
  unconstrained raw estimator value. Diagnostic statistics can expose both.
- There is no public controller-selection enum. SCReAM revisions and PulseBeam
  tuning are implementation details.

## Feedback and departure truth

### Baseline feedback

The production baseline is transport-wide congestion-control feedback. It
provides transport sequence numbers, receiver arrival deltas, and loss across
the bundled connection. SCReAM is a sender algorithm; the remote browser does
not need to run SCReAM.

Egress video negotiation rejects the absence of required packet-level feedback
with a specific error. It does not silently fall back to REMB or a fixed-rate
controller.

RFC 8888 congestion-control feedback can later be normalized into the same
internal packet-feedback representation. It is negotiated instead of TWCC,
not concurrently with it. ECN and L4S behavior must remain disabled unless the
selected feedback format actually reports ECN markings and the path proves it
preserves them. Baseline SCReAM behavior must be excellent without ECN.

### Departure receipts

A packet is in flight only after the caller reports a successful socket send.
Every emitted transmit therefore carries an opaque receipt.

- Successful departure records the observed departure time, wire size,
  transport sequence number, probe identity, and current in-flight bytes.
- A socket drop before departure never enters TWCC sent history and never
  increases bytes in flight.
- A missing receipt is abandoned after a bounded deadline and counted. It
  cannot remain in flight forever.
- Batched socket sends can return batched receipts, but each packet retains its
  own observed result and timestamp.

RTP sequence numbers can already have been committed when post-admission media
is lost, so such loss remains visible as an RTP gap. TWCC sequence numbers are
assigned only to packets that are about to be emitted and are entered into sent
history only after successful departure.

## SCReAM v2 core

PulseBeam implements the controller in Rust using the current SCReAM v2 draft,
the Ericsson reference implementation, and libwebrtc behavior as guidance. It
does not link either implementation and does not inherit their public APIs or
unstable constants.

The core is one aggregate controller per connection. Multiple RTP senders,
RTX, probes, and congestion-controlled DataChannel bytes share the same path
state. There is no per-sender congestion window and no coupled-controller layer
above several independent estimators.

### Inputs

The core consumes:

- Successfully departed packet time, wire size, and transport sequence number.
- Packet arrival or loss reported by TWCC.
- Feedback receipt time and estimated receiver feedback-hold time.
- Current bytes in flight and paced queue state.
- Network availability and selected-path changes.
- Aggregate sending and desired bitrate.
- Application-limited state from actual offered traffic, not merely a low media
  rate.
- An internal latency operating point from the governor.

### Network model

The delay estimator tracks a baseline one-way delay and treats delay above that
baseline as queueing. It must tolerate endpoint clock offset, gradual clock
drift, feedback batching, packet reordering, and wireless scheduling jitter.
Synchronized endpoint clocks are not required.

Conceptually:

```text
relative_one_way_delay = receiver_arrival_time - sender_departure_time
queue_delay = relative_one_way_delay - rolling_path_baseline
```

Only differences and trends are meaningful across endpoint clocks. The
implementation maintains bounded rolling minima and smoothed variation rather
than interpreting the absolute value as wall-clock latency. A path change
invalidates the baseline.

Feedback RTT is separated into network loop time and receiver feedback hold
where the feedback format allows it. Sparse application-limited samples must
not rapidly rewrite either estimate.

### Reference window and rate

The reference window is the network-safety state. It grows only when feedback
supports growth and falls in response to persistent queue growth, congestion
loss, or valid ECN signals. Random isolated wireless loss must not be treated
the same as sustained congestion loss, but loss filtering cannot hide repeated
loss or an overflowing queue.

The send window is a bounded function of the reference window. The target rate
is derived from the reference window, smoothed feedback loop time, delivery
rate, and configured constraints. The pacer can serialize a burst faster than
the long-term allocation rate, but it cannot exceed the allowed in-flight
window or manufacture average capacity.

The core exposes an internal safe envelope containing at least:

- Sustainable target rate.
- Maximum bytes in flight.
- Pacing-rate ceiling.
- Estimated queue delay and its confidence.
- Smoothed RTT and feedback-hold time.
- Delivery and loss observations.
- Application-limited and feedback-stale state.

### Application-limited behavior

When admitted traffic is below the safe rate, the connection is
application-limited. This includes paused video, low-complexity VBR scenes, and
audio-only periods.

Application-limited operation must:

- Stop unobserved reference-window growth.
- Prevent sparse RTT and delay samples from rapidly moving the path baseline.
- Preserve the last credible capacity estimate with decaying confidence rather
  than immediately collapsing it to the media rate.
- Use bounded periodic probes when demand indicates that retained or higher
  capacity would be useful.
- Resume normal estimation promptly when media returns.

The media bitrate and available network bitrate are never treated as the same
quantity.

### Stability parameters

Loss backoff, ECN response, baseline filters, clock-drift handling, reference
window minimums, estimator gains, and fairness rules are controller stability
parameters. They are versioned implementation constants backed by trace and
simulation evidence. The playout-delay control must not tune them per
connection.

## Latency governor

The governor turns user intent into a safe operating point. It is not a second
bandwidth estimator and cannot grant capacity that SCReAM has not established.

### Inputs

The governor observes:

- Requested playout minimum and maximum.
- Whether the request has been acknowledged by the receiver.
- Ingress receive time and current age of each forwarded packet.
- Current and predicted pacer delay.
- Smoothed RTT, queue delay, feedback hold, and estimator confidence.
- Recent video frame-size and burst variation.
- Current sending and desired media rates.
- Retransmission age and whether its original frame is still useful.
- Frame boundary, keyframe, dependency, audio, retransmission, and probe class.
- Queue occupancy and the safe envelope from SCReAM.

Ingress receive time is an exact forwarding-latency origin, not capture time.
When trustworthy absolute-capture-time metadata exists it can improve
diagnostics, but the governor must remain correct for opaque media and clients
that do not provide it.

### Outputs

The governor chooses an internal `LatencyOperatingPoint` with:

- A SCReAM queue-delay target within fixed safety bounds.
- A media-utilization fraction below the safe target rate.
- Pacing headroom within the safe send window.
- Maximum paced queue horizon.
- New-frame admission horizon.
- Retransmission usefulness horizon.
- Probe intensity and allowed probe queue impact.
- Stale-media shedding thresholds.
- The aggregate available bitrate reported to the SFU.

The structure is internal. The SFU supplies intent, not these values.

### What playout delay may tune

| Tunable | Lower-latency direction | Higher-quality direction |
| --- | --- | --- |
| Queue-delay target | Lower, subject to a jitter-tolerant floor | May rise modestly, subject to a low fixed cap |
| Allocation utilization | More capacity held as VBR headroom | Operate closer to the safe rate |
| Pacing headroom | Drain admitted bursts promptly within the send window | Smoother serialization is acceptable |
| Pacer horizon | Shorter | Longer, but still bounded independently of playout maximum |
| Frame admission | Reject predicted-late new frames earlier | Admit more frames when recovery remains useful |
| RTX horizon | Retransmit only when very likely to arrive usefully | Permit more loss recovery |
| Probe impact | Smaller queue footprint and faster abort | More evidence can be gathered when slack permits |

The governor must not map `playout_max` linearly to `queue_delay_target`.
Network queue delay remains deliberately small at every quality setting.

### Interpreting minimum and maximum

The maximum expresses urgency. A smaller maximum reduces queue horizons,
allocation utilization, and recovery time. A larger maximum allows more
quality recovery, but does not authorize a larger congestion window than the
network feedback supports.

The minimum expresses intentional receiver smoothing. A retransmission
predicted to arrive before the minimum is more likely to improve quality
without delaying render. The minimum is not free network budget: capture and
uplink time have already consumed part of the end-to-end interval.

The width of the range indicates how much freedom the receiver has to adapt its
jitter buffer. A fixed nonzero range (`min == max`) asks for stable playback;
the governor should avoid latency oscillation even when it could temporarily
drain faster.

### Predicted usefulness

For each queued packet or retransmission, the governor estimates:

```text
forwarding_age = now - ingress_received_at
predicted_departure = now + estimated_pacer_wait + serialization_cost
predicted_network_cost = path_delay_estimate + uncertainty_margin
predicted_forwarding_cost = forwarding_age
                          + estimated_pacer_wait
                          + serialization_cost
                          + predicted_network_cost
```

This is not claimed as capture-to-render latency. It is a conservative ranking
and admission signal. Unknown decode/render cost and capture-to-ingress time
remain outside the model.

The governor prefers dropping a complete frame that has not begun transmission.
Cut-through forwarding means later pressure can still lose a packet from a
started frame. In that case boundedness wins, the RTP gap remains visible, and
statistics record post-admission damage. The controller must never create an
unbounded commitment to finish an unknown-sized frame.

### Operating-point derivation

The governor derives a connection operating point separately from per-packet
usefulness. It does not continuously retune SCReAM for each packet.

For a nonzero playout maximum, it first forms a conservative remaining-slack
estimate from:

```text
observed_transport_floor = path_delay_estimate
                         + receiver_feedback_uncertainty
                         + serialization_floor

policy_slack = playout_max
             - observed_transport_floor
             - decode_and_render_reserve
             - unobserved_capture_and_uplink_reserve
```

The last two reserves are uncertainty allowances, not claims that those remote
costs are measured. Their policy curves and bounds are implementation tuning
backed by browser and trace evidence. Missing or stale observations increase
the reserves; uncertainty can never create apparent extra slack.

`policy_slack` is mapped through a bounded monotonic curve to an urgency value.
The curve has saturation at both ends: very large playout values stop relaxing
network behavior after a quality-oriented cap, and values below the observable
transport floor all produce the most urgent operating point. `0/0` bypasses
the calculation and selects that most urgent point directly.

Conceptually:

```text
urgency = urgency_curve(policy_slack, estimator_confidence)

queue_delay_target = bounded_inverse_map(urgency,
                                         low_queue_target,
                                         quality_queue_cap)

allocation_utilization = bounded_inverse_map(urgency,
                                             low_latency_utilization,
                                             quality_utilization_cap)

pacer_horizon = bounded_inverse_map(urgency,
                                    low_latency_horizon,
                                    quality_horizon_cap)

recovery_horizon = min(pacer_horizon + useful_rtx_allowance,
                       policy_slack_bound)
```

The symbolic bounds are private, versioned tuning values. They are not public
configuration and are not inferred by simply multiplying `playout_max`. The
relationships and saturation behavior are the contract.

The base operating point changes on playout-policy updates, meaningful path
estimate changes, congestion transitions, and application-limited transitions.
Small feedback noise is handled inside SCReAM and must not churn the governor.
Downward safety changes apply immediately; upward changes are rate-limited over
feedback rounds.

Per-packet admission then combines the stable operating point with packet age,
predicted departure, frame state, and recovery cost. This two-level design
prevents an old packet from globally forcing the network controller into a
different mode while still allowing that packet to be rejected as no longer
useful.

The playout minimum affects recovery classification after the operating point
is selected. A packet predicted to arrive before the minimum is considered
comfortably recoverable. Arrival between minimum and maximum is best-effort and
weighted by estimator confidence. Predicted arrival after the maximum is stale.
None of these classifications can override the SCReAM safe window.

### Required monotonicity

For identical network and traffic state, reducing the playout maximum must
never:

- Increase the SCReAM queue-delay target.
- Increase media allocation utilization.
- Increase pacer or admission horizons.
- Extend retransmission usefulness.
- Admit an older video frame that the looser policy rejected.
- Permit a probe with greater predicted queue impact.

Increasing the playout maximum may relax those limits gradually, but can never
increase the safe congestion window directly. These properties are tested over
the full supported playout range, not only named example profiles.

### Dynamic changes

Tightening takes effect immediately:

- The new playout value is scheduled for signaling.
- Not-yet-started queued video is re-evaluated.
- Obsolete retransmissions are removed.
- Queue and probe horizons shrink.
- The governed available bitrate can fall immediately.

Relaxing is damped over feedback rounds. It must not release a burst, jump the
reference window, or instantly trust a stale application-limited estimate.
Already dropped media is never reconstructed merely because the policy became
looser.

## Pacing and admission

The SCReAM safe rate, reference window, and latency operating point jointly
control pacing.

- Protocol control remains deliverable under media load.
- Audio is protected from video bursts.
- Retransmissions are admitted only while useful and still count against the
  connection's congestion budget.
- DataChannel traffic is congestion-accounted and bounded; it cannot consume
  reserved control or audio service indefinitely.
- Video uses packet-level cut-through forwarding and frame-aware admission.
- Padding is always lowest-value traffic and disappears immediately when real
  traffic or congestion needs the budget.

Pacing headroom permits short encoder or forwarded-frame bursts to drain
without setting the long-term media allocation equal to the burst rate. It is
always constrained by bytes in flight and current delay signals. A low-latency
policy should normally reserve more average-rate headroom while allowing safe
short serialization bursts.

The bitrate exposed to the SFU is the rate at which the SFU can sustainably
admit media under the current latency policy. It can therefore be lower than
the raw SCReAM target. This is intentional: low latency is purchased partly by
leaving room for VBR bursts rather than filling every estimated bit with an
average layer allocation.

## Probing and silence

SCReAM v2 does not remove the need for active observations when the source is
silent. PulseBeam owns a probe manager integrated with the latency governor and
pacer.

### SSRC zero

- Egress SSRC-zero padding can probe after SRTP is ready and before the first
  media packet.
- Probe packets receive ordinary transport sequence numbers and are processed
  by the same feedback and departure machinery as media.
- An ingress SSRC-zero packet contributes arrival and transport feedback only.
  It never creates an encoding, sender, media event, RTP stream, or keyframe
  state.

### Probe lifecycle

Probes are considered when:

- The connection first becomes writable.
- Desired bitrate materially exceeds the credible available bitrate.
- VBR media has left the controller application-limited long enough that
  capacity confidence is decaying.
- Every media stream is paused but the SFU still has nonzero desired demand.
- A previous congestion response has stabilized and evidence is needed before
  recovering allocation.

A probe is bounded by the safe send window, current queue delay, playout-derived
queue-impact limit, and configured overhead budget. It aborts on delay growth,
loss, departure failure, stale feedback, or real media arrival that consumes the
budget. Probe traffic never forces admitted media past its latency horizon.

Pre-media probing must not depend on a prior media SSRC, prior RTP timestamp, or
packet history. Periodic probing must not inflate the estimate merely because
the application cannot produce the probed rate.

## Controller states

The implementation does not need a public state enum, but its behavior must be
equivalent to these phases:

1. **Unproven:** Transport is writable but no valid packet feedback exists.
   Use a conservative initial envelope and bounded SSRC-zero probing.
2. **Learning:** Initial or recovery observations are arriving. Increase only
   from delivered evidence.
3. **Steady:** Delay, loss, and delivery estimates are credible. Track the path
   and current latency operating point.
4. **Application-limited:** Offered traffic is insufficient to test capacity.
   Freeze unsupported growth and use demand-aware periodic probing.
5. **Congested:** Queue growth, sustained loss, or valid ECN requires backoff.
   Stop probes, reduce the window, and shed stale video.
6. **Feedback-stale:** Sent packets lack timely feedback. Stop growth and
   probes, reduce confidence, and apply a conservative bounded envelope.

A selected-path change resets path baseline, reference-window confidence,
in-flight history that cannot apply to the new path, and probe state. It retains
the SFU's playout intent and bitrate demand.

## Failure and ambiguity handling

- Reordered feedback updates delivery state once and cannot acknowledge the
  same bytes twice.
- Feedback for unknown or abandoned transport sequence numbers is ignored and
  counted without allocating state.
- A receiver feedback pause cannot increase available bitrate.
- A delayed feedback batch is separated from forward-path delay where possible;
  it must not be interpreted wholly as network queue growth.
- Clock drift or a persistent baseline shift triggers bounded baseline recovery,
  not indefinite queue-target inflation.
- Isolated random loss slows unsupported growth but does not necessarily cause
  a full congestion backoff. Persistent loss or loss combined with queue growth
  does.
- ECN bleaching or inconsistent ECN reporting disables ECN use. Baseline
  delay/loss control continues.
- A rate policer is treated as a path condition: probing becomes conservative
  and repeated burst loss cannot be answered by ever larger probes.
- Congestion-control failure alone is not a reason to emit unbounded per-packet
  events. Counters and coherent snapshots carry diagnostics.

## Data-oriented state and bounds

Controller state is connection-local and aggregate. Hot feedback processing
uses dense bounded rings indexed by unwrapped transport sequence number rather
than per-packet heap objects or unbounded maps.

At minimum, configuration bounds:

- Sent-packet history duration and entry count.
- Maximum acknowledged reordering window.
- Feedback report span.
- Outstanding departure receipts.
- Probe history and concurrent probe clusters.
- Pacer bytes, packets, and queue horizon.
- Retransmission history by bytes and age.
- Rate and delay sample windows.

The sent ring stores only facts needed by feedback, accounting, retransmission,
and metrics. Expiration advances monotonically. Wraparound is validated before
an entry is reused, with debug assertions for internal generation and sequence
invariants and release checks for network input.

No state is shared between connections. Scaling many connections is the
caller's scheduling problem; each connection contributes one earliest wakeup
and performs work proportional to new input, expired bounded state, or emitted
output.

## Statistics

Statistics are coherent snapshots. They do not register a metrics backend.
The snapshot should make controller behavior explainable without exposing
mutable internals.

Required connection-level observations include:

- Requested playout minimum and maximum, last change time, and whether the
  current value is acknowledged by the receiver.
- Raw SCReAM target rate and governed available bitrate.
- Sending and desired bitrate.
- Reference window, allowed bytes in flight, and actual bytes in flight.
- Pacing rate, paced queue bytes, predicted pacer delay, and oldest media age.
- Baseline delay, queue delay, smoothed RTT, feedback hold, delivery rate, and
  estimator confidence.
- Application-limited, feedback-stale, probing, and congestion state.
- Probe attempts, successful observations, sent bytes, aborted probes, and
  abort reasons.
- Reported, recovered, reordered, unknown, and congestion-classified loss.
- Pre-admission frame drops, post-admission packet drops, deadline misses, and
  retransmissions skipped as no longer useful.
- Successful, failed, missing, and late departure receipts.
- Whether TWCC or another normalized feedback mode is active and whether ECN is
  usable.

Per-transmit metadata retains ingress receive time through the departure
receipt so the caller can record arbitrary forwarding-latency distributions.
The crate may also expose cumulative delay buckets, but those are not a
substitute for the receipt correlation needed by rich metrics.

## Validation

The implementation is accepted through deterministic crate-local simulation,
component comparison, and live browsers. Workspace tests are outside this
project.

### Property tests

- Playout-policy monotonicity across the full valid range.
- No successful departure means no TWCC sent-history entry or in-flight bytes.
- Every acknowledged byte was previously reported as departed exactly once.
- In-flight bytes remain within the active send-window policy, except for
  explicitly bounded accounting races around batched departure results.
- Tightening latency cannot retain a frame or retransmission rejected by the
  same state under a looser policy.
- Application-limited traffic cannot cause unsupported window growth.
- Unknown, duplicate, reordered, and wrapped feedback cannot corrupt the sent
  ring.
- One connection always exposes one next wakeup regardless of stream count.

### Deterministic network scenarios

- Startup with media, startup before media, and delayed first feedback.
- Constant-rate, strongly VBR, large keyframe, screen-share, audio-only, and
  fully paused traffic.
- Bandwidth steps in both directions and rapid wireless capacity variation.
- Low and high RTT, changing RTT, receiver feedback batching, and feedback loss.
- Uniform random loss, burst loss, reordering, duplication, and path-MTU-sized
  packets.
- Tail-drop queues, active queue management, rate policers, and competing
  CUBIC/BBR-like flows.
- Optional L4S/ECN paths plus bleaching and fallback to non-ECN behavior.
- Playout range tightened and relaxed while queues, retransmissions, and probes
  are active.
- Multiple simultaneous audio/video senders sharing one connection budget.
- Missing, failed, late, and batched departure receipts.
- Selected-path replacement and delay-baseline reset.

The primary outcomes are delivered useful media, p50/p95/p99 forwarding delay,
network queue delay, freeze duration, startup and post-pause convergence,
deadline damage, probe overhead, loss, fairness, and utilization. Pass
thresholds must be fixed against the trace corpus before tuning a candidate so
tests do not simply encode the latest constants.

### External evidence

Live Chrome and Firefox sessions must prove negotiation, TWCC feedback, RTP and
RTX behavior, pre-media SSRC-zero probing, playout-delay updates, pause/resume,
and sustained VBR forwarding. Stored SDP cannot prove these behaviors.

GCC and current libwebrtc SCReAM traces are comparison oracles, not production
dependencies and not independent browser evidence. PulseBeam-specific behavior
is accepted only when the same deterministic scenario demonstrates the intended
latency/quality improvement without a congestion-safety regression.

## References

- [SCReAM v2 Internet-Draft](https://datatracker.ietf.org/doc/draft-ietf-ccwg-rfc8298bis-screamv2/)
- [Ericsson SCReAM reference implementation](https://github.com/EricssonResearch/scream)
- [libwebrtc SCReAM implementation](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/congestion_controller/scream/)
- [libwebrtc SCReAM implementation differences](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/congestion_controller/scream/g3doc/implementation_diff.md)
- [libwebrtc transport-wide congestion-control extension](https://webrtc.googlesource.com/src/+/refs/heads/main/docs/native-code/rtp-hdrext/transport-wide-cc-02/README.md)
- [RFC 8888 congestion-control feedback](https://www.rfc-editor.org/rfc/rfc8888.html)
- [libwebrtc playout-delay extension](https://webrtc.googlesource.com/src/+/refs/heads/main/docs/native-code/rtp-hdrext/playout-delay/README.md)
