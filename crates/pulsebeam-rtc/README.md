# `pulsebeam-rtc`

An experimental, sans-I/O WebRTC transport engine for transport contributors.
The production PulseBeam SFU still uses direct str0m. Integrating this crate into
that server is a separate, unimplemented cutover.

For a first development check, run the owner suite from the workspace root:

```sh
./bazel test //crates/pulsebeam-rtc:test --test_output=errors
```

See the [connection contract](docs/design.md) for integration obligations and
[congestion control](docs/congestion-control.md) for the private SCReAM profile.
The public Rust definitions in `src/lib.rs`, `src/rtp.rs`, `src/io.rs`, and
`src/session.rs` are the API reference.

## Ownership

RTC owns negotiation, ICE, DTLS, SRTP/SRTCP, RTP/RTCP, SCTP, packet feedback,
congestion control, transport pacing, ordinary repair, timers, and mechanical
resource limits. Each connection is `Send`, not `Sync`, with explicit protocol
progress and owned outputs.

The caller owns sockets and scheduling, routing and bandwidth allocation,
source/layer selection, receiver sequence/timestamp translation, DD/frame
interpretation and rewriting, keyframe caches, source switching, playback
policy, global media normalization, and node serialization. RTC has no source
assignment, frame metadata, media-age admission, or global media timeline.

## Direct connection operations

Accept an initial offer using local candidates, a standard clock anchor, and
fresh cryptographic entropy supplied by the caller. This helper illustrates the
workspace API; it does not create sockets or obtain entropy:

```rust
use std::{net::SocketAddr, time::{Instant, SystemTime}};
use pulsebeam_rtc::{
    AcceptError, AcceptedConnection, Connection, ConnectionConfig,
    ConnectionEntropy, LocalCandidate, SdpOffer,
};

fn accept_peer(
    local: SocketAddr,
    offer: String,
    now: Instant,
    wallclock: SystemTime,
    fresh_entropy: [u8; 32],
) -> Result<AcceptedConnection, AcceptError> {
    Connection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(local)],
            ..ConnectionConfig::default()
        },
        SdpOffer::new(offer),
        now,
        wallclock,
        ConnectionEntropy::new(fresh_entropy),
    )
}
```

Send the returned SDP answer through the application's signaling channel.
Then use the connection directly:

* `receive(now, arrival, input)` accepts a UDP datagram or one complete RFC 4571
  ICE-TCP frame. Preserve actual arrival separately from delayed processing.
* `poll(now)` services protocol work and due timers, returning an event,
  committed transmit, next wakeup, or terminal close. Honor an immediate wakeup
  as runnable work. A caller can yield after a bounded output batch.
* `write_rtp(now, mid, packet)` writes to a negotiated outbound media section.
  `RtpWrite` supplies the extended primary sequence, timestamp, its associated
  monotonic `wallclock`, marker, payload, and negotiated media extension values.
* `open_data_channel`, `send_data`, and `close_data_channel` operate directly on
  channels. Backpressure returns caller-owned work.
* `set_desired_bitrate` sets one optional aggregate demand hint. `stats` and
  `Event::BandwidthEstimate` expose the native media-payload rate estimate.
* `discard_pending_rtp` discards uncommitted originals for a stream without
  resetting transport history or retained repairs. `close` performs bounded
  graceful shutdown; `abort` discards pending work and closes immediately.

## Numbering, clocks, and socket handoff

The caller owns primary RTP numbering. Gaps, wrap, and valid late holes are
allowed. **Never reuse an accepted original extended index under the same
sender/key**, including after discarding pending work. A rejected write retains
its packet and does not consume an index. RTX, TWCC, and RTCP numbering remain
RTC-owned; padding uses negotiated RTX, never invented primary indices.

Routine timed calls take `Instant`. The initial `Instant`/`SystemTime` anchor is
used only for protocol wall-clock conversion. Incoming Sender Report NTP
seconds/fraction and RTP timestamps remain wire facts. Normalization and
synchronization belong to the caller, not an RTC clock service.

Returning `Transmit` is an irrevocable send commit. Service a bounded batch
promptly, or account unsent output as local loss. Never replay an accepted UDP
batch prefix. Pending TCP bytes belong to the original socket incarnation:
finish a partially written frame there or close that socket. Do not redirect a
saved frame or its suffix to a replacement stream. The adapter owns incarnation
tracking and stale-socket filtering; RTC exposes socket tuples, not a flow
admission/retirement API.

## Negotiation and extensions

The supported profile is an immutable ICE-lite initial offer/answer with BUNDLE,
RTCP mux, UDP or passive ICE-TCP, negotiated Opus/H.264, and the existing DTLS and
SRTP profiles. Outbound RTP requires one packet-feedback mode, TWCC or RFC 8888.
There is no REMB/fixed-rate fallback, renegotiation, ICE restart, or TURN client.

RTP values own ordinary headers, payloads, and URI-addressed extensions. RTC
regenerates transport identity and TWCC. The caller supplies media extension
values, including DD, VLA, and explicit playout-delay bytes, when negotiated.
RTC remaps their IDs without interpreting frame/dependency or playback meaning.
Unknown extensions are dropped by default and can be negotiated as bounded
opaque values. See [extension ownership](docs/design.md#rtp-extension-ownership).

## Contributor verification

The RTC owner suite includes controlled native libwebrtc peers, public boundary
tests, and private protocol/congestion proofs. Its Linux x86_64 client baseline
is `pulsebeam-webrtc-sys` `0.6.2`, revision
`f87f99a48e58fb9cd13767ba58e9b7cd937c71a3`. Virtual time and packet delivery drive
real protocol implementations without real sockets or sleeps. Stored SDP is
parser evidence, not browser compatibility. RFC 8888 retains parser, report,
accounting, and ECN coverage, not a native-client interoperability claim.

```sh
./bazel test //crates/pulsebeam-rtc:test --nocache_test_results
./bazel test //crates/pulsebeam-rtc:format //crates/pulsebeam-rtc:clippy
```

The warm uncached owner suite has a fifteen-second operational host-time target,
not a correctness threshold. Quantitative modeled-network assertions remain
correctness gates. `src/network_tests.rs` and connection unit tests offer direct
RTP to real native receivers for capacity, competition, queue-sojourn,
application-limited, and feedback-outage proofs. Native ingress, fragmented
video, repair, channel, TCP, and same-seed replay tests cover separate wire
obligations. Source/frame/cache behavior remains in the native SFU's owner
proof, not RTC fixtures.

The existing active-media Criterion matrix remains separate from correctness
acceptance:

```sh
CRITERION_HOME="$PWD/target/criterion" ./bazel run -c opt //:benchmark
```

It measures native capture, direct writes/fanout, transport pumping, authenticated
feedback, queue statistics, and a 500 ms virtual drain for 1/8/32 senders and
burst depths of 1/8. Peer construction and teardown are outside timing; the
bottleneck is 500 kbit/s. It is not production-server throughput or isolated
queue-operation cost.

Follow the [workspace development workflow](../../README.md#development) and
[testing contract](../../docs/build.md#testing-contract). Complete composed
acceptance is `./bazel test //:test`; this crate's suite alone does not establish
production integration or additional browser/platform compatibility.
