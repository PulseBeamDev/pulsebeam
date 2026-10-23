# Congestion control and aggregate demand

This contributor reference describes the private Rust SCReAMv2 implementation in
`pulsebeam-rtc`. To review its provenance, run
`crates/pulsebeam-rtc/scripts/verify-scream-profile.sh` from the repository root,
then use the [owner verification targets](../README.md#contributor-verification).
The script checks source identity, not Rust behavioral conformance.

The [connection contract](design.md) defines the public boundary. This document
does not introduce controller configuration, source assignments, frame policy,
or a second admission budget.

## Pinned implementation

The implementation reference is libwebrtc commit
[`326fd06bb4d93900ed6947205b94cefd2413afca`](https://webrtc.googlesource.com/src/+/326fd06bb4d93900ed6947205b94cefd2413afca/modules/congestion_controller/scream/).
The production controller, feedback/delay/loss helpers, network-controller
adapter and RTP debt pacer are Rust ports. Production does not link the C++
implementation. Native peers in tests remain independent compatibility fixtures.

The port follows the pinned implementation and its default parameters, including
its documented differences from the SCReAM Internet-Draft. This supersedes the
former draft-01-first profile, the older libwebrtc comparison pin, the local
confirmed-loss qualification, offered-rate ALR detector, probe clusters, trailing
five-second probe ledger and extra payload-rate pacer. Those are not fallback
policies. A newer draft or upstream revision does not silently change this pin.

Review authority is:

1. public Rust definitions and the ownership contract in the [README](../README.md)
   and [design](design.md);
2. the explicit lean-transport adaptations below;
3. the pinned native implementation, defaults and implementation-difference notes;
4. the cited protocol specifications.

The source map is:

| Pinned source | Rust owner |
| --- | --- |
| `scream_v2.{cc,h}` | `src/congestion/screamv2.rs` |
| `scream_v2_parameters.{cc,h}` | `src/congestion/screamv2/parameters.rs` |
| `scream_feedback.{cc,h}` | `src/congestion/screamv2/feedback.rs` |
| `delay_based_congestion_control.{cc,h}` | `src/congestion/screamv2/delay.rs` |
| `loss_estimator.{cc,h}` | `src/congestion/screamv2/loss.rs` |
| `scream_network_controller.{cc,h}` | `src/congestion.rs` |
| `modules/congestion_controller/rtp/transport_feedback_adapter.cc` and `api/transport/network_types.h` | `src/sent_history/feedback.rs` |
| `modules/pacing/pacing_controller.{cc,h}` | `src/pacer.rs` and `src/egress.rs` |

SCReAM sources are under `modules/congestion_controller/scream/` upstream. The
verification script pins their byte lengths and SHA-256 digests, the corresponding
upstream tests and implementation-difference document, transport feedback flags,
the pacer sources, caller bitrate-allocation rules, and root `LICENSE`/`PATENTS`. It also checks the retained notices in
`src/congestion/screamv2/`. Optionally pass a local libwebrtc source-root directory
to verify the same artifacts without network access. Neither mode compiles C++.

## Demand, target and units

`set_desired_bitrate(Option<MediaPayloadBitrate>)` is one aggregate application-
demand hint. It is not an allocation, source binding, reservation or observation
of path capacity. The adapter uses it as aggregate allocation input for native
padding limits, not as a replacement for the controller's measured target.
Ordinary primary RTP remains sendable with `None` or zero; speculative padding
requires positive demand. Hint and queue changes do not restart congestion state,
numbering, feedback history or spent pacing service.

`Event::BandwidthEstimate` and `ConnectionStats.bandwidth_estimate` expose the
native **nominal media-payload target**. They do not expose an independent
physical-capacity estimator. Native encoder pushback, its reduction ratio and
pause-to-zero instruction are separate private outputs: pausing an encoder does
not erase the nominal target. The caller still owns allocation and frame QoS.

Desired rate and public estimate use RTP media-payload bits/s. Payload excludes
headers/extensions, RTX OSN, padding, SRTP, RTCP, SCTP, DTLS, ICE, RFC 4571 framing
and outer network headers. Native flight, send-window and pacing accounting use
emitted RTP transport bytes. At the public boundary, a transport unit is exactly
`Transmit.payload.len()`, including TCP framing when present, not unobservable
IP, UDP/TCP, tunnel or link overhead. Admission estimates protection/framing
conservatively until the actual committed size is known.

Returning `Transmit` commits identity, departure time, transport bytes,
classification, pacing service and counters. Prepared work is not departure.
Local socket loss cannot undo a returned commit. Every credited byte must refer
to committed output. Caller-supplied primary sequence indices can contain gaps,
wrap and valid late holes, but cannot reuse an accepted index under one sender
and key. RTC owns continuous RTX and TWCC numbering independently.

## SFU producer adaptation

Source and layer allocation remain in `pulsebeam`, outside RTC. The SFU retains
its priority, protected-minimum and fairness policies. It maps the cheapest
eligible encoded base to aggregate minimum service: a positive nominal target
below every eligible base still admits one base; zero suspends service. This
preserves native enforced-minimum behavior without changing the estimate or
pretending the SFU can re-encode a cheaper stream.

A previously paused slot needs its selected base cost plus the greater of
10% (truncated to whole bits/s) or 20 kbps, following pinned
[`MinBitrateWithHysteresis`](https://webrtc.googlesource.com/src/+/326fd06bb4d93900ed6947205b94cefd2413afca/call/bitrate_allocator.cc).
There is no timed resume dwell and no guarantee against a later pause when the
budget falls. These costs come from the SFU's changing encoded-layer statistics,
not a native encoder's fixed configured minimum. No encoder protection-rate
observer is synthesized for opaque forwarded media. Complete originals, repairs
and padding remain subject to RTC's independent native wire accounting.

## Native control and lean adaptations

### Feedback and path evidence

Each connection selects one negotiated packet-feedback mode. TWCC and RFC 8888
normalize to the same bounded committed-packet history; they do not both credit
one departure. TWCC uses the bundled transport sequence space. RFC 8888 uses
per-SSRC RTP progression rather than an invented connection-wide RTP cursor.

Validation covers retained identity, report generations, receiver-relative
arrival timestamps, unknown/future gaps, duplicates, replay, bounded reordering
and selected-path epochs. Relative receiver clocks are not wall clocks. Recorded
arrival time remains distinct from delayed callback execution.

Highest-received ACK-edge credit, flight retirement, terminal transport loss and
native loss/recovery events are separate facts. Covered gaps can earn once-only
span credit without becoming delivered packets. At this pin, the native RFC 8888
adapter reports first loss immediately and first recovery on a later receipt;
the native TWCC adapter leaves both provenance flags unset. The Rust adapter
preserves that distinction rather than deriving extra SCReAM loss events from
TWCC holes. Ledger reordering and expiry still account for terminal loss in both
modes. Late real receipts remain eligible without refunding terminal loss or
spending ACK-edge credit twice. Pure replay and expired history do not create
fresh network evidence.

Selected-path replacement resets delay/window evidence while retaining native
route-reset target semantics, crypto identity and sequence continuity. Old-path
feedback cannot finance new-path growth. Timers can retire history and update
liveness, confidence and padding opportunities; they cannot manufacture ACKs,
delivery or capacity growth.

### Window, delay and application limitation

The core ports native reference-window growth/backoff, delay baselines and
filters, loss/recovery rounds, receive-rate observations, target/pacing equations
and application-limited classification. Flight peaks are observed before batched
ACK retirement. Native ALR uses receive rate and observed flight headroom, not
queued demand, a 200 ms offered-rate threshold or silence alone.

The reference window is internal controller state, not a public absolute
congestion window. The native send window includes its own reference-window
slack and MSS floor. Every complete original, repair or padding packet must fit
the contemporaneous send window. Queue limits and transport accounting do not
add a second delivery-feasibility or payload-rate budget.

The complete private defaults live in
[`parameters.rs`](../src/congestion/screamv2/parameters.rs). Important review
anchors are 60 ms queue-delay target, 25 ms virtual RTT, 1,280-byte MSS, three
loss rounds before loss backoff, native 0.9 ALR threshold, three-second periodic
padding duration and interval, and one-second post-congestion padding cooldown.
These are pinned defaults, not public tuning controls. Production ECN/L4S remains
disabled until transport validation establishes an eligible mode; parsing ECN
fields does not establish that validation. No separate policer classifier or
alternate GCC controller is substituted.

### One RTP pacer and bounded service

One transport-byte debt pacer serves originals, repairs and lowest-priority
padding. Reconfiguration preserves its processed-time cursor and unpaid byte
debt; elapsed service uses the current effective media rate when processing
resumes, as in native `SetPacerConfig`. It retains debt across hint/queue changes,
bounds catch-up and padding work, and
charges successful generic padding to **both** media and padding debt. Failed
padding generation does not claim a departure. There is no cluster ledger or
second probe pacer.

Like the native default pacer, it drains a long admitted queue at the greater of
configured pacing rate and `queued transport bytes / max(1 ms, 2 s - average
queue age)`. Queue age excludes path pauses and congestion, and its packet-time
sum is maintained without scanning queued packets. This changes effective media
pacing, not the controller target, padding rate or full-packet send window.

Native padding opportunities and allocation caps control speculative emission.
They do not guarantee that a packet is eligible: egress also requires a viable
path, negotiated RTX identity, no higher-priority queued work and full-packet
send-window room. Padding uses continuous RTX identity without inventing
primary media or requiring a source binding. Without RTX, startup and outage
recovery rely on real primary media and feedback.

Upstream's six-second initial-BWE fallback applies only when its maximum padding
allocation is zero. The lean API requires positive aggregate demand for all
speculative padding, so that no-allocation fallback is inapplicable here. The
retained upstream parameter does **not** authorize six seconds of continuous
padding, a fixed byte grant or a fresh bootstrap on source/queue/hint changes.
Positive-demand padding follows native periodic opportunities and congestion
cooldown.

Protocol control, equal-byte sender scheduling, repair retention/resend spacing
and channels remain bounded mechanical transport responsibilities. Frames,
keyframes, source priorities, decode targets, presentation timing and playback
usefulness remain caller responsibilities. SCTP has its own congestion/SACK state
and bounded queues, is excluded from RTP ACK history, and does not create a
combined-capacity estimator.

## Verification scope

The owner suite covers pinned core/default/adapter behavior, normalized feedback
and history adversaries, native pacer debt and commit accounting, actual sparse
media ALR, pre-ACK flight observation, and production connection/network paths.
Controlled connection proof includes capacity steps, shared-bottleneck fairness,
source-free RTX growth, no-RTX feedback outage, selected-path replacement, caller
pause/queue discard and primary-media recovery. Public native proof separately
covers authenticated packet facts, fragmentation, wrap/late holes, repairs,
channels, TCP and lifecycle.

Source digests establish provenance, not conformance. These tests are not claims
of bit-for-bit equivalence across integer units, operating systems or entire
libwebrtc builds. They are not browser, Pion, native RFC 8888 peer or whole-SFU
quality evidence. Source selection, starvation recovery, DD/keyframe/cache
semantics, identity/routing and media quality require their own SFU proof. A
nominal target alone cannot prove those properties or physical capacity.

## References

- [Pinned implementation differences](https://webrtc.googlesource.com/src/+/326fd06bb4d93900ed6947205b94cefd2413afca/modules/congestion_controller/scream/g3doc/implementation_diff.md)
- [Pinned native pacer](https://webrtc.googlesource.com/src/+/326fd06bb4d93900ed6947205b94cefd2413afca/modules/pacing/pacing_controller.cc)
- [SCReAM v2 draft 01, historical standards reference](https://datatracker.ietf.org/doc/html/draft-ietf-ccwg-rfc8298bis-screamv2-01)
- [RFC 3550: RTP/RTCP](https://www.rfc-editor.org/rfc/rfc3550.html)
- [RFC 8888: RTP congestion-control feedback](https://www.rfc-editor.org/rfc/rfc8888.html)
- [RFC 8835: WebRTC media transport priority](https://www.rfc-editor.org/rfc/rfc8835.html)
- [RFC 8831: WebRTC DataChannels](https://www.rfc-editor.org/rfc/rfc8831.html)
- [RFC 8832: DataChannel establishment](https://www.rfc-editor.org/rfc/rfc8832.html)
- [RFC 8260: SCTP stream scheduling and interleaving](https://www.rfc-editor.org/rfc/rfc8260.html)
