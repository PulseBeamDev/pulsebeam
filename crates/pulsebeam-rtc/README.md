# `pulsebeam-rtc`

`pulsebeam-rtc` provides a sans-I/O WebRTC connection boundary for an SFU.
It encapsulates a peer connection behind one native transport and media facade;
the caller retains topology, routing, and source-selection policy.

The production `pulsebeam` server still uses its existing transport engine.
Integrating this facade into that server and its runtime is a separate change.

The public and architectural contract is defined in
[docs/design.md](docs/design.md). The congestion-control implementation contract
is defined in
[docs/congestion-control.md](docs/congestion-control.md).

## Ownership boundary

* One `Connection` owns one peer's negotiation, ICE, DTLS, SRTP/SRTCP, SCTP,
  RTP, RTCP, inbound media-clock normalization, outbound RTP continuity, timers,
  scheduler, pacer, probing, and congestion control.
* The caller owns sockets, all connections, shard scheduling, cross-connection
  routing, and cluster clock synchronization. It feeds individual UDP packets or
  RFC 4571-framed ICE-TCP packets into a connection. Accepted TCP streams are
  explicitly admitted before input and retired on close. Flow IDs identify
  non-reused stream incarnations, not peer socket addresses.
* The SFU owns source and layer selection, keyframe caches, and semantic media
  knowledge needed for routing. Once media is submitted to an outbound sender,
  the connection owns its transport feasibility and wire representation.
* A connection has no threads, callbacks, hidden clock reads, shared mutable
  state, global scans, or cross-connection coordination. It is `Send`, not
  `Sync`.
* Internal state is dense, bounded, and connection-local.
* `str0m` supplies low-level ICE, DTLS, and SRTP components. `dcsctp` supplies
  low-level SCTP. Their types and policy do not cross the public API.

## Connection model

The public surface is one `Connection` facade plus configuration, stable typed
IDs, immutable media values, commands, events, transmits, and coherent
statistics.

Conceptually:

```text
Connection::accept(...)
connection.receive(...)
connection.command(...)
connection.poll(...)
connection.stats()
```

All protocol progress occurs through explicit calls. The caller drains `poll`
until it returns:

```text
Idle { next_wakeup }
```

`next_wakeup` is the connection's single externally scheduled timer.

Returning `Transmit` is an irrevocable send commit. RTP/RTCP continuity,
congestion-control sent history, bytes in flight, pacing, and forwarding
accounting have already advanced. The caller must immediately submit the
transmit to its selected UDP socket or ICE-TCP stream before driving the
connection again. There is no departure receipt or rollback API.

A connection supports graceful close, which rejects new application work and
drains required protocol shutdown traffic to a deadline, and immediate abort.

## Time and media model

PulseBeam deliberately separates local execution time from globally comparable
media time.

* `Instant` drives local timers, RTT, congestion control, pacing,
  retransmission, and `next_wakeup`.
* `GlobalMediaTime` is a serializable cluster-global microsecond time domain
  supplied from the runtime's NTP/PTP-disciplined clock.
* The runtime supplies coherent local/global `TimePoint` values; the connection
  never reads or disciplines clocks itself.
* First-ingress RTP is normalized into `GlobalMediaTime` using the server clock,
  RTP clock progression, and RTCP Sender Report relationships. Endpoint
  wall-clock epochs are not authoritative.
* Every media packet carries an immutable `global_media_at`. Routing across
  shards or nodes preserves it exactly.
* `global_media_at` is comparable across media kinds, participants, connections,
  and nodes in the same PulseBeam clock domain. It is normalized server media
  time, not a claim of exact physical capture time.

The SFU supplies frame semantics it already knows, including frame identity,
boundaries, random-access state, and dependencies. `pulsebeam-rtc` uses those
facts for admission, deadline enforcement, shedding, RTX usefulness, and
outbound dependency continuity. Core forwarding does not require codec-payload
parsing and must carry opaque or SFrame-protected media. Within a fragmented
frame, native admission checks canonical source RTP continuity. A missing middle
packet interrupts the frame rather than emitting its End marker on contiguous
output. The caller can discard unprepared work and resume at fresh independent
media without rewinding committed transport identity.

## Session and interoperability

* The session is one immutable ICE-lite remote-offer/local-answer exchange.
* BUNDLE, RTCP mux, inline candidates, all SDP media directions, UDP, and
  passive ICE-TCP over IPv4 and IPv6 are supported.
* Trickle ICE, ICE restart, media renegotiation, and a TURN client are outside
  this boundary. A changed session creates a new connection. Client relay
  candidates remain usable.
* A fresh ephemeral DTLS identity is generated from caller-supplied
  cryptographic entropy for each connection. Its private key is neither exported
  nor reused.
* DTLS 1.3 is preferred and negotiates `TLS_AES_128_GCM_SHA256` with capable
  peers; DTLS 1.2 remains an in-handshake compatibility fallback. AEAD-GCM SRTP
  profiles are preferred, with `SRTP_AEAD_AES_128_GCM` as the modern baseline.
  `SRTP_AES128_CM_HMAC_SHA1_80` remains supported for compatibility and is never
  preferred when a mutually supported AEAD-GCM profile is available.
* Codecs and RTP header extensions are negotiated from SFU configuration.
  Inbound RTP payload acceptance is independent of outbound sender payload types,
  including mixed publish/subscribe sessions.
* Every accepted outbound RTP session, including audio-only RTP, requires one
  supported packet-feedback mode: transport-wide congestion-control feedback or
  RFC 8888. There is no REMB or fixed-rate egress fallback.
* Compatibility means compatibility with the negotiated PulseBeam WebRTC
  profile. The sole client compatibility baseline is pinned libwebrtc
  `pulsebeam-webrtc-sys` `0.6.2` in controlled execution. Stored SDP is generic
  parser regression evidence only, not browser compatibility evidence.

`SessionInfo` exposes typed `CodecInfo` values for every accepted inbound primary
payload and each selected outbound codec. They include RTP/RTX payload mapping,
clock rate, channel count and the negotiated Opus or H.264 profile, so callers
can interpret source media without reparsing SDP or depending on str0m types.

## Packet and stream model

After ingress authentication and decryption, one immutable `Bytes` allocation
is the canonical plaintext packet. Parsing records compact byte ranges and
semantic accessors decode metadata lazily. Local fanout uses shallow clones.
Cross-shard or cross-node transit performs an explicit deep copy so shared
packet reference counts do not become cross-core runtime state.

Negotiated outbound media produce stable `SenderId` values. A sender's identity,
policy, outbound SSRC, RTP sequence and timestamp spaces, extension state, RTCP
state, and congestion state survive source and layer changes. The SFU can discard
unprepared queued media and retained RTX payloads for a replaced assignment with
`DiscardSenderMedia`, without rewinding committed transport continuity. Native
audio senders mark their first committed packet and the first committed packet
after discard as a talk-spurt start, preserving any source marker without changing
the canonical source packet.

Unsignaled authenticated ingress SSRCs or RIDs can produce stable encoding
identities within a configured connection-wide bound. Overflow is dropped and
counted without closing the connection. Inactivity alone does not retire a
paused encoding.

Source packets retain their canonical `global_media_at`. An outbound sender maps
that global media timeline into its own continuous RTP clock, so source
switching does not depend on source RTP timestamps or SSRCs and does not rewind
the receiver's RTP timeline.

Forwarding is packet-level and cut-through. Frame metadata allows admission and
shedding to prefer complete not-yet-started frames, but a started frame is not
an unbounded commitment when congestion or deadline safety requires dropping
remaining packets.

## RTP extension policy

Extension policy is immutable per negotiated media sender and keyed by semantic
URI, never source wire ID.

* Connection-managed extensions such as MID, RID, repaired RID, transport-wide
  sequence numbers, absolute send time, playout delay, and dependency
  descriptors requiring outbound continuity are generated or rewritten.
* Known endpoint-independent values such as audio level and video orientation
  can be forwarded after URI-based remapping.
* Absolute Capture Time may be forwarded or exposed diagnostically but is not
  authoritative for `GlobalMediaTime`, A/V synchronization, deadlines, or
  congestion control.
* Unknown extensions are dropped by default and may be explicitly permitted as
  validated opaque pass-through.
* Authenticated ingress extensions remain available through lazy URI-based
  accessors even when they are not forwarded.

## Scheduling, latency, and congestion control

Protocol control remains deliverable under load and padding remains the
lowest-value traffic.

Every outbound sender has mutable SFU-provided semantic policy:

* browser-managed default playout or an explicit playout-delay range;
* desired media-payload bitrate.

The SFU chooses sources, layers, and sender targets. The connection reports its
native aggregate RTP payload budget through `Event::MediaBudgetChanged` and
`ConnectionStats.media_budget`. This is not measured spare path bandwidth, a
reservation, or a redistribution of caller targets.

Egress uses one connection-level SCReAM-v2-derived RTP congestion controller.
The SCReAM core remains independently testable and self-contained. PulseBeam
latency policy governs admission, pacing horizons, retransmission, and
probing outside that core; it does not override SCReAM's native queue-delay target,
congestion window, pacing equations, or estimated capacity.

The latency governor uses sender policy and immutable `global_media_at` to
derive private receiver-specific admission, pacing, shedding, retransmission,
and probing limits. A larger playout range can trade latency headroom for
quality recovery, but it never maps directly to an equivalently large network
queue.

Each sender's caller-selected payload rate bounds local admission. One aggregate
media-payload pacer enforces the native budget at transmit commit, independently
of the native transport-byte burst pacer and send window. Over-capacity caller
targets do not override these limits. Eligible original media uses equal byte
quanta across senders, without application priorities or target-weighted shares.

Pre-media and application-limited probing uses ordinary RTP padding on a
negotiated media or RTX SSRC. There is no synthetic SSRC-zero probe stream.

SCTP retains its own congestion controller and acknowledgment state.
DataChannel bytes never enter RTP packet-feedback history or SCReAM bytes in
flight. RTP and SCTP service are coordinated by the connection's bounded
top-level scheduler.

The complete algorithm, fixed version-one profiles, hard bounds, alternatives
considered, and validation requirements are in
[docs/congestion-control.md](docs/congestion-control.md).

## DataChannels and resource safety

DataChannels support local and remote DCEP opening, externally negotiated
channels, ordered and unordered delivery, reliability by retransmit count or
lifetime, text and binary message boundaries, priority, buffered-amount
backpressure, and graceful close. No `dcsctp` type is public.

Configuration explicitly bounds dynamic encodings, channel count, message
sizes, buffered DataChannel data, media queues, retransmission history, protocol
state, feedback history, and per-poll work.

Outbound pressure returns a typed `WouldBlock` before exceeding configured
bounds. Authenticated violations are isolated to their stream or channel where
protocol semantics permit. Malformed or unauthenticated network input is
normally dropped and counted rather than surfaced as an application error.

Only unrecoverable authenticated transport state, mandatory resource exhaustion
required for correctness, cryptographic failure, or timeout is terminal.

Statistics are coherent snapshots with connection, sender, encoding, and
DataChannel counters. They expose negotiated feedback, queue and in-flight
state, native aggregate media budget and pacing rate, transmitted byte classes,
drop/feedback counters, sender demand and traffic totals, encoding receive/retirement state, and
DataChannel buffering/message totals. The crate has no metrics-framework
dependency and does not expose mutable controller internals.

## Validation boundary

Crate checks, deterministic tests, benchmarks, and the test-only native
simulation harness are owned by `crates/pulsebeam-rtc`. Private controller
assertions, including the fixed SCReAM scenario matrix, run in the unit target;
integration tests exercise the public `Connection` boundary. Controlled native
peers establish the distinct interoperability obligations described below.
See the [shared development workflow](../../README.md#development) for workspace
gates. Web and React retain consumer browser coverage; their shared browser
provisioning is owned by `tools/browser-matrix.json`.

Required evidence includes deterministic crate-local tests and simulation for
negotiation, media-clock normalization, source switching, RTP/RTCP continuity,
loss, delay, reordering, VBR, pauses, probing, feedback loss, DataChannel
coexistence, malformed traffic, overload, resource bounds, and timer scaling.

Parameterized many-connection and many-stream benchmarks preserve one
externally scheduled wakeup per connection and avoid global scans. Run the
Criterion scaling matrix separately from test gates:

```sh
./bazel run //:benchmark
```

The matrix covers 1/16/64 connections with 1/8/32 senders each. Criterion measures
wall-clock execution of 30 simulated seconds of polling after attempting one
initial packet per sender, allowing admission backpressure. Connection negotiation,
media admission, and teardown are excluded from timing.
Transmits are discarded, so this measures timer/poll scheduling, not sustained
network throughput or a simulated bandwidth/RTT path. Reports and comparison
baselines live under `target/criterion`. Criterion arguments can be passed to the
entrypoint, for example `./bazel run //:benchmark -- --save-baseline main`.

Run the complete RTC unit, property, integration, and native interoperability
suite, with cached test results disabled:

```sh
./bazel test --config=sim //crates/pulsebeam-rtc:test --nocache_test_results
```

The Linux x86_64 harness uses `pulsebeam-webrtc-sys` `0.6.2`, commit
`f87f99a48e58fb9cd13767ba58e9b7cd937c71a3`. Bazel provisions matching immutable
Rust sources and released native artifacts before compilation. Native actions
do not download artifacts. The isolated `simulation/Cargo.toml` keeps upstream
build-dependency pins out of the production Cargo workspace; Bazel explicitly
supplies this test-only dependency to RTC fixtures.

Every native peer uses a process-exclusive controlled world, explicit simulated
time, task pumping, and virtual UDP or active-client TCP packet delivery. Multiple peers in one scenario
share the world. The unit target uses one Rust test thread to avoid parallel
harness threads blocking on the native-world lease. Separate test binaries may
run concurrently. No real sockets, hardware media, production scheduling,
sleeps, or wall-clock protocol deadlines are used. The warm, uncached complete RTC
suite has an operational host elapsed-time target of fifteen seconds, including
peer setup and teardown. This execution target is not a correctness acceptance
threshold. Virtual-time protocol deadlines, resource bounds, and quantitative
modeled-network/controller assertions remain correctness requirements. Use the
Criterion matrix above for host-performance comparisons, within its polling-only
measurement boundary.

High-volume egress traces in `src/network_tests.rs` and `src/connection.rs`
construct canonical SFU media inputs through the private `src/test_source.rs`
component source. Their receivers and packet feedback still use actual controlled
libwebrtc peers. These traces establish RTC admission, scheduling, and
congestion invariants, not production-server integration or native ingress
interoperability. Separate native media
and media-clock tests retain authenticated source delivery and normalization.

Actual authenticated bidirectional media, client-generated TWCC accounting,
DataChannels, and same-seed replay are interoperability evidence. The ICE-TCP
fixture connects an active native client to RTC's passive candidate and verifies
authenticated media, DataChannels, flow retirement, and graceful close. The pinned
adapter has no TCP listener and exposes unframed non-TLS packet-socket payloads;
the fixture translates those payloads to and from RTC's complete RFC 4571 frames.
This establishes native active-client ICE-TCP interoperability at the packet-socket
boundary, not native TCP byte-stream fragmentation or passive-client coverage.
RFC 8888 retains client-free negotiation, report generation, parsing, and
accounting coverage;
RFC 8888 client interoperability is not claimed. Neither browser nor Pion compatibility
is inferred from the native replacement. Missing artifacts or required native
evidence fail the gate.

Other platforms, ICE restart, and renegotiation remain outside this acceptance
profile. Criterion still measures only RTC polling; native setup and teardown
are outside its timed region.

Run root `./bazel test //:test` for complete workspace acceptance, including the
consumer-owned Web and React browser coverage.

Detailed public types, ownership decisions, alternatives considered, and their
rationale are specified in [docs/design.md](docs/design.md). Detailed
congestion-control behavior and rationale are specified in
[docs/congestion-control.md](docs/congestion-control.md).
