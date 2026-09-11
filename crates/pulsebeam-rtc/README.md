# `pulsebeam-rtc`

`pulsebeam-rtc` is PulseBeam's production WebRTC connection boundary for an
SFU. It encapsulates one peer connection and is designed to replace direct
`str0m` use in `pulsebeam`. Migrating the server is a separate project.

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
  RFC 4571-framed ICE-TCP packets into a connection.
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
parsing and must carry opaque or SFrame-protected media.

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
* Every accepted outbound RTP session, including audio-only RTP, requires one
  supported packet-feedback mode: transport-wide congestion-control feedback or
  RFC 8888. There is no REMB or fixed-rate egress fallback.
* Compatibility means compatibility with the negotiated PulseBeam WebRTC
  profile. Live acceptance evidence covers current pinned Chrome and Firefox
  versions. Stored SDP is parser regression evidence only.

## Packet and stream model

After ingress authentication and decryption, one immutable `Bytes` allocation
is the canonical plaintext packet. Parsing records compact byte ranges and
semantic accessors decode metadata lazily. Local fanout uses shallow clones.
Cross-shard or cross-node transit performs an explicit deep copy so shared
packet reference counts do not become cross-core runtime state.

Negotiated outbound media produce stable `SenderId` values. A sender's identity,
policy, outbound SSRC, RTP sequence and timestamp spaces, RTX state, extension
state, RTCP state, and congestion state survive source and layer changes.

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

* playout-delay range;
* relative media priority;
* desired media-payload bitrate.

The SFU chooses sources and layers. The connection returns governed aggregate
and per-sender allocations and owns transport scheduling.

Egress uses one connection-level SCReAM-v2-derived RTP congestion controller.
The SCReAM core remains independently testable and self-contained. A private
PulseBeam latency governor may only tighten its native queue-delay target; it
cannot increase SCReAM's congestion window, pacing permission, or estimated
capacity.

The latency governor uses sender policy and immutable `global_media_at` to
derive private receiver-specific admission, pacing, shedding, retransmission,
and probing limits. A larger playout range can trade latency headroom for
quality recovery, but it never maps directly to an equivalently large network
queue.

Constrained media capacity is divided among active senders using weighted
max-min allocation. Priority controls relative share; it does not manufacture
capacity or make stale media useful.

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
state, target/pacing rates, transmitted byte classes, drop/feedback counters,
sender allocation and traffic totals, encoding receive/retirement state, and
DataChannel buffering/message totals. The crate has no metrics-framework
dependency and does not expose mutable controller internals.

## Validation boundary

Crate checks, fixtures, deterministic tests, benchmarks, browser sources, and
browser evidence are owned by `crates/pulsebeam-rtc`. The root `just test` gate
also runs workspace consumers and their existing browser regression suite.

Required evidence includes deterministic crate-local tests and simulation for
negotiation, media-clock normalization, source switching, RTP/RTCP continuity,
loss, delay, reordering, VBR, pauses, probing, feedback loss, DataChannel
coexistence, malformed traffic, overload, resource bounds, and timer scaling.

Parameterized many-connection and many-stream benchmarks must preserve one
externally scheduled wakeup per connection and avoid global scans.

Live pinned Chrome and Firefox sessions are required interoperability evidence.
Differential checks against `str0m`, Ericsson SCReAM, or libwebrtc are useful
component evidence but do not replace the documented contract or live-browser
tests.

The binding Linux x86_64 matrix is Chrome/ChromeDriver `153.0.8010.36` and
Firefox ESR `140.15.0esr` with geckodriver `0.36.0`. Exact URLs, lengths, hashes,
and version probes live in `browser/browser-matrix.json`. Normal `cargo test`
runs are offline and never provision or launch a browser. Run the complete local
and CI browser gate with:

```sh
crates/pulsebeam-rtc/scripts/run-browser-matrix.sh --platform linux-x86_64 --include-root-tests
```

The command provisions into `target/pulsebeam-rtc-browsers/linux-x86_64`, runs
the exact ignored Chrome and Firefox RTC matrices, and then invokes the literal
root `just test` with the same Chrome binary. Browser/driver logs, offer inputs,
and compact JSON scenario reports are written under
`target/pulsebeam-rtc-browser-artifacts`. Other platforms, ICE restart, and
renegotiation are not part of the accepted v3 profile. Unsupported browser
profile differences are recorded in each matrix report rather than skipped.

Detailed public types, ownership decisions, alternatives considered, and their
rationale are specified in [docs/design.md](docs/design.md). Detailed
congestion-control behavior and rationale are specified in
[docs/congestion-control.md](docs/congestion-control.md).
