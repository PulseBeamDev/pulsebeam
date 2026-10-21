# Lean RTC connection contract

This document describes the independently exercised `pulsebeam-rtc` transport
engine. Production PulseBeam still uses direct str0m; this is not a production
cutover contract or a Rust-compatible replacement for str0m.

The public Rust definitions are authoritative. The [README](../README.md) gives
the shortest development path; [congestion control](congestion-control.md)
defines the private controller profile and quantitative proof.

## Ownership and scope

One `Connection` owns one peer's immutable initial negotiation and transport
state: ICE, DTLS, SRTP/SRTCP, RTP/RTCP, SCTP, feedback, congestion control,
pacing, ordinary retransmission, protocol timers, and mechanical resource
bounds. Protocol progress is explicit and connection-local. There is no hidden
transport task, cross-connection controller, global registry, or socket retry
queue.

The caller owns routing, allocation, source/layer selection, receiver sequence
and timestamp translation, frame/DD interpretation and rewriting, keyframe
caches, source switching, recovery and playback policy, global media
normalization, and node serialization. These responsibilities are not encoded
as RTC commands or moved into alternate RTC packet/frame types.

This crate exposes RTC-owned values, not public str0m, SCTP, or native-controller
types. The production SFU retains its native packet utilities and dependency
wiring. Neither side depends on completing a cutover first.

## Initial negotiation

`Connection::accept(config, offer, instant, system_time, entropy)` returns the
connection, SDP answer, and `SessionInfo`. The caller sends that answer through
its signaling mechanism. Supply valid local candidates and fresh cryptographic
entropy for every peer. Reusing entropy across peers is not an identity or key
management strategy.

The existing supported profile is:

* one immutable remote offer/local answer, ICE-lite, BUNDLE, RTCP mux, inline
  candidates, and all initial SDP media directions;
* UDP and passive ICE-TCP over IPv4 or IPv6, with active-client TCP covered at
  the controlled packet-socket boundary;
* the negotiated Opus/H.264 codec profile and directional RTP/RTX payload maps;
* DTLS 1.3 preferred, DTLS 1.2 compatibility fallback, preferred mutually
  supported AEAD SRTP, and the existing legacy SRTP compatibility profile;
* one supported outbound packet-feedback mode, TWCC or RFC 8888, including
  audio-only outbound RTP. There is no REMB or fixed-rate fallback.

Application-only sessions do not require RTP feedback. Rejected media sections
retain valid dynamic payload mappings in the answer. Trickle ICE, ICE restart,
renegotiation, a TURN client, additional codecs, and additional browser/platform
compatibility are not implied by this contract.

`SessionInfo` exposes negotiated inbound media, outbound senders addressed by
MID, codec/RTX payload mappings, clock rates, profiles, and directional extension
facts. `offered_media` retains every offered section, including application,
inactive, and disabled sections, with its original index and local direction.
Callers enforce admission from those facts, not just accepted RTP lists. The
caller need not parse the answer again or depend on native types.

## Service loop and commits

Feed one `NetworkInput` to `receive(now, arrival, input)`. `now` is execution
time; `arrival` is the actual receive timestamp and must not be later than
`now`. Preserve it when socket processing is delayed. UDP input owns one
datagram. ICE-TCP input owns exactly one complete RFC 4571 frame, including its
length prefix. Malformed network traffic is isolated and counted rather than
made into media events.

Call `poll(now)` for protocol progress, including due timers. Its owned result
is one of:

| Output | Caller obligation |
| --- | --- |
| `Event` | Consume the connection, RTP, Sender Report, keyframe, estimate, encoding, or channel fact. |
| `Transmit` | Submit the already committed bytes promptly, or treat them as local loss. |
| `Idle { next_wakeup }` | Schedule that wakeup; a due-now wakeup means runnable continuation. |
| `Closed` | Observe the terminal reason once; later polls are idle. |

`Event::AuthenticatedPath` reports an ICE-authenticated protocol/local/remote
tuple and the arrival observation of the input that proved it. It does not
identify a socket incarnation or a shard. Preserve adapter input attribution
until consuming the fact; neither the latest input nor `Connected` is a route
proof. Unauthenticated route hints do not produce this event.

Internal drains and event queues are bounded. A caller may collect a bounded
batch, yield to other connections, and resume without draining one connection
to idle. Service finalized output promptly before requesting more finalized
work from that connection. Blocked sends must not create a busy loop; pending
protocol work must not disappear from the next wakeup.

Returning `Transmit` is the irrevocable send commit for continuity, sent history,
bytes in flight, pacing, and byte counters. There is no departure receipt or
rollback API. Unsent UDP datagrams may receive bounded adapter retry or be
counted as local loss. An accepted batch prefix must never be replayed.

`close(now, deadline)` rejects new application work and drains required shutdown
traffic to a bounded deadline. `abort()` discards pending work and publishes one
terminal result. Neither operation promises delivery of queued media.

## RTP values and caller numbering

Authenticated ingress emits `Event::Rtp { mid, rid, packet }`. `RtpPacket`
contains actual arrival, ordinary primary RTP header facts, payload bytes, and
negotiated URI-addressed extension values. RTX is authenticated and decapsulated
into its primary identity before this event. Payload and extension values remain
opaque to frame and playback policy.

Write directly to an outbound MID with `write_rtp(now, mid, RtpWrite)`:

* `sequence` is the caller's extended primary index, below `2^48`;
* `timestamp` and `wallclock: Instant` establish the caller's RTP clock
  correspondence used by Sender Reports;
* marker, payload, and negotiated media extension values are caller supplied;
* negotiated transport SSRC/payload mapping, MID, TWCC, protection, pacing, and
  send-history accounting remain RTC-owned.

Gaps, wrap, and valid late holes are allowed. RTC does not impose a strictly
increasing gate or allocate hidden primary numbers. **The caller must never
reuse an accepted original extended index under the same sender/key**, including
after discarding queued work. There is no unbounded lifetime index registry or
promise to detect caller nonce misuse. Rejected writes return their owned
packet and do not consume an index; retrying that rejected work is allowed.

Admission enforces structural and mechanical bounds, not selected source rates,
frame usefulness, or playback deadlines. The caller owns packetization and
receiver continuity. Oversized packets and invalid/unsupported extension writes
are rejected; a full queue returns `WouldBlock` with the original packet.

`discard_pending_rtp(mid)` discards uncommitted originals. It does not reset
committed primary/RTX identity, sent history, spent pacing service, congestion
state, or retained repair packets. It is not a source-switch, decoder-ready, or
keyframe-recovery operation.

## RTP extension ownership

Extensions are addressed by negotiated semantic URI, not source wire ID. Their
wire IDs can differ between ingress and egress.

RTC generates negotiated transport identity and transport-wide sequence
numbers. Do not submit MID, RID, repaired RID, or TWCC as media extension values.
Caller-owned media values can include audio level, video orientation, Absolute
Capture Time, DD, VLA, and explicit playout delay when negotiated. RTC remaps
and serializes them; it does not resolve DD templates, rewrite frame references,
choose decode targets, or infer keyframe/dependency meaning.

Absolute Capture Time is not authoritative for RTC normalization or congestion
control. Unknown extensions are dropped by default; `UnknownExtensionPolicy`
can permit bounded negotiated opaque values. Absence remains absence. RTC does
not manufacture a default playout-delay extension or keep receiver-confirmed
playout policy. `PlayoutDelay::to_bytes` is only an explicit wire-value encoder,
with rounding to 10 ms units, not an admission, queue, or repair setting.

## Time and Sender Reports

All routine timed operations use `Instant`. The initial `Instant`/`SystemTime`
anchor converts subsequent monotonic progress into RTCP NTP seconds/fraction,
including NTP era wrap. RTC does not read a new wall clock on each packet or
require a coherent cluster-global timestamp/provenance context.

Outgoing Sender Reports relate the advancing committed primary RTP clock to
that fixed protocol anchor. Incoming `SenderReport` exposes actual arrival,
SSRC, raw NTP seconds/fraction, and raw RTP timestamp. The caller owns any
synchronization or normalization derived from those facts. Packet arrival,
execution time, media correspondence, and protocol wall-clock conversion are
not interchangeable.

`Event::ReceiverReport` exposes authenticated SR/RR reception progress for a
negotiated primary sender, associated with its MID and SSRC. The extended highest
sequence is the RFC 3550 32-bit value, not a local write or transmit count.
Progress coalesces to one pending fact per sender and uses serial-number
comparison across 32-bit wrap; stale reports cannot reverse it. The caller owns
mapping it to its extended primary numbering and any playout confirmation.

TWCC and RFC 8888 arrival/report calculations continue to use monotonic time.
Removing the global timeline does not remove wire NTP fields, change their
fractions, or authorize clock-dependent feedback accounting.

## Congestion, repair, and resources

`set_desired_bitrate(Option<MediaPayloadBitrate>)` supplies one aggregate
application-demand hint. `None` and zero are neutral: primary RTP remains
sendable without a hint, while speculative padding is disabled. A positive hint
can enable negotiated RTX startup without a source binding. The native
bandwidth estimate is exposed through `Event::BandwidthEstimate` and connection
statistics, not a reservation or allocation of caller targets.

Desired rate and estimate use RTP media-payload bits/s. Native pacing and the
send window account RTP transport bytes. There is no second payload-admission
pacer, per-source rate policer, or additional delivery-feasibility budget.
SCReAM, TWCC, and RFC 8888 remain supported; only the negotiated mode accounts
acknowledgments. SCTP retains its own congestion control and is excluded from
RTP feedback history.

Padding uses negotiated RTX identity and continuous RTX numbering. Without RTX,
startup and feedback-outage recovery use actual primary media and feedback,
not fabricated primary padding. Repairs use ordinary committed packet retention,
byte/age bounds, and resend spacing. The mechanical retention horizon is not an
estimate of playback usefulness, a frame deadline, or a caller repair policy.

`ConnectionLimits` bounds encodings, channels, message buffering, queued media,
and retained repair bytes. Packet/history/probe queues also have fixed internal
bounds. Authenticated unsignaled SSRC discovery is bounded and counted; paused
encodings are not retired merely because they are inactive. Callers can request
keyframes and explicitly retire an ingress SSRC using direct operations.

## Channels

`open_data_channel(now, config)`, `send_data(now, channel, message)`, and
`close_data_channel(now, channel)` operate directly. An open rejection returns
its configuration; a send rejection returns its message. Count, message, and
buffer bounds apply before consuming application work. Channel open/message/
close facts are events, with ordinary reliability and ordering settings. RTP and
SCTP share bounded service without sharing acknowledgment or capacity accounting.

## TCP adapter responsibilities

The caller owns stream admission, socket incarnations, framing, and retirement.
RTC input/output carries socket tuples, not public flow IDs. Snapshot the chosen
socket incarnation when accepting output for adapter submission. Later output at
the same tuple does not authorize moving saved bytes to a different socket.

A pending or partially written frame must finish on its original socket or cause
that socket to close. Never drop a suffix and continue with a later frame, append
old bytes to a replacement stream, or feed stale closed-socket input into RTC.
The adapter must filter retired incarnations before receive and preserve any
required ordering around replacement. These are socket ownership obligations,
not an RTC registry or retry service.

The native interoperability fixture proves an active client against a passive
RTC candidate at the controlled packet-socket boundary. It translates unframed
native packet-socket payloads to/from complete RFC 4571 frames. Runtime socket
proof separately covers partial byte-stream writes and stale-socket filtering;
packet-socket interoperability is not that byte-stream proof.

## Evidence and limits

Use the owner targets in the [README](../README.md#contributor-verification).
Controlled native peers prove authenticated bidirectional media, fragmented
video, negotiated extension bytes, primary wrap/late-hole feedback receipts,
repair continuity, native TWCC, channels, passive TCP, and same-seed replay.
Protocol unit tests retain SR NTP/RTP correspondence, crypto/index boundaries,
delayed arrival, parser/report/accounting, and negotiated RFC 8888 coverage.

Production routing, DD/keyframe/cache correctness, source/layer switching, and
node behavior remain in the direct-str0m SFU and simulator proof. RTC metadata
assertions alone do not establish those behaviors. Stored SDP, RFC 8888 component
tests, and the controlled native baseline must not be described as new browser,
Pion, platform, or production-cutover compatibility.
